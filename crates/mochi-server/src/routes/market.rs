//! `/api/market` (`app.ts` "market").

use axum::extract::{RawQuery, State};
use axum::routing::get;
use axum::{Json, Router};
use mochi_domain::{Candle, DataSource, Quote};

use crate::error::ApiError;
use crate::validate::{self, CandleQuery, Schema, SourceQuery};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/market/candles", get(candles))
        .route("/api/market/quotes", get(quotes))
}

/// Oldest first. The pair must be tradable on that source, as for an order. Live candles come
/// from OKX REST (at most 300, cached 3 s), awaited outside the runtime task.
async fn candles(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<Candle>>, ApiError> {
    let query: CandleQuery =
        validate::parse_query(Schema::CandleQuery, query.as_deref().unwrap_or(""))?;
    let symbol = query.symbol.clone();
    let source = query.source;
    let replay = state
        .runtime
        .call(move |runtime| {
            runtime.assert_tradable(&symbol, source)?;
            Ok(match source {
                DataSource::Replay => {
                    Some(runtime.replay_candles(&symbol, query.timeframe, query.limit))
                }
                DataSource::Live => None,
            })
        })
        .await??;
    if let Some(candles) = replay {
        return Ok(Json(candles));
    }
    let live = state.runtime.call(|runtime| runtime.live()).await?;
    match live
        .candles(&query.symbol, query.timeframe, query.limit)
        .await
    {
        Ok(candles) => Ok(Json(candles)),
        Err(message) => {
            let error = state
                .runtime
                .call(move |runtime| runtime.api_error(message))
                .await?;
            Err(error)
        }
    }
}

/// The latest quote of every registered pair that has one on that source.
async fn quotes(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<Quote>>, ApiError> {
    let query: SourceQuery =
        validate::parse_query(Schema::SourceQuery, query.as_deref().unwrap_or(""))?;
    let quotes = state
        .runtime
        .call(move |runtime| runtime.quotes(query.source))
        .await?;
    Ok(Json(quotes))
}
