//! Read models for suggestions, decisions and the arena.
//!
//! The web app refetches their lists every time its event stream opens, so their GETs answer
//! with what the database really holds, exactly as Node reads it.

use axum::extract::{RawQuery, State};
use axum::routing::get;
use axum::{Json, Router};
use mochi_core::paper::PaperStore;
use mochi_core::performance::{max_drawdown_pct, win_rate_pct};
use mochi_decimal::Dec;
use mochi_domain::jsnum::js_number_raw;
use mochi_domain::{AccountBalance, AccountVenue, DataSource, PaperAccount};
use mochi_store::rows;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::error::ApiError;
use crate::runtime::Runtime;
use crate::validate::{self, DecisionQuery, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/signals", get(signals))
        .route("/api/decisions", get(decisions))
        .route("/api/arena", get(arena))
}

/// A store read on the runtime task, its failure as Node's 400.
async fn read<T: Serialize + Send + 'static>(
    state: &AppState,
    work: impl FnOnce(&Runtime) -> Result<T, ApiError> + Send + 'static,
) -> Result<Json<T>, ApiError> {
    Ok(Json(
        state.runtime.call(move |runtime| work(runtime)).await??,
    ))
}

async fn signals(State(state): State<AppState>) -> Result<Json<Vec<Box<RawValue>>>, ApiError> {
    read(&state, |runtime| {
        rows::list_pending_signals(runtime.connection()).map_err(|e| runtime.api_error(e))
    })
    .await
}

async fn decisions(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<Box<RawValue>>>, ApiError> {
    let DecisionQuery { limit, agent_id } =
        validate::parse_query(Schema::DecisionQuery, query.as_deref().unwrap_or(""))?;
    read(&state, move |runtime| {
        // `...(agentId ? { agentId } : {})`: an empty agentId filters nothing.
        let agent_id = agent_id.filter(|id| !id.is_empty());
        rows::list_decisions(runtime.connection(), agent_id.as_deref(), limit)
            .map_err(|e| runtime.api_error(e))
    })
    .await
}

// ---- arena ------------------------------------------------------------------------------------

/// `ArenaRow`, keys in Node's order. The `f64`s are JS numbers in Node and are written the way
/// JS writes them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArenaRow {
    agent_id: String,
    agent_name: String,
    account_id: String,
    source: DataSource,
    venue: AccountVenue,
    is_mock: bool,
    provider_name: String,
    model: String,
    symbol: String,
    /// Every pair the agent trades; `symbol` is the first.
    symbols: Vec<String>,
    status: String,
    initial_balance: String,
    equity: String,
    /// Whose result the row shows; see [`Attribution`].
    attribution: Attribution,
    /// The agent's profit in USDT, `None` when it cannot be told apart from another agent's.
    pnl: Option<String>,
    /// `None` on an exchange account: its balance moves with deposits and transfers, so it is
    /// no base for a return.
    return_pct: Option<Box<RawValue>>,
    realized_pnl: Option<String>,
    max_drawdown_pct: Option<Box<RawValue>>,
    win_rate_pct: Option<Box<RawValue>>,
    trade_count: usize,
    decision_count: usize,
    estimated_ai_cost: Option<String>,
    curve: Vec<CurvePoint>,
    /// An exchange row's own P&L over time, from 0 when the agent was made: each closed position
    /// adds its result when it closed, and the last point adds what is still open. Empty on a
    /// paper row, whose `curve` is the account's equity, and on a shared row.
    pnl_curve: Vec<PnlPoint>,
}

/// Whose result an arena row shows.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Attribution {
    /// A paper account: nothing moves money in or out but trading, so its equity is the result.
    Account,
    /// An exchange account where the agent is the only one trading its pair: its result is what
    /// that pair's fills and open position made.
    Agent,
    /// An exchange account where another agent trades the same pair. One-way position mode nets
    /// them into one OKX position, so no fill or position belongs to either alone.
    Shared,
}

#[derive(Serialize)]
struct CurvePoint {
    ts: i64,
    equity: Box<RawValue>,
}

#[derive(Serialize)]
struct PnlPoint {
    ts: i64,
    pnl: Box<RawValue>,
}

/// History reads behind each row, as Node's `arena()` caps them.
const ARENA_HISTORY: usize = 5000;
/// The most curve points a row carries; longer histories are thinned evenly.
const CURVE_POINTS: usize = 400;

async fn arena(State(state): State<AppState>) -> Result<Json<Vec<ArenaRow>>, ApiError> {
    read(&state, arena_rows).await
}

/// `runtime.arena()` (`runtime.ts:929-969`): one row per agent whose account still exists. A
/// paper row shows its account's result; an exchange row only the agent's own trading (see
/// [`exchange_result`]).
fn arena_rows(runtime: &Runtime) -> Result<Vec<ArenaRow>, ApiError> {
    let conn = runtime.connection();
    // Only each provider's name and kind are read here, never whether it has a key.
    let providers = rows::list_provider_views(conn, false).map_err(|e| runtime.api_error(e))?;
    let store = runtime.engine().store();
    let agents = rows::list_agents(conn).map_err(|e| runtime.api_error(e))?;
    let mut out = Vec::new();
    for agent in &agents {
        let Some(account) = store
            .get_account(&agent.account_id)
            .map_err(|e| runtime.api_error(e))?
        else {
            continue;
        };
        let balance = runtime.balance_of(&account)?;
        let result = if account.venue == AccountVenue::Paper {
            paper_result(runtime, &account, &balance)?
        } else {
            exchange_result(runtime, &account, agent, &agents)?
        };
        let stats = rows::decision_stats(conn, &agent.id).map_err(|e| runtime.api_error(e))?;
        let provider = providers.iter().find(|p| p.id == agent.provider_id);

        out.push(ArenaRow {
            agent_id: agent.id.clone(),
            agent_name: agent.name.clone(),
            account_id: account.id,
            source: account.source,
            venue: account.venue,
            // Only an agent left on the removed mock AI's provider row: it cannot run any more.
            is_mock: provider.is_some_and(|p| p.kind == "MOCK"),
            provider_name: provider
                .map_or_else(|| "Removed provider".to_string(), |p| p.name.clone()),
            model: agent.model.clone(),
            symbol: agent.symbol.clone(),
            symbols: agent.symbols.clone(),
            status: agent.status.clone(),
            initial_balance: account.initial_balance,
            equity: balance.equity,
            attribution: result.attribution,
            pnl: result.pnl,
            return_pct: result.return_pct,
            realized_pnl: result.realized_pnl,
            max_drawdown_pct: result.max_drawdown_pct,
            win_rate_pct: result.win_rate_pct,
            trade_count: result.trade_count,
            decision_count: stats.count,
            // `str(d(cost))`: the JS sum read back as a decimal, in plain notation.
            estimated_ai_cost: stats.estimated_cost.map(cost_text),
            curve: result.curve,
            pnl_curve: result.pnl_curve,
        });
    }
    Ok(out)
}

/// The result columns of one arena row.
struct RowResult {
    attribution: Attribution,
    pnl: Option<String>,
    return_pct: Option<Box<RawValue>>,
    realized_pnl: Option<String>,
    max_drawdown_pct: Option<Box<RawValue>>,
    win_rate_pct: Option<Box<RawValue>>,
    trade_count: usize,
    curve: Vec<CurvePoint>,
    pnl_curve: Vec<PnlPoint>,
}

/// A paper account's result: its equity against its starting balance, as Node computes it.
fn paper_result(
    runtime: &Runtime,
    account: &PaperAccount,
    balance: &AccountBalance,
) -> Result<RowResult, ApiError> {
    let store = runtime.engine().store();
    let history = store
        .list_equity(&account.id, ARENA_HISTORY)
        .map_err(|e| runtime.api_error(e))?;
    let closed = store
        .list_closed_positions(&account.id, ARENA_HISTORY)
        .map_err(|e| runtime.api_error(e))?;
    // Net result per closed position, as a JS number: realized - fees - funding.
    let mut net = Vec::new();
    for position in &closed {
        let value = decimal(&position.realized_pnl)?
            .sub(&decimal(&position.fees_paid)?)
            .sub(&decimal(&position.funding_paid)?);
        net.push(to_number(&value));
    }
    let points: Vec<(i64, f64)> = history
        .iter()
        .map(|p| (p.ts, js_number(&p.equity)))
        .collect();
    let mut equities: Vec<f64> = points.iter().map(|&(_, equity)| equity).collect();
    equities.push(js_number(&balance.equity));

    let initial = decimal(&account.initial_balance)?;
    let gain = decimal(&balance.equity)?.sub(&initial);
    let hundred = Dec::parse("100").expect("100 is a decimal");
    // decimal.js divides by zero to Infinity or NaN, which JSON writes as null.
    let return_pct = match gain.div(&initial) {
        Ok(fraction) => to_number(&fraction.mul(&hundred)),
        Err(_) => f64::NAN,
    };
    let stride = points.len().div_ceil(CURVE_POINTS).max(1);
    let last = points.len().saturating_sub(1);
    let curve = points
        .iter()
        .enumerate()
        .filter(|&(i, _)| i % stride == 0 || i == last)
        .map(|(_, &(ts, equity))| CurvePoint {
            ts,
            equity: js_number_raw(equity),
        })
        .collect();
    Ok(RowResult {
        attribution: Attribution::Account,
        pnl: Some(gain.to_fixed()),
        return_pct: Some(js_number_raw(return_pct)),
        realized_pnl: Some(balance.realized_pnl.clone()),
        max_drawdown_pct: Some(js_number_raw(max_drawdown_pct(&equities))),
        win_rate_pct: win_rate_pct(&net).map(js_number_raw),
        trade_count: closed.len(),
        curve,
        pnl_curve: Vec::new(),
    })
}

/// Arena P&L is the net position result from OKX, attributed only when a known opening-side
/// execution falls in one exchange lifetime. Local aggregate fill times are reconciliation times.
fn exchange_result(
    runtime: &Runtime,
    account: &PaperAccount,
    agent: &rows::Agent,
    agents: &[rows::Agent],
) -> Result<RowResult, ApiError> {
    let conn = runtime.connection();
    let shared_row = || -> Result<RowResult, ApiError> {
        let trade_count = rows::count_agent_fills(conn, &account.id, &agent.id)
            .map_err(|e| runtime.api_error(e))?;
        Ok(RowResult {
            attribution: Attribution::Shared,
            pnl: None,
            return_pct: None,
            realized_pnl: None,
            max_drawdown_pct: None,
            win_rate_pct: None,
            trade_count,
            curve: Vec::new(),
            pnl_curve: Vec::new(),
        })
    };
    // Every pair is attributed on its own; one pair the agent cannot claim alone makes the
    // whole row shared, since a partial sum would read as the agent's full result.
    let mut pairs = Vec::new();
    for symbol in &agent.symbols {
        match pair_result(runtime, account, agent, agents, symbol)? {
            Some(pair) => pairs.push(pair),
            None => return shared_row(),
        }
    }
    let zero = || Dec::parse("0").expect("0 is a decimal");
    let (mut realized, mut unrealized) = (zero(), zero());
    let (mut realized_available, mut unrealized_available) = (true, true);
    let mut net = Vec::new();
    let mut closes: Vec<(i64, Dec)> = Vec::new();
    let mut open_until = None;
    for pair in pairs {
        open_until = open_until.max(pair.open_until);
        realized = realized.add(&pair.realized);
        unrealized = unrealized.add(&pair.unrealized);
        realized_available &= pair.realized_available;
        unrealized_available &= pair.unrealized_available;
        net.extend(pair.net);
        closes.extend(pair.closes);
    }
    let pnl = (realized_available && unrealized_available).then(|| realized.add(&unrealized));
    Ok(RowResult {
        attribution: Attribution::Agent,
        pnl: pnl.as_ref().map(Dec::to_fixed),
        return_pct: None,
        realized_pnl: realized_available.then(|| realized.to_fixed()),
        max_drawdown_pct: None,
        win_rate_pct: win_rate_pct(&net).map(js_number_raw),
        trade_count: net.len(),
        curve: Vec::new(),
        pnl_curve: if realized_available {
            pnl_curve(agent.created_at, closes, pnl.as_ref(), open_until)
        } else {
            Vec::new()
        },
    })
}

/// What one pair of an exchange agent made.
struct PairResult {
    realized: Dec,
    unrealized: Dec,
    realized_available: bool,
    unrealized_available: bool,
    /// Net results of the positions that fully closed.
    net: Vec<f64>,
    /// When each attributed position's realized result landed, and how much it was.
    closes: Vec<(i64, Dec)>,
    /// The last sync of an attributed position that is still open.
    open_until: Option<i64>,
}

/// One pair of an exchange agent, or `None` when another agent shares it: one-way position mode
/// nets both agents' trades into one OKX position, so neither owns its result.
fn pair_result(
    runtime: &Runtime,
    account: &PaperAccount,
    agent: &rows::Agent,
    agents: &[rows::Agent],
    symbol: &str,
) -> Result<Option<PairResult>, ApiError> {
    let conn = runtime.connection();
    if agents.iter().any(|other| {
        other.id != agent.id
            && other.account_id == account.id
            && other.symbols.iter().any(|s| s == symbol)
    }) {
        return Ok(None);
    }

    let executions =
        rows::agent_executions(conn, &account.id, symbol).map_err(|e| runtime.api_error(e))?;
    let open: Vec<_> = runtime
        .exchange_open_results(&account.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.symbol == symbol)
        .collect();
    let history = runtime
        .engine()
        .store()
        .list_position_records(&account.id, &mochi_okx::ids::to_inst_id(symbol), 0)
        .map_err(|e| runtime.api_error(e))?;

    struct Lifetime<'a> {
        opened_at: i64,
        until: i64,
        opening_side: Option<&'a str>,
        realized: Option<&'a str>,
        unrealized: Option<&'a str>,
        completed: bool,
    }
    let mut lifetimes: Vec<_> = history
        .iter()
        .filter(|record| record.opening_side.is_some())
        .filter(|record| {
            !open
                .iter()
                .any(|o| o.pos_id == record.pos_id && o.opened_at == Some(record.opened_at))
        })
        .map(|record| {
            let completed = matches!(record.close_type.as_str(), "2" | "3" | "6");
            Lifetime {
                opened_at: record.opened_at,
                until: record.closed_at,
                opening_side: record.opening_side.as_deref(),
                realized: Some(&record.realized_pnl),
                // A partial close records realized P&L but needs the live position
                // snapshot to know what the remainder is worth.
                unrealized: completed.then_some("0"),
                completed,
            }
        })
        .collect();
    lifetimes.extend(open.iter().filter_map(|position| {
        Some(Lifetime {
            opened_at: position.opened_at?,
            until: position.updated_at?,
            opening_side: Some(position.opening_side.as_deref()?),
            realized: position.realized_pnl.as_deref(),
            unrealized: position.unrealized_pnl.as_deref(),
            completed: false,
        })
    }));
    lifetimes.sort_by_key(|position| position.opened_at);

    let zero = || Dec::parse("0").expect("0 is a decimal");
    let mut result = PairResult {
        realized: zero(),
        unrealized: zero(),
        realized_available: true,
        unrealized_available: true,
        net: Vec::new(),
        closes: Vec::new(),
        open_until: None,
    };
    let mut prior_end = 0_i64;
    for (index, position) in lifetimes.iter().enumerate() {
        // Discard evidence in the overlap of two lifetimes, including a same-ms boundary.
        let start = position.opened_at.max(prior_end.saturating_add(1));
        prior_end = prior_end.max(position.until);
        let end = lifetimes.get(index + 1).map_or(position.until, |next| {
            position.until.min(next.opened_at.saturating_sub(1))
        });
        if position.opened_at < agent.created_at || start > end {
            continue;
        }
        let Some(opening_side) = position.opening_side else {
            continue;
        };
        let first = executions.partition_point(|trade| trade.at < start);
        let last = executions.partition_point(|trade| trade.at <= end);
        let trades = &executions[first..last];
        if !trades.iter().any(|trade| {
            trade.agent_id == agent.id && !trade.reduce_only && trade.side == opening_side
        }) {
            continue;
        }
        // Ambiguous boundary fills cannot establish ownership, but possible co-participation
        // from another agent is enough to withhold the entire position's P&L.
        let possible_start = executions.partition_point(|trade| trade.at < position.opened_at);
        let possible_end = executions.partition_point(|trade| trade.at <= position.until);
        if executions[possible_start..possible_end]
            .iter()
            .any(|trade| trade.agent_id != agent.id)
        {
            return Ok(None);
        }
        if !position.completed {
            result.open_until = result.open_until.max(Some(position.until));
        }
        if let Some(open_pnl) = position.unrealized {
            result.unrealized = result.unrealized.add(&decimal(open_pnl)?);
        } else {
            result.unrealized_available = false;
        }
        let Some(realized_pnl) = position.realized else {
            result.realized_available = false;
            continue;
        };
        let value = decimal(realized_pnl)?;
        if position.completed {
            result.net.push(to_number(&value));
        }
        result.closes.push((position.until, value.clone()));
        result.realized = result.realized.add(&value);
    }
    Ok(Some(result))
}

/// The P&L curve of an exchange row: 0 when the agent was made, the running realized total after
/// each attributed close in time order, and finally the whole P&L, open positions included, at
/// their last sync. Thinned evenly like the equity curves.
fn pnl_curve(
    created_at: i64,
    mut closes: Vec<(i64, Dec)>,
    total: Option<&Dec>,
    open_until: Option<i64>,
) -> Vec<PnlPoint> {
    closes.sort_by_key(|&(at, _)| at);
    let mut running = Dec::parse("0").expect("0 is a decimal");
    let mut points = vec![(created_at, 0.0)];
    for (at, value) in closes {
        running = running.add(&value);
        points.push((at.max(created_at), to_number(&running)));
    }
    if let (Some(total), Some(at)) = (total, open_until) {
        let last = points.last().map_or(created_at, |&(at, _)| at);
        points.push((at.max(last), to_number(total)));
    }
    let stride = points.len().div_ceil(CURVE_POINTS).max(1);
    let last = points.len().saturating_sub(1);
    points
        .iter()
        .enumerate()
        .filter(|&(i, _)| i % stride == 0 || i == last)
        .map(|(_, &(ts, pnl))| PnlPoint {
            ts,
            pnl: js_number_raw(pnl),
        })
        .collect()
}

/// A stored money string as a decimal. Every one was written by the engine, so a failure is a
/// corrupt row, reported as Node would report the throw.
fn decimal(text: &str) -> Result<Dec, ApiError> {
    Dec::parse(text)
        .map_err(|_| ApiError::bad_request(format!("[DecimalError] Invalid argument: {text}")))
}

/// decimal.js `toNumber()`: the nearest double to the exact value.
fn to_number(value: &Dec) -> f64 {
    value.to_fixed().parse().unwrap_or(f64::NAN)
}

/// `Number(text)` for a stored decimal string.
fn js_number(text: &str) -> f64 {
    text.parse().unwrap_or(f64::NAN)
}

/// `str(d(cost))` for a JS number. decimal.js keeps NaN and Infinity as words.
fn cost_text(cost: f64) -> String {
    if cost.is_nan() {
        "NaN".to_string()
    } else if cost.is_infinite() {
        if cost > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        Dec::from_js_number(cost).to_fixed()
    }
}
