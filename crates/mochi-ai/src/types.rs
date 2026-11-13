use indexmap::IndexMap;
use mochi_domain::{
    AccountBalance, Candle, NewsItem, PositionView, Quote, RiskSettings, Timeframe,
};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProviderKind {
    Mock,
    OpenaiCompatible,
    Openai,
    Anthropic,
    Gemini,
    Deepseek,
    Opencode,
}

// Header values can contain credentials, so this type intentionally has no Debug implementation.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub default_model: String,
    #[serde(rename = "inputPricePerMTok")]
    pub input_price_per_m_tok: Option<f64>,
    #[serde(rename = "outputPricePerMTok")]
    pub output_price_per_m_tok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_headers: Option<IndexMap<String, String>>,
    pub created_at: i64,
}

pub type IndicatorSnapshot = IndexMap<String, Option<f64>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketContext {
    pub symbol: String,
    pub timeframe: Timeframe,
    pub now: i64,
    pub horizon_sec: f64,
    pub candles: Vec<Candle>,
    pub indicators: IndicatorSnapshot,
    pub quote: Quote,
    pub position: Option<PositionView>,
    pub balance: AccountBalance,
    pub risk: RiskSettings,
    pub strategy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub news: Option<Vec<NewsItem>>,
}

#[derive(Clone)]
pub struct AiRequest {
    pub model: String,
    pub system: String,
    pub user: String,
    /// Connection tests send prompts without a trading snapshot. Mock analysis requires one.
    pub context: Option<MarketContext>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiResponse {
    pub text: String,
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProviderErrorCode {
    NoKey,
    Auth,
    RateLimit,
    Timeout,
    Network,
    Server,
    Http,
    BadResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub code: ProviderErrorCode,
    pub message: String,
}
impl ProviderError {
    pub fn new(code: ProviderErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn critical(&self) -> bool {
        matches!(
            self.code,
            ProviderErrorCode::NoKey | ProviderErrorCode::Auth
        )
    }
}
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ProviderError {}

pub type CompletionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AiResponse, ProviderError>> + Send + 'a>>;
/// Dropping the future cancels its HTTP request. Callers may select it against lifecycle signals.
pub trait AiProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn complete<'a>(&'a self, request: &'a AiRequest) -> CompletionFuture<'a>;
}
