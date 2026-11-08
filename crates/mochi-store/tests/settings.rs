//! The settings, instruments and vault-presence accessors, against the SQL in
//! `apps/server/src/db/store.ts`. Node and Rust share the file, so values are stored as the same
//! JSON text Node's `JSON.stringify` writes.

use mochi_domain::default_instrument;
use mochi_store::{
    delete_instrument, get_setting, has_vault, list_instruments, open_in_memory, save_instrument,
    set_setting,
};
use serde_json::{json, Value};

fn raw_setting(conn: &rusqlite::Connection, key: &str) -> String {
    conn.query_row("SELECT value FROM settings WHERE key=?", [key], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn settings_round_trip_json() {
    let conn = open_in_memory().unwrap();
    assert_eq!(get_setting::<u32>(&conn, "replay.speed").unwrap(), None);

    set_setting(&conn, "replay.speed", &20u32).unwrap();
    set_setting(&conn, "replay.cursor", &5_800usize).unwrap();
    set_setting(&conn, "demo.active", &true).unwrap();
    let sources = json!([{ "id": "news_1", "name": "Feed", "enabled": false, "builtIn": false }]);
    set_setting(&conn, "news.sources", &sources).unwrap();

    assert_eq!(get_setting::<u32>(&conn, "replay.speed").unwrap(), Some(20));
    assert_eq!(
        get_setting::<i64>(&conn, "replay.cursor").unwrap(),
        Some(5_800)
    );
    assert_eq!(
        get_setting::<bool>(&conn, "demo.active").unwrap(),
        Some(true)
    );
    assert_eq!(
        get_setting::<Value>(&conn, "news.sources").unwrap(),
        Some(sources)
    );

    // The text is what JSON.stringify writes, so Node reads it back unchanged.
    assert_eq!(raw_setting(&conn, "replay.speed"), "20");
    assert_eq!(raw_setting(&conn, "demo.active"), "true");
    assert_eq!(
        raw_setting(&conn, "news.sources"),
        r#"[{"id":"news_1","name":"Feed","enabled":false,"builtIn":false}]"#
    );

    // A second write replaces the value (upsert), as `setSetting` does.
    set_setting(&conn, "replay.speed", &60u32).unwrap();
    assert_eq!(get_setting::<u32>(&conn, "replay.speed").unwrap(), Some(60));

    // A value Node wrote reads back too.
    conn.execute(
        "INSERT INTO settings (key,value) VALUES ('demo.active','false') \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [],
    )
    .unwrap();
    assert_eq!(
        get_setting::<bool>(&conn, "demo.active").unwrap(),
        Some(false)
    );

    // Text that is not the requested type is an error, not a silent default.
    assert!(get_setting::<u32>(&conn, "news.sources").is_err());
}

#[test]
fn instruments_round_trip_in_symbol_order() {
    let conn = open_in_memory().unwrap();
    assert!(list_instruments(&conn).unwrap().is_empty());

    let mut doge = default_instrument("SOLUSDT").unwrap();
    doge.symbol = "DOGEUSDT".into();
    doge.base_currency = "DOGE".into();
    let mut ada = doge.clone();
    ada.symbol = "ADAUSDT".into();
    ada.base_currency = "ADA".into();

    save_instrument(&conn, &doge).unwrap();
    save_instrument(&conn, &ada).unwrap();
    assert_eq!(
        list_instruments(&conn).unwrap(),
        vec![ada.clone(), doge.clone()]
    );

    // Saving again updates the spec in place.
    doge.max_leverage = 20;
    save_instrument(&conn, &doge).unwrap();
    assert_eq!(
        list_instruments(&conn).unwrap(),
        vec![ada.clone(), doge.clone()]
    );

    // The spec column is the camelCase JSON Node stores.
    let spec: String = conn
        .query_row(
            "SELECT spec FROM instruments WHERE symbol='DOGEUSDT'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let spec: Value = serde_json::from_str(&spec).unwrap();
    assert_eq!(spec["maxLeverage"], json!(20));
    assert_eq!(spec["venueContractValue"], json!("1"));

    delete_instrument(&conn, "ADAUSDT").unwrap();
    assert_eq!(list_instruments(&conn).unwrap(), vec![doge]);
}

#[test]
fn vault_presence_is_any_vault_keys_row() {
    let conn = open_in_memory().unwrap();
    assert!(!has_vault(&conn).unwrap());
    conn.execute(
        "INSERT INTO vault_keys (id,kind,params,created_at) VALUES ('password','password','{}',1)",
        [],
    )
    .unwrap();
    assert!(has_vault(&conn).unwrap());
}
