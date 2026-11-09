use mochi_domain::DEFAULT_SYMBOLS;
use mochi_market::tape::{generate_tape, TAPE_MINUTES};
use sha2::{Digest, Sha256};

fn golden(name: &str) -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/");
    serde_json::from_str(&std::fs::read_to_string(format!("{path}{name}")).unwrap()).unwrap()
}

#[test]
fn head_matches_typescript() {
    let golden = golden("tape.json");
    for symbol in DEFAULT_SYMBOLS {
        let tape = generate_tape(symbol, TAPE_MINUTES).unwrap();
        let expected = golden["head"][symbol].as_array().unwrap();
        assert_eq!(expected.len(), 64);
        for (i, want) in expected.iter().enumerate() {
            let got = serde_json::to_value(tape.candle(i)).unwrap();
            assert_eq!(
                got["time"].as_i64(),
                want["time"].as_i64(),
                "{symbol} time {i}"
            );
            assert_eq!(got["closed"], want["closed"], "{symbol} closed {i}");
            // Compare as f64, not as text: serde writes 68000.0 where JS writes 68000.
            for field in ["open", "high", "low", "close", "volume"] {
                assert_eq!(
                    got[field].as_f64(),
                    want[field].as_f64(),
                    "{symbol} {field} at {i}"
                );
            }
        }
    }
}

#[test]
fn full_tape_hash_matches_typescript() {
    let mut hash = Sha256::new();
    for symbol in DEFAULT_SYMBOLS {
        let tape = generate_tape(symbol, TAPE_MINUTES).unwrap();
        for column in [
            tape.open(),
            tape.high(),
            tape.low(),
            tape.close(),
            tape.volume(),
        ] {
            for value in column {
                hash.update(value.to_le_bytes());
            }
        }
    }
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(digest, golden("tape.json")["sha256"].as_str().unwrap());
}

#[test]
fn unknown_symbol_has_no_tape() {
    assert!(generate_tape("DOGEUSDT", 10).is_none());
}
