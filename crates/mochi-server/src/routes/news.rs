//! `/api/news` (`app.ts` "news"): headlines for one pair, and the feeds they come from.
//!
//! Feeds are fetched outside the runtime task, so a slow feed never holds up quotes or other
//! requests. Every success is 200, and every refusal a 400 with Node's text.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::routing::{get, put};
use axum::{Json, Router};
use mochi_domain::NewsSource;
use mochi_market::news::NewsResult;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::validate::{self, NewsQuery, NewsSourceInput, NewsSourceToggle, Schema};
use crate::AppState;

/// What `GET /api/news` asks for (`runtime.newsFor(symbol, 12)`).
const NEWS_LIMIT: usize = 12;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/news", get(news))
        .route("/api/news/sources", get(sources).post(add))
        .route("/api/news/sources/{id}", put(toggle).delete(remove))
}

/// `{ items, error }`: the newest headlines, this coin's first. `error` names the feeds that
/// could not be read; the items are still good.
async fn news(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<NewsResult>, ApiError> {
    let query: NewsQuery =
        validate::parse_query(Schema::NewsQuery, query.as_deref().unwrap_or(""))?;
    let (sources, base_currency, service) = state
        .runtime
        .call(move |runtime| {
            Ok::<_, ApiError>((
                runtime.news_sources()?,
                runtime.news_base_currency(&query.symbol),
                runtime.news(),
            ))
        })
        .await??;
    Ok(Json(
        service
            .for_symbol(sources, &base_currency, NEWS_LIMIT)
            .await,
    ))
}

async fn sources(State(state): State<AppState>) -> Result<Json<Vec<NewsSource>>, ApiError> {
    let sources = state
        .runtime
        .call(|runtime| runtime.news_sources())
        .await??;
    Ok(Json(sources))
}

async fn add(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<NewsSource>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: NewsSourceInput = validate::parse_input(Schema::NewsSourceInput, body.as_ref())?;
    let source = state
        .runtime
        .call(move |runtime| runtime.add_news_source(input.name, input.url))
        .await??;
    Ok(Json(source))
}

/// The body is checked before the id, as Node evaluates its arguments.
async fn toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body = validate::read_body(&headers, &body)?;
    let input: NewsSourceToggle = validate::parse_input(Schema::NewsSourceToggle, body.as_ref())?;
    state
        .runtime
        .call(move |runtime| runtime.set_news_source_enabled(&id, input.enabled))
        .await??;
    Ok(Json(json!({ "ok": true })))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    validate::read_body(&headers, &body)?;
    state
        .runtime
        .call(move |runtime| runtime.remove_news_source(&id))
        .await??;
    Ok(Json(json!({ "ok": true })))
}
