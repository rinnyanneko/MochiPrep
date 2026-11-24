//! A position that changes while the agent waits runs the agent at once, instead of at its next
//! interval: a stop loss, take profit or trailing stop filling, a resting entry filling, or a
//! trade by the owner. The agent then decides with the position as it is now.
//!
//! Each running agent keeps what it last saw of each pair's position. Its own orders move that
//! mark with them, so they never wake it: a paper order fills on the task, and an OKX order is
//! only seen on the next sync, so its pair is left out until a sync that started after it.
//! Another change to that pair in that window, up to one sync, counts as the agent's own unless
//! a filled order left the position as it was; the next scheduled run still sees it.
use std::collections::{HashMap, HashSet};

use mochi_core::paper::PaperStore;
use mochi_domain::{AccountVenue, OrderStatus, Position};
use mochi_store::rows;

use super::agent_execution::Acted;
use super::execution::ExecutionTarget;
use super::Runtime;

/// An agent and one of its pairs.
type Leg = (String, String);

/// An OKX order of the agent's own that moves a position, until a sync shows it.
struct Settling {
    account_id: String,
    /// The sync pass under way when OKX answered; that pass may have read OKX before the order
    /// landed. `None` until OKX answers, and no sync settles it meanwhile.
    pass: Option<u64>,
    /// OKX said the order filled, so the sync must show a different position. The same one means
    /// something else closed it again in between, such as its stop.
    filled: bool,
}

#[derive(Default)]
pub(super) struct PositionWatch {
    /// What each agent last saw of each pair's position, as [`mark`] writes it.
    seen: HashMap<Leg, String>,
    settling: HashMap<Leg, Settling>,
    /// Pairs a sync found changed behind the agent's own order, to wake it on the next tick.
    woken: HashSet<Leg>,
    /// Counts sync passes, so a pass can tell whether it started after an order.
    pass: u64,
}

impl PositionWatch {
    /// A new sync pass starts; it reads OKX after every order answered so far.
    pub(super) fn next_pass(&mut self) -> u64 {
        self.pass += 1;
        self.pass
    }
}

/// The positions on one pair, in a form that changes whenever they do: a new position, a closed
/// one, a flip, or a fill that changes the size. Empty when flat.
fn mark(positions: &[Position], symbol: &str) -> String {
    let mut parts: Vec<String> = positions
        .iter()
        .filter(|p| p.symbol == symbol)
        .map(|p| format!("{} {:?} {}", p.id, p.side, p.quantity))
        .collect();
    parts.sort();
    parts.join(";")
}

fn pairs(a: &rows::Agent) -> Vec<String> {
    if a.symbols.is_empty() {
        vec![a.symbol.clone()]
    } else {
        a.symbols.clone()
    }
}

/// What OKX's answer says about the position: `None` when the order surely moved nothing,
/// otherwise whether it filled. An error may be a lost answer, which can still have filled.
fn answered(acted: &Acted) -> Option<bool> {
    match acted {
        Acted::Placed(Ok(result)) => match result.order.status {
            OrderStatus::Filled => Some(true),
            OrderStatus::Open => Some(false),
            OrderStatus::Canceled | OrderStatus::Rejected => None,
        },
        Acted::Placed(Err(_)) => Some(false),
        Acted::Protected(_) => None,
    }
}

impl Runtime {
    /// The account's positions as last known: paper from the store, OKX from the last sync.
    /// `None` for an OKX account with no session, which has nothing to compare.
    fn watched_positions(&self, account_id: &str) -> Option<Vec<Position>> {
        if let Some(session) = self.exchange_session(account_id) {
            return Some(
                session
                    .engine
                    .positions()
                    .into_iter()
                    .map(|view| view.position)
                    .collect(),
            );
        }
        let account = self.engine.store().get_account(account_id).ok()??;
        if account.venue != AccountVenue::Paper {
            return None;
        }
        self.engine.store().list_open_positions(account_id).ok()
    }

    /// Whether a position on one of a running agent's pairs changed since the agent last saw
    /// it. Each change is reported once. Pairs seen for the first time are only recorded.
    /// Callers ask only when they can start the cycle, so a reported change is never lost.
    pub(super) fn positions_changed(&mut self, a: &rows::Agent) -> bool {
        let Some(positions) = self.watched_positions(&a.account_id) else {
            return false;
        };
        let watch = &mut self.agents.watch;
        let mut changed = false;
        for symbol in pairs(a) {
            let leg = (a.id.clone(), symbol);
            changed |= watch.woken.remove(&leg);
            if watch.settling.contains_key(&leg) {
                continue;
            }
            let now = mark(&positions, &leg.1);
            match watch.seen.insert(leg, now.clone()) {
                Some(before) if before != now => changed = true,
                _ => {}
            }
        }
        changed
    }

    /// Pairs the agent no longer trades: if one is added back, it is new to the agent again.
    pub(super) fn forget_pairs(&mut self, agent_id: &str, symbols: &[String]) {
        let watch = &mut self.agents.watch;
        let gone = |(id, symbol): &Leg| id == agent_id && symbols.contains(symbol);
        watch.seen.retain(|leg, _| !gone(leg));
        watch.settling.retain(|leg, _| !gone(leg));
        watch.woken.retain(|leg| !gone(leg));
    }

    /// A stopped agent watches nothing; it starts again from what it sees then.
    pub(super) fn forget_positions(&mut self, agent_id: &str) {
        let watch = &mut self.agents.watch;
        watch.seen.retain(|(id, _), _| id != agent_id);
        watch.settling.retain(|(id, _), _| id != agent_id);
        watch.woken.retain(|(id, _)| id != agent_id);
    }

    /// The position a cycle just read for the model: changes after this wake the agent again.
    /// The actor reads it in the same step as the model's copy, so both are the same version.
    pub(super) fn saw_position(&mut self, a: &rows::Agent) {
        let leg = (a.id.clone(), a.symbol.clone());
        if self.agents.watch.settling.contains_key(&leg) {
            return;
        }
        if let Some(positions) = self.watched_positions(&a.account_id) {
            let now = mark(&positions, &a.symbol);
            self.agents.watch.seen.insert(leg, now);
        }
    }

    /// The pair's position before the agent's own order, to carry its mark across that order.
    pub(super) fn before_own_order(&self, a: &rows::Agent, target: &ExecutionTarget) -> String {
        match target {
            ExecutionTarget::Paper => self
                .watched_positions(&a.account_id)
                .map(|p| mark(&p, &a.symbol))
                .unwrap_or_default(),
            ExecutionTarget::Okx(_) => String::new(),
        }
    }

    /// The agent's own order moved its position: that is no change to wake it for. A mark that
    /// already differed before the order still wakes it, since something else moved it first.
    /// `moves` is false for a protection change, which leaves the position as it is.
    pub(super) fn after_own_order(
        &mut self,
        a: &rows::Agent,
        target: &ExecutionTarget,
        before: String,
        moves: bool,
    ) {
        let leg = (a.id.clone(), a.symbol.clone());
        match target {
            ExecutionTarget::Paper => {
                let after = self
                    .watched_positions(&a.account_id)
                    .map(|p| mark(&p, &a.symbol))
                    .unwrap_or_default();
                let watch = &mut self.agents.watch;
                if watch.seen.get(&leg).is_none_or(|seen| *seen == before) {
                    watch.seen.insert(leg, after);
                }
            }
            ExecutionTarget::Okx(_) if moves => {
                let settling = Settling {
                    account_id: a.account_id.clone(),
                    pass: None,
                    filled: false,
                };
                self.agents.watch.settling.insert(leg, settling);
            }
            ExecutionTarget::Okx(_) => {}
        }
    }

    /// OKX answered the agent's own order. A refused one moved nothing, so its pair is watched
    /// again at once; otherwise a sync pass that starts after this one shows it.
    pub(super) fn own_order_answered(&mut self, agent_id: &str, symbol: &str, acted: &Acted) {
        let watch = &mut self.agents.watch;
        let leg = (agent_id.to_string(), symbol.to_string());
        let pass = watch.pass;
        match answered(acted) {
            None => {
                watch.settling.remove(&leg);
            }
            Some(filled) => {
                if let Some(settling) = watch.settling.get_mut(&leg) {
                    settling.pass = Some(pass);
                    settling.filled = filled;
                }
            }
        }
    }

    /// A sync of the account succeeded. Orders answered before this pass started show in it, so
    /// their pairs are watched again from here.
    pub(super) fn settle_synced(&mut self, account_id: &str, pass: u64) {
        let done: Vec<Leg> = self
            .agents
            .watch
            .settling
            .iter()
            .filter(|(_, s)| s.account_id == account_id && s.pass.is_some_and(|p| p < pass))
            .map(|(leg, _)| leg.clone())
            .collect();
        if done.is_empty() {
            return;
        }
        let Some(positions) = self.watched_positions(account_id) else {
            return;
        };
        let watch = &mut self.agents.watch;
        for leg in done {
            let Some(settling) = watch.settling.remove(&leg) else {
                continue;
            };
            let now = mark(&positions, &leg.1);
            if settling.filled && watch.seen.get(&leg) == Some(&now) {
                watch.woken.insert(leg.clone());
            }
            watch.seen.insert(leg, now);
        }
    }
}
