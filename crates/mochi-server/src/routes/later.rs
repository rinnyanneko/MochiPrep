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
            is_mock: provider.is_some_and(|p| p.kind == "MOCK"),
            provider_name: provider
                .map_or_else(|| "Removed provider".to_string(), |p| p.name.clone()),
            model: agent.model.clone(),
            symbol: agent.symbol.clone(),
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
    })
}

/// How far a fill's local time may fall before OKX's open time and still be the fill that opened
/// the position: OKX's clock and this computer's differ.
const CLOCK_SKEW_MS: i64 = 5_000;
/// How long after OKX's last update a closing fill may be recorded: fills are recorded by the
/// sync that sees them, which can run late.
const RECORD_LAG_MS: i64 = 60_000;

/// An agent's result on an exchange account: what its positions on its pair made.
///
/// The account's equity cannot be used: a deposit, or a transfer from the funding account, moves
/// it as much as a trade does. Nor can local fills: a stop or take profit OKX triggers, or a close
/// in the OKX app, leaves none. So closed positions come from OKX's position history
/// (`realizedPnl`, which includes fees and funding), and the open one from the last sync.
///
/// A position is the agent's when it opened after the agent was created and a fill of one of the
/// agent's orders on the pair was recorded while it was open (by fill time, not order time, so a
/// limit order that rests a while still counts, and the closing fill of the agent's previous
/// position does not). One the owner opened and closed in the OKX app is not. All of this holds
/// only while no other agent on the account trades the pair: one-way mode nets them into one
/// position.
fn exchange_result(
    runtime: &Runtime,
    account: &PaperAccount,
    agent: &rows::Agent,
    agents: &[rows::Agent],
) -> Result<RowResult, ApiError> {
    let conn = runtime.connection();
    let shared = agents.iter().any(|other| {
        other.id != agent.id && other.account_id == account.id && other.symbol == agent.symbol
    });
    if shared {
        // Which orders it placed is all that still belongs to it.
        let trade_count = rows::count_agent_fills(conn, &account.id, &agent.id)
            .map_err(|e| runtime.api_error(e))?;
        return Ok(RowResult {
            attribution: Attribution::Shared,
            pnl: None,
            return_pct: None,
            realized_pnl: None,
            max_drawdown_pct: None,
            win_rate_pct: None,
            trade_count,
            curve: Vec::new(),
        });
    }
    let filled = rows::agent_fill_times(conn, &account.id, &agent.id, &agent.symbol)
        .map_err(|e| runtime.api_error(e))?;
    let owned = |opened_at: i64, until: i64| {
        opened_at >= agent.created_at
            && filled.iter().any(|&at| {
                at >= opened_at - CLOCK_SKEW_MS && at <= until.saturating_add(RECORD_LAG_MS)
            })
    };
    let open: Vec<_> = runtime
        .exchange_open_results(&account.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.symbol == agent.symbol)
        .collect();

    let zero = || Dec::parse("0").expect("0 is a decimal");
    let (mut realized, mut unrealized) = (zero(), zero());
    let mut net = Vec::new();
    let history = runtime
        .engine()
        .store()
        .list_position_records(
            &account.id,
            &mochi_okx::ids::to_inst_id(&agent.symbol),
            agent.created_at,
        )
        .map_err(|e| runtime.api_error(e))?;
    for record in &history {
        // A partly closed position still open: its realized part is in its open result.
        let still_open = open
            .iter()
            .any(|o| o.pos_id == record.pos_id && o.opened_at == Some(record.opened_at));
        if still_open || !owned(record.opened_at, record.closed_at) {
            continue;
        }
        let value = decimal(&record.realized_pnl)?;
        net.push(to_number(&value));
        realized = realized.add(&value);
    }
    // Without a session the open position is unknown, so only the closed result shows. Without an
    // open time from OKX, no agent can be said to own it.
    for position in &open {
        if position
            .opened_at
            .is_some_and(|opened_at| owned(opened_at, i64::MAX))
        {
            realized = realized.add(&decimal(&position.realized_pnl)?);
            unrealized = unrealized.add(&decimal(&position.unrealized_pnl)?);
        }
    }
    Ok(RowResult {
        attribution: Attribution::Agent,
        pnl: Some(realized.add(&unrealized).to_fixed()),
        return_pct: None,
        realized_pnl: Some(realized.to_fixed()),
        max_drawdown_pct: None,
        win_rate_pct: win_rate_pct(&net).map(js_number_raw),
        trade_count: net.len(),
        curve: Vec::new(),
    })
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
