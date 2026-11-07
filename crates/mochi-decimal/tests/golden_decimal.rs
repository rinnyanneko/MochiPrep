use mochi_decimal::{Dec, DecError};
use serde_json::Value;
use std::cmp::Ordering;

fn golden(name: &str) -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/");
    serde_json::from_str(&std::fs::read_to_string(format!("{path}{name}")).unwrap()).unwrap()
}

/// The fixture writes "error" where decimal.js throws or returns a non-finite value.
fn shown(result: Result<Dec, DecError>) -> String {
    result.map_or_else(|_| "error".to_string(), |v| v.to_fixed())
}

fn apply(case: &Value) -> String {
    let op = case["op"].as_str().unwrap();
    if op == "fromNumber" {
        return Dec::from_js_number(case["a"].as_f64().unwrap()).to_fixed();
    }
    let a: Dec = case["a"].as_str().unwrap().parse().unwrap();
    let b: Option<Dec> = case["b"].as_str().map(|s| s.parse().unwrap());
    let b = || b.as_ref().expect("case needs b");
    let dp = || case["dp"].as_u64().expect("case needs dp") as u32;
    match op {
        "add" => a.add(b()).to_fixed(),
        "sub" => a.sub(b()).to_fixed(),
        "mul" => a.mul(b()).to_fixed(),
        "div" => shown(a.div(b())),
        "neg" => a.neg().to_fixed(),
        "floor" => a.floor().to_fixed(),
        "ceil" => a.ceil().to_fixed(),
        "round" => a.round().to_fixed(),
        "toDP" => a.to_decimal_places(dp()).to_fixed(),
        "toFixed" => a.to_fixed(),
        "toFixedDp" => a.to_fixed_dp(dp()),
        "cmp" => match a.cmp_value(b()) {
            Ordering::Less => "-1".to_string(),
            Ordering::Equal => "0".to_string(),
            Ordering::Greater => "1".to_string(),
        },
        "floorToStep" => shown(a.floor_to_step(b())),
        "ceilToStep" => shown(a.ceil_to_step(b())),
        "roundToStep" => shown(a.round_to_step(b())),
        other => panic!("unknown op {other}"),
    }
}

#[test]
fn matches_decimal_js() {
    let fixture = golden("decimal.json");
    let cases = fixture.as_array().unwrap();
    assert!(cases.len() > 400);
    for case in cases {
        assert_eq!(apply(case), case["out"].as_str().unwrap(), "{case}");
    }
}
