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
        // JSON cannot carry -0, so the value is compared first and the sign of a zero result
        // comes from the third column (JS `Object.is(Math.round(x), -0)`).
        let negative_zero = case[2].as_bool().unwrap();
        let got = js_round(x);
        assert_eq!(got, expected, "{x}");
        assert_eq!(got == 0.0 && got.is_sign_negative(), negative_zero, "{x}");
    }
    for case in fixture["toFixed"].as_array().unwrap() {
        let x = case[0].as_f64().unwrap();
        let digits = case[1].as_u64().unwrap() as usize;
        let expected = case[2].as_str().unwrap();
        assert_eq!(js_to_fixed(x, digits), expected, "{x} {digits}");
    }
    // Inputs JSON can't hold as numbers are stored as String(x); Rust parses "NaN" and
    // "Infinity" (and "1e+21") back to the same f64.
    for case in fixture["toFixedSpecial"].as_array().unwrap() {
        let x: f64 = case[0].as_str().unwrap().parse().unwrap();
        let digits = case[1].as_u64().unwrap() as usize;
        let expected = case[2].as_str().unwrap();
        assert_eq!(js_to_fixed(x, digits), expected, "{x} {digits}");
    }
}

#[test]
fn negative_zero_matches_javascript() {
    // The fixture cannot hold -0, so check the JS results for it directly.
    assert_eq!(js_to_fixed(-0.0, 2), "0.00");
    assert!(js_round(-0.0).is_sign_negative());
    // Math.round's -0 never shows in output: every path from js_round to text prints it as 0,
    // exactly as JSON.stringify(-0) and (-0).toFixed(2) do.
    assert_eq!(js_number_string(js_round(-0.4)), "0");
    assert_eq!(js_to_fixed(js_round(-0.4), 2), "0.00");
    assert_eq!(js_to_fixed(js_round(-0.4) * 0.1, 1), "0.0");
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
