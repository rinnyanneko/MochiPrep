use mochi_domain::registry::Instruments;
use mochi_domain::{
    default_account_config, default_instrument, default_live_limits, default_risk_settings,
    AccountBalance, AiDecisionData, EquityPoint, Fill, LedgerEvent, Order, OrderRequest,
    OrderResult, PaperAccount, PaperAccountConfig, Position, PositionView, RiskCheck, RiskResult,
    RiskSettings, SizedTrade,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

fn golden() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../golden/accounting-types.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Every sample under `key` must parse into `T` and serialize back to the identical JSON value.
fn assert_round_trip<T: DeserializeOwned + Serialize>(key: &str) {
    let fixture = golden();
    let samples = fixture["samples"][key].as_array().unwrap();
    assert!(!samples.is_empty(), "no samples for {key}");
    for sample in samples {
        let parsed: T = serde_json::from_value(sample.clone())
            .unwrap_or_else(|error| panic!("{key}: {error}\n{sample}"));
        assert_eq!(&serde_json::to_value(&parsed).unwrap(), sample, "{key}");
    }
}

#[test]
fn defaults_match_typescript() {
    let fixture = golden();
    let defaults = &fixture["defaults"];
    assert_eq!(
        serde_json::to_value(default_account_config()).unwrap(),
        defaults["accountConfig"]
    );
    assert_eq!(
        serde_json::to_value(default_risk_settings()).unwrap(),
        defaults["riskSettings"]
    );
    assert_eq!(
        serde_json::to_value(default_live_limits()).unwrap(),
        defaults["liveLimits"]
    );
}

#[test]
fn samples_round_trip() {
    assert_round_trip::<PaperAccount>("paperAccount");
    assert_round_trip::<PaperAccountConfig>("paperAccountConfig");
    assert_round_trip::<Order>("order");
    assert_round_trip::<OrderRequest>("orderRequest");
    assert_round_trip::<OrderResult>("orderResult");
    assert_round_trip::<Fill>("fill");
    assert_round_trip::<Position>("position");
    assert_round_trip::<PositionView>("positionView");
    assert_round_trip::<LedgerEvent>("ledgerEvent");
    assert_round_trip::<EquityPoint>("equityPoint");
    assert_round_trip::<AccountBalance>("accountBalance");
    assert_round_trip::<RiskSettings>("riskSettings");
    assert_round_trip::<AiDecisionData>("aiDecisionData");
    assert_round_trip::<RiskCheck>("riskCheck");
    assert_round_trip::<RiskResult>("riskResult");
    assert_round_trip::<SizedTrade>("sizedTrade");
}

#[test]
fn config_json_text_matches() {
    let fixture = golden();
    assert_eq!(
        serde_json::to_string(&default_account_config()).unwrap(),
        fixture["configJsonText"].as_str().unwrap()
    );
    assert_eq!(
        serde_json::to_string(&default_risk_settings()).unwrap(),
        fixture["riskJsonText"].as_str().unwrap()
    );
}

#[test]
fn order_request_omits_absent_optionals() {
    // JSON.stringify drops undefined fields, so an absent TS field must stay absent here.
    let request: OrderRequest = serde_json::from_value(json!({
        "accountId": "a", "clientOrderId": "c", "symbol": "BTCUSDT", "side": "BUY",
        "type": "MARKET", "quantity": "1", "leverage": 1, "price": null
    }))
    .unwrap();
    assert!(request.price.is_none());
    let value = serde_json::to_value(&request).unwrap();
    assert!(value.get("price").is_none());
}

#[test]
fn registry_has_defaults_and_registers() {
    let mut instruments = Instruments::with_defaults();
    assert_eq!(
        instruments.get("BTCUSDT"),
        default_instrument("BTCUSDT").as_ref()
    );
    assert!(instruments.get("DOGEUSDT").is_none());

    let mut doge = default_instrument("SOLUSDT").unwrap();
    doge.symbol = "DOGEUSDT".to_string();
    instruments.register(doge.clone());
    assert_eq!(instruments.get("DOGEUSDT"), Some(&doge));

    instruments.unregister("DOGEUSDT");
    assert!(instruments.get("DOGEUSDT").is_none());

    // Default symbols are always available, so unregistering one is a no-op.
    instruments.unregister("BTCUSDT");
    assert!(instruments.get("BTCUSDT").is_some());
}
