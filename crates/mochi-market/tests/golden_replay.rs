use mochi_domain::{Timeframe, DEFAULT_SYMBOLS};
use mochi_market::replay::ReplayMarketData;
use mochi_market::tape::WARMUP_MINUTES;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn golden(name: &str) -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/");
    serde_json::from_str(&std::fs::read_to_string(format!("{path}{name}")).unwrap()).unwrap()
}

const TIMEFRAMES: [(Timeframe, &str); 5] = [
    (Timeframe::M1, "1m"),
    (Timeframe::M5, "5m"),
    (Timeframe::M15, "15m"),
    (Timeframe::H1, "1h"),
    (Timeframe::H4, "4h"),
];

#[test]
fn meta_matches() {
    let meta = &golden("replay.json")["meta"];
    let feed = ReplayMarketData::new(None);
    assert_eq!(feed.len() as i64, meta["len"].as_i64().unwrap());
    assert_eq!(feed.position(), WARMUP_MINUTES);
    assert_eq!(
        feed.first_playable_time(),
        meta["firstPlayableTime"].as_i64().unwrap()
    );
    assert_eq!(feed.end_time(), meta["endTime"].as_i64().unwrap());
    assert_eq!(feed.now(), meta["nowAtStart"].as_i64().unwrap());
}

#[test]
fn steps_match() {
    let golden = golden("replay.json");
    let mut feed = ReplayMarketData::new(None);
    for want in golden["steps"].as_array().unwrap() {
        let got = serde_json::to_value(feed.step()).unwrap();
        assert_eq!(&got, want);
    }
}

#[test]
fn snapshots_match() {
    let golden = golden("replay.json");
    let mut feed = ReplayMarketData::new(None);
    for snapshot in golden["snapshots"].as_array().unwrap() {
        let cursor = snapshot["cursor"].as_i64().unwrap();
        feed.seek(cursor);
        assert_eq!(
            feed.now(),
            snapshot["now"].as_i64().unwrap(),
            "now at {cursor}"
        );
        let quotes: Vec<_> = DEFAULT_SYMBOLS
            .iter()
            .map(|s| serde_json::to_value(feed.get_quote(s).unwrap()).unwrap())
            .collect();
        assert_eq!(
            &Value::Array(quotes),
            &snapshot["quotes"],
            "quotes at {cursor}"
        );

        for (symbol, by_timeframe) in snapshot["candles"].as_object().unwrap() {
            for (timeframe, label) in TIMEFRAMES {
                let want = by_timeframe[label].as_array().unwrap();
                let got = feed.get_candles(symbol, timeframe, 50);
                assert_eq!(got.len(), want.len(), "{symbol} {label} count at {cursor}");
                for (i, (got, want)) in got.iter().zip(want).enumerate() {
                    let at = format!("{symbol} {label} #{i} at {cursor}");
                    assert_eq!(got.time, want["time"].as_i64().unwrap(), "time {at}");
                    assert_eq!(got.closed, want["closed"].as_bool().unwrap(), "closed {at}");
                    assert_eq!(got.open, want["open"].as_f64().unwrap(), "open {at}");
                    assert_eq!(got.high, want["high"].as_f64().unwrap(), "high {at}");
                    assert_eq!(got.low, want["low"].as_f64().unwrap(), "low {at}");
                    assert_eq!(got.close, want["close"].as_f64().unwrap(), "close {at}");
                    assert_eq!(got.volume, want["volume"].as_f64().unwrap(), "volume {at}");
                }
            }
        }
    }
}

#[test]
fn edges_match() {
    let edges = &golden("replay.json")["edges"];
    let mut feed = ReplayMarketData::new(None);
    feed.seek(-5);
    assert_eq!(
        feed.position() as i64,
        edges["afterSeekNegative"].as_i64().unwrap()
    );
    feed.seek(99_999);
    assert_eq!(
        feed.position() as i64,
        edges["afterSeekPastEnd"].as_i64().unwrap()
    );
    assert!(feed.at_end());
    assert_eq!(
        feed.step().len() as i64,
        edges["stepLengthAtEnd"].as_i64().unwrap()
    );
    feed.seek(WARMUP_MINUTES as i64);
    let all = feed.get_candles("BTCUSDT", Timeframe::H4, 100_000);
    assert_eq!(
        all.len() as i64,
        edges["hugeLimitCandles"].as_i64().unwrap()
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn full_step_sequence_hash_matches() {
    let golden = golden("replay.json");
    let mut feed = ReplayMarketData::new(None);
    let mut hash = Sha256::new();
    let mut step_count = 0;
    loop {
        let quotes = feed.step();
        if quotes.is_empty() {
            break;
        }
        step_count += 1;
        for quote in quotes {
            // Quote's field order and value types match the TS object literal, so this is
            // byte-identical to JSON.stringify.
            hash.update(serde_json::to_string(&quote).unwrap().as_bytes());
            hash.update(b"\n");
        }
    }
    assert_eq!(step_count, golden["stepCount"].as_i64().unwrap());
    assert_eq!(
        hex(&hash.finalize()),
        golden["stepsSha256"].as_str().unwrap()
    );
}

#[test]
fn candle_hash_matches() {
    let golden = golden("replay.json");
    let mut feed = ReplayMarketData::new(None);
    let mut hash = Sha256::new();
    for cursor in [5760, 9000, 20000, 25919] {
        feed.seek(cursor);
        for symbol in DEFAULT_SYMBOLS {
            for (timeframe, _) in TIMEFRAMES {
                for candle in feed.get_candles(symbol, timeframe, 300) {
                    hash.update(candle.time.to_le_bytes());
                    for value in [
                        candle.open,
                        candle.high,
                        candle.low,
                        candle.close,
                        candle.volume,
                    ] {
                        hash.update(value.to_le_bytes());
                    }
                    hash.update([u8::from(candle.closed)]);
                }
            }
        }
    }
    assert_eq!(
        hex(&hash.finalize()),
        golden["candlesSha256"].as_str().unwrap()
    );
}

#[test]
fn unknown_symbol_has_no_quote_or_candles() {
    let feed = ReplayMarketData::new(None);
    assert!(feed.get_quote("DOGEUSDT").is_none());
    assert!(feed.get_candles("DOGEUSDT", Timeframe::M1, 5).is_empty());
}
