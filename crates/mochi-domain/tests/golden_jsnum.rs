use mochi_domain::jsnum::{js_number_raw, js_number_string, js_round, js_to_fixed};

fn golden(name: &str) -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/");
    serde_json::from_str(&std::fs::read_to_string(format!("{path}{name}")).unwrap()).unwrap()
}

#[test]
fn matches_javascript() {
    let fixture = golden("jsnum.json");
    for case in fixture["round"].as_array().unwrap() {
        let x = case[0].as_f64().unwrap();
        let expected = case[1].as_f64().unwrap();
        // JSON cannot carry -0, so compare by value (-0 == 0) rather than by bits.
        assert_eq!(js_round(x), expected, "{x}");
    }
    for case in fixture["toFixed"].as_array().unwrap() {
        let x = case[0].as_f64().unwrap();
        let digits = case[1].as_u64().unwrap() as usize;
        let expected = case[2].as_str().unwrap();
        assert_eq!(js_to_fixed(x, digits), expected, "{x} {digits}");
    }
}

#[test]
fn negative_zero_matches_javascript() {
    // The fixture cannot hold -0, so check the JS results for it directly.
    assert_eq!(js_to_fixed(-0.0, 2), "0.00");
    assert_eq!(js_round(-0.0), 0.0);
}

#[test]
fn number_text_matches_json_stringify() {
    let fixture = golden("jsnumber.json");
    for case in fixture.as_array().unwrap() {
        let x = case[0].as_f64().unwrap();
        let expected = case[1].as_str().unwrap();
        assert_eq!(js_number_string(x), expected, "{x:?}");
        // The wire form is the raw text, byte for byte.
        assert_eq!(js_number_raw(x).get(), expected, "{x:?}");
    }
}

#[test]
fn non_finite_numbers_are_null_and_negative_zero_is_zero() {
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(js_number_string(x), "null");
    }
    assert_eq!(js_number_string(-0.0), "0");
}

#[test]
fn struct_fields_use_javascript_number_text() {
    let mut risk = mochi_domain::default_risk_settings();
    risk.max_risk_per_trade_pct = 0.0000015;
    risk.max_notional = 1e17;
    let text = serde_json::to_string(&risk).unwrap();
    assert!(text.contains("\"maxRiskPerTradePct\":0.0000015,"), "{text}");
    assert!(
        text.contains("\"maxNotional\":100000000000000000}"),
        "{text}"
    );
}
