use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DataSource {
    Replay,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Timeframe {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "4h")]
    H4,
}

impl Timeframe {
    pub fn ms(self) -> i64 {
        match self {
            Timeframe::M1 => 60_000,
            Timeframe::M5 => 300_000,
            Timeframe::M15 => 900_000,
            Timeframe::H1 => 3_600_000,
            Timeframe::H4 => 14_400_000,
        }
    }
}

/// OHLCV stays `f64` because it only feeds charts and indicators, never the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candle {
    /// Bar open time, ms since epoch.
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    /// False for the bar still forming. Agents only ever see closed bars.
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub source: DataSource,
    pub symbol: String,
    pub bid: String,
    pub ask: String,
    pub last: String,
    pub mark: String,
    /// True when the venue gave no mark price and `mark` falls back to the bid/ask mid.
    pub mark_estimated: bool,
    pub funding_rate: Option<String>,
    pub next_funding_time: Option<i64>,
    /// Market timestamp of this quote, ms.
    pub ts: i64,
}
