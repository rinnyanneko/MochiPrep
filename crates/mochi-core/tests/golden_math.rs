use mochi_core::fill_model::{
    is_marketable, is_resting_limit_filled, is_stop_triggered, is_take_profit_triggered,
    market_fill_price,
};
use mochi_core::ledger::{audit_ledger, fold_ledger};
use mochi_core::performance::{max_drawdown_pct, win_rate_pct};
use mochi_core::perp;
use mochi_decimal::Dec;
use mochi_domain::{
    InstrumentSpecification, LedgerEvent, OrderSide, PaperAccount, PaperAccountConfig,
    PositionSide, Quote,
};
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/core-math.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn dec(v: &Value) -> Dec {
    v.as_str().unwrap().parse().unwrap()
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn side(v: &Value) -> PositionSide {
    serde_json::from_value(v.clone()).unwrap()
}

fn order_side(v: &Value) -> OrderSide {
    serde_json::from_value(v.clone()).unwrap()
}

fn quote(v: &Value) -> Quote {
    serde_json::from_value(v.clone()).unwrap()
}

#[test]
fn perp_math_matches_typescript() {
    let g = golden();
    let p = &g["perp"];
    for c in p["notional"].as_array().unwrap() {
        let got = perp::notional(&dec(&c["quantity"]), &dec(&c["price"]));
        assert_eq!(got.to_fixed(), text(&c["out"]), "{c}");
    }
    for c in p["pnl"].as_array().unwrap() {
        let got = perp::pnl(
            side(&c["side"]),
            &dec(&c["entry"]),
            &dec(&c["exit"]),
            &dec(&c["quantity"]),
        );
        assert_eq!(got.to_fixed(), text(&c["out"]), "{c}");
    }
    for c in p["initialMargin"].as_array().unwrap() {
        let leverage = c["leverage"].as_u64().unwrap() as u32;
        let got = perp::initial_margin(&dec(&c["notional"]), leverage).unwrap();
        assert_eq!(got.to_fixed(), text(&c["out"]), "{c}");
    }
    for c in p["liquidationPrice"].as_array().unwrap() {
        let got = perp::liquidation_price(
            side(&c["side"]),
            &dec(&c["entry"]),
            &dec(&c["quantity"]),
            &dec(&c["margin"]),
            &dec(&c["maintenanceRate"]),
        )
        .unwrap();
        assert_eq!(got.to_fixed(), text(&c["out"]), "{c}");
    }
    for c in p["funding"].as_array().unwrap() {
        let got = perp::funding(
            side(&c["side"]),
            &dec(&c["markPrice"]),
            &dec(&c["quantity"]),
            &dec(&c["rate"]),
        );
        assert_eq!(got.to_fixed(), text(&c["out"]), "{c}");
    }
}

#[test]
fn math_for_rejects_other_market_types() {
    let g = golden();
    let defaults = mochi_domain::default_instruments();
    for c in g["perp"]["mathFor"].as_array().unwrap() {
        let mut spec: InstrumentSpecification = defaults[0].clone();
        spec.market_type = text(&c["marketType"]).to_string();
        match perp::math_for(&spec) {
            Ok(()) => assert!(c["error"].is_null(), "{c}"),
            Err(e) => assert_eq!(e.to_string(), text(&c["error"]), "{c}"),
        }
    }
}

#[test]
fn market_fill_price_matches_typescript() {
    let g = golden();
    for c in g["fill"]["marketFill"].as_array().unwrap() {
        let config: PaperAccountConfig = serde_json::from_value(c["config"].clone()).unwrap();
        let instrument: InstrumentSpecification =
            serde_json::from_value(c["instrument"].clone()).unwrap();
        let got = market_fill_price(
            order_side(&c["side"]),
            &dec(&c["quantity"]),
            &quote(&c["quote"]),
            &config,
            &instrument,
        )
        .unwrap();
        assert_eq!(got.to_fixed(), text(&c["out"]), "{c}");
    }
}

#[test]
fn limit_and_trigger_rules_match_typescript() {
    let g = golden();
    let f = &g["fill"];
    for c in f["marketable"].as_array().unwrap() {
        let got = is_marketable(
            order_side(&c["side"]),
            &dec(&c["limit"]),
            &quote(&c["quote"]),
        )
        .unwrap();
        assert_eq!(got, c["out"].as_bool().unwrap(), "{c}");
    }
    for c in f["restingLimit"].as_array().unwrap() {
        let got = is_resting_limit_filled(
            order_side(&c["side"]),
            &dec(&c["limit"]),
            &quote(&c["quote"]),
        )
        .unwrap();
        assert_eq!(got, c["out"].as_bool().unwrap(), "{c}");
    }
    for c in f["stop"].as_array().unwrap() {
        let long = c["positionIsLong"].as_bool().unwrap();
        let got = is_stop_triggered(long, &dec(&c["trigger"]), &dec(&c["last"]));
        assert_eq!(got, c["out"].as_bool().unwrap(), "{c}");
    }
    for c in f["takeProfit"].as_array().unwrap() {
        let long = c["positionIsLong"].as_bool().unwrap();
        let got = is_take_profit_triggered(long, &dec(&c["trigger"]), &dec(&c["last"]));
        assert_eq!(got, c["out"].as_bool().unwrap(), "{c}");
    }
}

#[test]
fn ledger_fold_and_audit_match_typescript() {
    let g = golden();
    for c in g["ledger"].as_array().unwrap() {
        let name = text(&c["name"]);
        let account: PaperAccount = serde_json::from_value(c["account"].clone()).unwrap();
        let events: Vec<LedgerEvent> = serde_json::from_value(c["events"].clone()).unwrap();
        let totals = fold_ledger(&events).unwrap();
        let fold = &c["fold"];
        assert_eq!(totals.wallet, text(&fold["wallet"]), "{name}");
        assert_eq!(totals.used_margin, text(&fold["usedMargin"]), "{name}");
        assert_eq!(totals.realized_pnl, text(&fold["realizedPnl"]), "{name}");
        assert_eq!(totals.fees_paid, text(&fold["feesPaid"]), "{name}");
        assert_eq!(totals.funding_paid, text(&fold["fundingPaid"]), "{name}");
        let expected: Vec<&str> = c["audit"].as_array().unwrap().iter().map(text).collect();
        assert_eq!(audit_ledger(&account, &events).unwrap(), expected, "{name}");
    }
}

#[test]
fn performance_stats_match_typescript() {
    let g = golden();
    let floats = |v: &Value| -> Vec<f64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect()
    };
    for c in g["performance"]["maxDrawdown"].as_array().unwrap() {
        let got = max_drawdown_pct(&floats(&c["equity"]));
        assert_eq!(got, c["out"].as_f64().unwrap(), "{c}");
    }
    for c in g["performance"]["winRate"].as_array().unwrap() {
        let got = win_rate_pct(&floats(&c["closedNetPnl"]));
        assert_eq!(got, c["out"].as_f64(), "{c}");
    }
}
