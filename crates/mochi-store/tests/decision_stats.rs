use mochi_store::{open_in_memory, rows::decision_stats};
use serde_json::json;

#[test]
fn stored_cost_strings_follow_javascript_number_semantics() {
    let conn = open_in_memory().unwrap();
    for (index, (text, expected)) in [
        ("0x10", 16.0),
        ("inf", f64::NAN),
        ("infinity", f64::NAN),
        ("\u{feff}2\u{feff}", 2.0),
        ("\u{0085}2", f64::NAN),
        ("", 0.0),
        (" 0.25 ", 0.25),
    ]
    .into_iter()
    .enumerate()
    {
        let id = index.to_string();
        conn.execute(
            "INSERT INTO decisions (id, agent_id, account_id, ts, outcome, body) VALUES (?, ?, 'a', 0, 'HOLD', ?)",
            rusqlite::params![id, id, json!({"estimatedCost": text}).to_string()],
        )
        .unwrap();
        let stats = decision_stats(&conn, &id).unwrap();
        assert_eq!(stats.count, 1);
        let actual = stats.estimated_cost.unwrap();
        if expected.is_nan() {
            assert!(actual.is_nan(), "{text:?}: {actual}");
        } else {
            assert_eq!(actual, expected, "{text:?}");
        }
    }
}
