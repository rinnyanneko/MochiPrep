//! `AccountVenue` names any exchange, while the strings already stored and sent stay the same.

use mochi_domain::{AccountVenue, Environment, ExchangeId};
use serde_json::{json, Value};

fn read(text: &str) -> Result<AccountVenue, serde_json::Error> {
    serde_json::from_value(Value::String(text.into()))
}

#[test]
fn legacy_venue_strings_round_trip() {
    let cases = [
        ("PAPER", AccountVenue::Paper),
        ("OKX_DEMO", AccountVenue::okx_demo()),
        ("OKX_LIVE", AccountVenue::okx_live()),
    ];
    for (text, venue) in cases {
        assert_eq!(read(text).unwrap(), venue, "{text}");
        assert_eq!(serde_json::to_value(&venue).unwrap(), json!(text), "{text}");
    }
}

#[test]
fn new_exchange_strings_parse() {
    let venue = read("BYBIT_LIVE").unwrap();
    assert_eq!(
        venue,
        AccountVenue::Exchange {
            exchange: ExchangeId::new("bybit"),
            environment: Environment::Live,
        }
    );
    let AccountVenue::Exchange { exchange, .. } = &venue else {
        unreachable!()
    };
    assert_eq!(exchange.as_str(), "bybit");
    assert_eq!(serde_json::to_value(&venue).unwrap(), json!("BYBIT_LIVE"));
    assert_eq!(
        read("B1N4NCE_DEMO").unwrap(),
        AccountVenue::Exchange {
            exchange: ExchangeId::new("b1n4nce"),
            environment: Environment::Demo,
        }
    );
}

#[test]
fn garbage_rejected() {
    for text in [
        "OKX",
        "okx_demo",
        "OKX_PAPER",
        "",
        "_LIVE",
        "OKX_LIVE_LIVE",
        "OKX__LIVE",
        "OK-X_LIVE",
        "Okx_LIVE",
        "OKX_live",
        "paper",
        " PAPER",
    ] {
        assert!(read(text).is_err(), "{text:?} was accepted");
    }
    // Only strings: a number or null is not a venue either.
    assert!(serde_json::from_value::<AccountVenue>(json!(1)).is_err());
    assert!(serde_json::from_value::<AccountVenue>(Value::Null).is_err());
}

#[test]
fn only_live_environments_are_live() {
    assert!(!AccountVenue::Paper.is_live());
    assert!(!AccountVenue::okx_demo().is_live());
    assert!(AccountVenue::okx_live().is_live());
    assert!(read("BYBIT_LIVE").unwrap().is_live());
    assert!(!read("BYBIT_DEMO").unwrap().is_live());
}

#[test]
fn okx_is_lowercase() {
    assert_eq!(ExchangeId::okx().as_str(), "okx");
    assert_eq!(ExchangeId::okx(), ExchangeId::new("okx"));
}
