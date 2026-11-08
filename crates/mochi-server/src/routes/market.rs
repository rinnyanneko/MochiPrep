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

/// Oldest first. The pair must be tradable on that source, as for an order.
async fn candles(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<Candle>>, ApiError> {
    let query: CandleQuery =
        validate::parse_query(Schema::CandleQuery, query.as_deref().unwrap_or(""))?;
    let candles = state
        .runtime
        .call(move |runtime| {
            runtime.assert_tradable(&query.symbol, query.source)?;
            match query.source {
                DataSource::Replay => {
                    Ok(runtime.replay_candles(&query.symbol, query.timeframe, query.limit))
                }
                // Node fetches these from OKX REST; the OKX public adapter is not ported yet.
                DataSource::Live => Err(ApiError::not_available()),
            }
        })
        .await??;
    Ok(Json(candles))
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
