//! OKX's closed-position history, kept locally, and what the open positions have made so far. The
//! arena reads both to tell what an agent's pair made without counting deposits and transfers,
//! which move the account's equity just as much.

use std::collections::HashMap;

use mochi_domain::{ExchangePositionRecord, OrderSide};

use super::sync::js_number;
use super::{dec, is_leg, EngineError, OkxEngine, Registry};
use crate::client::{
    nullish_text, truthy_text, Row, EXECUTION_HISTORY_PAGE, POSITIONS_HISTORY_PAGE,
};
use crate::ids::to_inst_id;

/// How often the history is read.
const HISTORY_EVERY_MS: i64 = 60_000;
/// Pages read per pass. A longer history is read over several passes.
const HISTORY_PAGES: usize = 10;
const EXECUTION_PAGES: usize = 3;

/// An open position's result so far, as OKX reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPositionResult {
    pub symbol: String,
    pub pos_id: String,
    /// OKX's `cTime`; `None` when OKX gave none, and then no agent can be said to own it.
    pub opened_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub opening_side: Option<String>,
    /// OKX's `realizedPnl` on the open position: fees, funding and partial closes so far.
    pub realized_pnl: String,
    pub unrealized_pnl: String,
}

/// Clears the in-flight flag however a pass ends.
struct Running<'a>(&'a OkxEngine);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.state().history_running = false;
    }
}

impl OkxEngine {
    /// One pass, at most once a minute and never two at a time. It takes no part in the order
    /// queue, so it never holds up an order or an exit; the sync loop runs it as a task of its own
    /// (see [`OkxEngine::spawn_position_history`]) so it never holds up a sync either.
    ///
    /// The pass reads from the newest close back until it meets what is recorded, then carries on
    /// with any older history still unread, within a page budget. It writes everything, records
    /// and the backfill cursor, in one transaction at the end: a pass that fails part-way writes
    /// nothing and the next one starts over.
    pub async fn sync_position_history(&self) -> Result<(), EngineError> {
        let now = self.now();
        {
            let mut state = self.state();
            if state.history_running || now - state.last_history_at < HISTORY_EVERY_MS {
                return Ok(());
            }
            state.history_running = true;
        }
        let _running = Running(self);
        let account_id = self.deps.account_id.clone();
        let (newest, mut backfill) = {
            let account_id = account_id.clone();
            self.rows(move |rows| {
                Ok((
                    rows.latest_position_record_at(&account_id)?,
                    rows.position_history_backfill(&account_id)?,
                ))
            })
            .await?
        };
        let mut records = Vec::new();
        let mut budget = HISTORY_PAGES;

        // Newest first, down to what is recorded.
        let read = self
            .read_back(None, newest, &mut budget, &mut records, &account_id)
            .await?;
        match read {
            Read::End => backfill = None,
            Read::Met => {}
            // Out of budget before meeting the records: the gap is read as backfill.
            Read::Stopped(oldest) => {
                backfill = Some(backfill.map_or(oldest, |before| before.max(oldest)));
            }
        }
        // Then older history still unread.
        if let Some(before) = backfill {
            if budget > 0 {
                backfill = match self
                    .read_back(Some(before), None, &mut budget, &mut records, &account_id)
                    .await?
                {
                    Read::End | Read::Met => None,
                    Read::Stopped(oldest) => Some(oldest),
                };
            }
        }

        let account_for_records = account_id.clone();
        self.rows(move |rows| {
            rows.transaction(&mut || {
                for record in &records {
                    rows.upsert_position_record(record)?;
                }
                rows.set_position_history_backfill(&account_for_records, backfill)?;
                Ok(())
            })
        })
        .await?;
        let execution_result = self.read_execution_history(&account_id).await;
        self.state().last_history_at = now;
        execution_result
    }

    /// In each pass, continue the active scan until its old watermark or the end of retention.
    /// The cursor commits with each page so an interruption cannot create a gap or repeat a trade.
    async fn read_execution_history(&self, account_id: &str) -> Result<(), EngineError> {
        let account = account_id.to_string();
        let mut cursor = self
            .rows(move |rows| Ok(rows.execution_cursor(&account)?))
            .await?;
        let completed = cursor
            .completed_bill
            .as_deref()
            .and_then(|id| id.parse::<u64>().ok());
        for _ in 0..EXECUTION_PAGES {
            let after = cursor
                .scan_after
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok());
            let page = self.deps.client.get_execution_history(after).await?;
            let bills = page
                .iter()
                .map(|row| {
                    row.get("billId")
                        .and_then(|id| id.as_str())
                        .and_then(|id| id.parse::<u64>().ok())
                        .ok_or_else(|| {
                            EngineError::message("OKX execution history is missing a bill ID")
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let oldest = bills.iter().copied().min();
            if let Some(newest) = bills.iter().copied().max() {
                if after.is_some_and(|after| newest >= after) {
                    return Err(EngineError::message(
                        "OKX execution history pagination did not advance",
                    ));
                }
                cursor.scan_head.get_or_insert_with(|| newest.to_string());
            }
            let complete = page.len() < EXECUTION_HISTORY_PAGE
                || completed
                    .zip(oldest)
                    .is_some_and(|(completed, oldest)| oldest <= completed);
            if complete {
                if let Some(newest) = cursor.scan_head.take() {
                    if completed.is_none_or(|old| newest.parse::<u64>().is_ok_and(|new| new > old))
                    {
                        cursor.completed_bill = Some(newest);
                    }
                }
                cursor.scan_after = None;
            } else {
                cursor.scan_after = oldest.map(|id| id.to_string());
            }

            let account = account_id.to_string();
            let next = cursor.clone();
            let progressed = self
                .rows(move |rows| {
                    let mut progressed = false;
                    rows.transaction(&mut || {
                        // The parent is stored before submission. If its venue ID has not arrived,
                        // do not move the cursor past a fill that may be its execution.
                        if !page.is_empty()
                            && rows.list_open_orders(&account)?.iter().any(|order| {
                                order.agent_id.is_some()
                                    && !is_leg(order)
                                    && order.exchange_order_id.as_deref().is_none_or(str::is_empty)
                            })
                        {
                            return Ok(());
                        }
                        let mut ids: Vec<_> = page
                            .iter()
                            .filter_map(|r| nullish_text(r.get("ordId")))
                            .filter(|id| !id.is_empty())
                            .collect();
                        ids.sort_unstable();
                        ids.dedup();
                        let orders = rows.find_orders_by_exchange_ids(&account, &ids)?;
                        let by_identity: HashMap<_, _> = orders
                            .iter()
                            .filter(|order| order.agent_id.is_some() && !is_leg(order))
                            .filter_map(|order| {
                                Some((
                                    (
                                        order.exchange_order_id.as_deref()?,
                                        to_inst_id(&order.symbol),
                                    ),
                                    order,
                                ))
                            })
                            .collect();
                        // Reprocess the overlapping page: an order may now have an exchange ID
                        // that it lacked on the previous pass. The execution primary key dedupes it.
                        for fill in &page {
                            let Some(id) = fill.get("ordId").and_then(|v| v.as_str()) else {
                                continue;
                            };
                            let Some(inst) = nullish_text(fill.get("instId")) else {
                                continue;
                            };
                            let Some(order) = by_identity.get(&(id, inst)) else {
                                continue;
                            };
                            let side = if order.side == OrderSide::Buy {
                                "buy"
                            } else {
                                "sell"
                            };
                            if fill.get("posSide").and_then(|v| v.as_str()) != Some("net")
                                || fill.get("side").and_then(|v| v.as_str()) != Some(side)
                            {
                                continue;
                            }
                            if let Some((trade_id, at)) = execution_of(fill) {
                                rows.record_exchange_execution(&order.id, &trade_id, at)?;
                            }
                        }
                        rows.save_execution_cursor(&account, &next)?;
                        progressed = true;
                        Ok(())
                    })?;
                    Ok(progressed)
                })
                .await?;
            if complete || !progressed {
                break;
            }
        }
        Ok(())
    }

    /// Reads pages older than `after` (from the newest when `None`) until OKX has no more, a page
    /// reaches `until`, or the budget runs out.
    async fn read_back(
        &self,
        mut after: Option<i64>,
        until: Option<i64>,
        budget: &mut usize,
        records: &mut Vec<ExchangePositionRecord>,
        account_id: &str,
    ) -> Result<Read, EngineError> {
        loop {
            if *budget == 0 {
                return Ok(match after {
                    Some(after) => Read::Stopped(after),
                    None => Read::Met,
                });
            }
            *budget -= 1;
            let page = self.deps.client.get_positions_history(after).await?;
            let mut oldest: Option<i64> = None;
            for row in &page {
                let record = record_of(row, account_id)?;
                oldest = Some(oldest.map_or(record.closed_at, |o| o.min(record.closed_at)));
                records.push(record);
            }
            let Some(oldest) = oldest else {
                return Ok(Read::End);
            };
            if page.len() < POSITIONS_HISTORY_PAGE {
                return Ok(Read::End);
            }
            if until.is_some_and(|until| oldest <= until) {
                return Ok(Read::Met);
            }
            // `after` is exclusive. One past the oldest also rereads its millisecond, in case the
            // page split it, unless that would read the same page again.
            after = Some(if after == Some(oldest + 1) {
                oldest
            } else {
                oldest + 1
            });
        }
    }
}

/// How a read ended.
enum Read {
    /// OKX has nothing older.
    End,
    /// It reached what is already recorded.
    Met,
    /// The page budget ran out; older history is still to be read before this uTime.
    Stopped(i64),
}

pub(super) fn execution_of(row: &Row) -> Option<(String, i64)> {
    let trade_id = row.get("tradeId")?.as_str()?.parse::<u64>().ok()?;
    let executed_at = row.get("fillTime")?.as_str()?.parse::<i64>().ok()?;
    (trade_id > 0 && executed_at > 0).then(|| (trade_id.to_string(), executed_at))
}

/// A whole number of milliseconds from an OKX time field.
pub(super) fn okx_time(row: &Row, field: &str) -> Option<i64> {
    let value = js_number(row.get(field));
    (value.is_finite() && value > 0.0 && value.fract() == 0.0).then_some(value as i64)
}

fn isolated_net(row: &Row) -> bool {
    row.get("mgnMode").and_then(|v| v.as_str()) == Some("isolated")
        && row.get("posSide").and_then(|v| v.as_str()) == Some("net")
}

/// One history row, kept whether or not its pair is in the registry.
fn record_of(row: &Row, account_id: &str) -> Result<ExchangePositionRecord, EngineError> {
    let missing = |field: &str| {
        EngineError::message(format!("OKX reported a closed position without {field}"))
    };
    let realized = truthy_text(row.get("realizedPnl")).ok_or_else(|| missing("realizedPnl"))?;
    Ok(ExchangePositionRecord {
        account_id: account_id.to_string(),
        pos_id: nullish_text(row.get("posId"))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| missing("posId"))?,
        opened_at: okx_time(row, "cTime").ok_or_else(|| missing("cTime"))?,
        inst_id: nullish_text(row.get("instId"))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| missing("instId"))?,
        closed_at: okx_time(row, "uTime").ok_or_else(|| missing("uTime"))?,
        realized_pnl: dec(&realized)?.to_fixed(),
        close_type: nullish_text(row.get("type")).unwrap_or_default(),
        opening_side: match (
            isolated_net(row),
            row.get("direction").and_then(|v| v.as_str()),
        ) {
            (true, Some("long")) => Some("BUY".into()),
            (true, Some("short")) => Some("SELL".into()),
            _ => None,
        },
    })
}

/// An open OKX position's result so far. `None` for a pair missing from the registry.
pub(super) fn open_result_of(
    row: &Row,
    registry: &Registry,
) -> Result<Option<OpenPositionResult>, EngineError> {
    let inst_id = nullish_text(row.get("instId")).unwrap_or_default();
    let Some(spec) = registry.symbol_of_inst_id(&inst_id) else {
        return Ok(None);
    };
    let amount = |field: &str| -> Result<String, EngineError> {
        Ok(dec(&truthy_text(row.get(field)).unwrap_or_else(|| "0".into()))?.to_fixed())
    };
    Ok(Some(OpenPositionResult {
        symbol: spec.symbol.clone(),
        pos_id: nullish_text(row.get("posId")).unwrap_or_default(),
        opened_at: okx_time(row, "cTime"),
        updated_at: okx_time(row, "uTime"),
        opening_side: if isolated_net(row) {
            let size = dec(&amount("pos")?)?;
            if size.is_zero() {
                None
            } else if size.is_negative() {
                Some("SELL".into())
            } else {
                Some("BUY".into())
            }
        } else {
            None
        },
        realized_pnl: amount("realizedPnl")?,
        unrealized_pnl: amount("upl")?,
    }))
}
