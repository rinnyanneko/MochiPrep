//! Read models for decisions/arena and the remaining Plan 5 OKX routes.
//!
//! The web app refetches their lists every time its event stream opens, so their GETs answer
//! with what the database really holds, exactly as Node reads it. Nothing is made up: a server
//! without OKX sessions reports every exchange account as not connected. OKX writes answer 501
//! until Plan 5 ports them.

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use mochi_core::paper::PaperStore;
use mochi_core::performance::{max_drawdown_pct, win_rate_pct};
use mochi_decimal::Dec;
use mochi_domain::jsnum::js_number_raw;
use mochi_domain::{default_live_limits, AccountVenue, DataSource, RiskSettings};
use mochi_store::rows;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::error::ApiError;
use crate::runtime::Runtime;
use crate::validate::{self, DecisionQuery, Schema};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        // exchange accounts
        .route("/api/exchange/status", get(exchange_status))
        .route("/api/exchange/okx/connect", post(not_available))
        .route("/api/exchange/okx/{id}/disconnect", post(not_available))
        .route("/api/exchange/okx/{id}/arm", post(not_available))
        .route("/api/exchange/okx/{id}/disarm", post(not_available))
        // suggestions, decisions, arena
        .route("/api/signals", get(signals))
        .route("/api/decisions", get(decisions))
        .route("/api/arena", get(arena))
}

/// A write this backend cannot do yet. Fastify parses the body before any handler, so a body
/// it cannot parse is still Node's 400 (or 415) first.
async fn not_available(headers: HeaderMap, body: Bytes) -> ApiError {
    match validate::read_body(&headers, &body) {
        Ok(_) => ApiError::not_available(),
        Err(error) => error,
    }
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

/// `ExchangeStatus`, keys in Node's order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeStatus {
    account_id: String,
    name: String,
    venue: AccountVenue,
    connected: bool,
    saved: bool,
    armed: bool,
    healthy: bool,
    last_sync_at: Option<i64>,
    error: Option<String>,
    limits: RiskSettings,
}

/// `exchangeStatuses()` for a backend with no OKX sessions: every exchange account is
/// disconnected and disarmed, which is the truth here.
async fn exchange_status(
    State(state): State<AppState>,
) -> Result<Json<Vec<ExchangeStatus>>, ApiError> {
    read(&state, |runtime| {
        let accounts = runtime
            .engine()
            .store()
            .list_accounts()
            .map_err(|e| runtime.api_error(e))?;
        let mut statuses = Vec::new();
        for account in accounts {
            if account.venue == AccountVenue::Paper {
                continue;
            }
            let saved = rows::has_sealed_exchange_credentials(runtime.connection(), &account.id)
                .map_err(|e| runtime.api_error(e))?;
            statuses.push(ExchangeStatus {
                account_id: account.id,
                name: account.name,
                venue: account.venue,
                connected: false,
                saved,
                armed: false,
                healthy: false,
                last_sync_at: None,
                error: None,
                limits: account.live_limits.unwrap_or_else(default_live_limits),
            });
        }
        Ok(statuses)
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
    return_pct: Box<RawValue>,
    realized_pnl: String,
    max_drawdown_pct: Box<RawValue>,
    win_rate_pct: Option<Box<RawValue>>,
    trade_count: usize,
    decision_count: usize,
    estimated_ai_cost: Option<String>,
    curve: Vec<CurvePoint>,
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

/// `runtime.arena()` (`runtime.ts:929-969`): one row per agent whose account still exists.
fn arena_rows(runtime: &Runtime) -> Result<Vec<ArenaRow>, ApiError> {
    let conn = runtime.connection();
    // Only each provider's name and kind are read here, never whether it has a key.
    let providers = rows::list_provider_views(conn, false).map_err(|e| runtime.api_error(e))?;
    let store = runtime.engine().store();
    let mut out = Vec::new();
    for agent in rows::list_agents(conn).map_err(|e| runtime.api_error(e))? {
        let Some(account) = store
            .get_account(&agent.account_id)
            .map_err(|e| runtime.api_error(e))?
        else {
            continue;
        };
        let balance = runtime.balance_of(&account)?;
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
        let stats = rows::decision_stats(conn, &agent.id).map_err(|e| runtime.api_error(e))?;
        let provider = providers.iter().find(|p| p.id == agent.provider_id);

        let initial = decimal(&account.initial_balance)?;
        let hundred = Dec::parse("100").expect("100 is a decimal");
        // decimal.js divides by zero to Infinity or NaN, which JSON writes as null.
        let return_pct = match decimal(&balance.equity)?.sub(&initial).div(&initial) {
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

        out.push(ArenaRow {
            agent_id: agent.id,
            agent_name: agent.name,
            account_id: account.id,
            source: account.source,
            venue: account.venue,
            is_mock: provider.is_some_and(|p| p.kind == "MOCK"),
            provider_name: provider
                .map_or_else(|| "Removed provider".to_string(), |p| p.name.clone()),
            model: agent.model,
            symbol: agent.symbol,
            status: agent.status,
            initial_balance: account.initial_balance,
            equity: balance.equity,
            return_pct: js_number_raw(return_pct),
            realized_pnl: balance.realized_pnl,
            max_drawdown_pct: js_number_raw(max_drawdown_pct(&equities)),
            win_rate_pct: win_rate_pct(&net).map(js_number_raw),
            trade_count: closed.len(),
            decision_count: stats.count,
            // `str(d(cost))`: the JS sum read back as a decimal, in plain notation.
            estimated_ai_cost: stats.estimated_cost.map(cost_text),
            curve,
        });
    }
    Ok(out)
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
