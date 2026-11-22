//! Guards on the tape's shape: a replay needs at least one playable bar after the warmup, and
//! the five columns of a tape always have one length.

use mochi_market::replay::ReplayMarketData;
use mochi_market::tape::{generate_tape, WARMUP_MINUTES};

#[test]
fn replay_needs_more_minutes_than_the_warmup() {
    for minutes in [0, 1, WARMUP_MINUTES - 1, WARMUP_MINUTES] {
        let error = ReplayMarketData::new(Some(minutes)).err().unwrap();
        assert_eq!(
            error.to_string(),
            format!("A replay tape needs more than {WARMUP_MINUTES} minutes, got {minutes}")
        );
    }

    let mut short = ReplayMarketData::new(Some(WARMUP_MINUTES + 2)).unwrap();
    assert_eq!(short.position(), WARMUP_MINUTES);
    assert!(!short.at_end());
    assert_eq!(short.step().len(), mochi_domain::DEFAULT_SYMBOLS.len());
    assert!(short.at_end());
    assert!(short.step().is_empty());

    assert!(ReplayMarketData::new(None).is_ok());
}

#[test]
fn tape_columns_share_one_length() {
    let tape = generate_tape("BTCUSDT", 100).unwrap();
    assert_eq!(tape.len(), 100);
    for column in [
        tape.open(),
        tape.high(),
        tape.low(),
        tape.close(),
        tape.volume(),
    ] {
        assert_eq!(column.len(), tape.len());
    }
}

#[test]
fn a_replay_of_some_pairs_steps_only_those() {
    let pairs = ["BTCUSDT", "ETHUSDT", "SOLUSDT"];
    let mut replay = ReplayMarketData::with_symbols(Some(WARMUP_MINUTES + 2), &pairs).unwrap();
    assert_eq!(replay.symbols(), pairs);
    let symbols: Vec<_> = replay
        .step()
        .into_iter()
        .map(|quote| quote.symbol)
        .collect();
    assert_eq!(symbols, pairs);
    assert!(replay.get_quote("BNBUSDT").is_none());
}
