use mochi_store::rows::save_provider_key;
use mochi_store::{
    delete_sealed_exchange_credentials, delete_vault, list_sealed_exchange_credentials,
    open_in_memory, save_sealed_exchange_credentials, save_sealed_provider_key, save_vault_key,
    sealed_provider_keys, vault_key,
};
use mochi_vault::PasswordWrap;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{json, Value};

const TABLES: [&str; 4] = [
    "vault_keys",
    "sealed_provider_keys",
    "sealed_exchange_credentials",
    "provider_keys",
];

fn golden() -> Value {
    let path = format!("{}/../golden/vault-rows.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn setup(conn: &Connection, g: &Value) {
    for sql in g["setup"].as_array().unwrap() {
        conn.execute_batch(sql.as_str().unwrap()).unwrap();
    }
}

fn raw_rows(conn: &Connection, table: &str) -> Vec<Value> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let names: Vec<String> = stmt.column_names().iter().map(|n| n.to_string()).collect();
    let rows = stmt
        .query_map([], |r| {
            let mut row = serde_json::Map::new();
            for (i, name) in names.iter().enumerate() {
                let v = match r.get::<_, SqlValue>(i)? {
                    SqlValue::Null => Value::Null,
                    SqlValue::Integer(n) => json!(n),
                    SqlValue::Real(x) => json!(x),
                    SqlValue::Text(s) => json!(s),
                    SqlValue::Blob(_) => panic!("unexpected blob in {table}.{name}"),
                };
                row.insert(name.clone(), v);
            }
            Ok(Value::Object(row))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn apply(conn: &Connection, step: &Value) {
    let a = &step["args"];
    let now = step["now"].as_i64().unwrap();
    match s(&step["op"]) {
        "saveVaultKey" => {
            let wrap: PasswordWrap = serde_json::from_value(a[2].clone()).unwrap();
            save_vault_key(conn, s(&a[0]), s(&a[1]), &wrap, now).unwrap();
        }
        "saveProviderKey" => save_provider_key(conn, s(&a[0]), s(&a[1]), now).unwrap(),
        "saveSealedProviderKey" => save_sealed_provider_key(conn, s(&a[0]), s(&a[1]), now).unwrap(),
        "saveSealedExchangeCredentials" => {
            save_sealed_exchange_credentials(conn, s(&a[0]), s(&a[1]), now).unwrap()
        }
        "deleteSealedExchangeCredentials" => {
            delete_sealed_exchange_credentials(conn, s(&a[0])).unwrap()
        }
        "deleteVault" => delete_vault(conn).unwrap(),
        other => panic!("unknown op {other}"),
    }
}

#[test]
fn writes_the_same_rows_as_node_and_reads_what_node_reads() {
    let g = golden();
    let conn = open_in_memory().unwrap();
    setup(&conn, &g);
    for step in g["steps"].as_array().unwrap() {
        apply(&conn, step);
        let label = format!("after {} @{}", s(&step["op"]), step["now"]);
        for table in TABLES {
            assert_eq!(
                Value::Array(raw_rows(&conn, table)),
                step["rows"][table],
                "{label}: table {table}"
            );
        }
        let reads = &step["reads"];
        let wrap = vault_key(&conn, "password").unwrap();
        assert_eq!(
            wrap.map(|w| serde_json::to_value(w).unwrap())
                .unwrap_or(Value::Null),
            reads["vaultKey"],
            "{label}: vault_key"
        );
        // Node returns [id, sealed] pairs for the provider map.
        let sealed: Vec<Value> = sealed_provider_keys(&conn)
            .unwrap()
            .into_iter()
            .map(|(a, b)| json!([a, b]))
            .collect();
        assert_eq!(
            Value::Array(sealed),
            reads["sealedProviderKeys"],
            "{label}: sealed_provider_keys"
        );
        let creds: Vec<Value> = list_sealed_exchange_credentials(&conn)
            .unwrap()
            .into_iter()
            .map(|(id, sealed)| json!({"accountId": id, "sealed": sealed}))
            .collect();
        assert_eq!(
            Value::Array(creds),
            reads["sealedExchangeCredentials"],
            "{label}: sealed exchange credentials"
        );
    }
}

fn wrap(tag: &str) -> PasswordWrap {
    serde_json::from_value(json!({
        "salt": format!("salt-{tag}"),
        "kdf": {"N": 1024, "r": 8, "p": 1},
        "wrappedKey": format!("v1.a.b.{tag}"),
    }))
    .unwrap()
}

#[test]
fn upsert_keeps_created_at() {
    let conn = open_in_memory().unwrap();
    save_vault_key(&conn, "password", "PASSWORD", &wrap("one"), 10).unwrap();
    save_vault_key(&conn, "password", "PASSWORD", &wrap("two"), 20).unwrap();
    let created: i64 = conn
        .query_row("SELECT created_at FROM vault_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(created, 10);
    assert_eq!(
        vault_key(&conn, "password").unwrap().unwrap().wrapped_key,
        "v1.a.b.two"
    );
    assert!(vault_key(&conn, "missing").unwrap().is_none());
}

fn seed_parents(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO providers (id,name,kind,default_model,created_at) VALUES ('p','P','openai','m',1);
         INSERT INTO accounts (id,name,source,is_demo,initial_balance,wallet_balance,used_margin,realized_pnl,fees_paid,funding_paid,config,created_at)
           VALUES ('a','A','LIVE',0,'1','1','0','0','0','0','{}',1);",
    )
    .unwrap();
}

#[test]
fn sealing_a_provider_key_removes_the_plaintext_row() {
    let conn = open_in_memory().unwrap();
    seed_parents(&conn);
    save_provider_key(&conn, "p", "sk-plain", 1).unwrap();
    save_sealed_provider_key(&conn, "p", "v1.a.b.c", 2).unwrap();
    assert!(raw_rows(&conn, "provider_keys").is_empty());
    assert_eq!(sealed_provider_keys(&conn).unwrap().len(), 1);
}

#[test]
fn a_failed_sealed_write_leaves_the_plaintext_row() {
    // No such provider: the sealed insert violates the foreign key, so nothing may change.
    let conn = open_in_memory().unwrap();
    seed_parents(&conn);
    save_provider_key(&conn, "p", "sk-plain", 1).unwrap();
    assert!(save_sealed_provider_key(&conn, "ghost", "x", 2).is_err());
    assert_eq!(raw_rows(&conn, "provider_keys").len(), 1);
}

#[test]
fn delete_vault_empties_all_three_tables() {
    let conn = open_in_memory().unwrap();
    seed_parents(&conn);
    save_vault_key(&conn, "password", "PASSWORD", &wrap("x"), 1).unwrap();
    save_sealed_provider_key(&conn, "p", "s", 1).unwrap();
    save_sealed_exchange_credentials(&conn, "a", "s", 1).unwrap();
    delete_vault(&conn).unwrap();
    for table in &TABLES[..3] {
        assert!(raw_rows(&conn, table).is_empty(), "{table}");
    }
}

#[test]
fn deleting_the_parent_cascades_to_sealed_rows() {
    let conn = open_in_memory().unwrap();
    seed_parents(&conn);
    save_sealed_provider_key(&conn, "p", "s", 1).unwrap();
    save_sealed_exchange_credentials(&conn, "a", "s", 1).unwrap();
    conn.execute("DELETE FROM providers WHERE id='p'", [])
        .unwrap();
    conn.execute("DELETE FROM accounts WHERE id='a'", [])
        .unwrap();
    assert!(raw_rows(&conn, "sealed_provider_keys").is_empty());
    assert!(raw_rows(&conn, "sealed_exchange_credentials").is_empty());
}

#[test]
fn grouped_writes_nest_inside_an_open_transaction() {
    let conn = open_in_memory().unwrap();
    seed_parents(&conn);
    conn.execute_batch("BEGIN").unwrap();
    save_sealed_provider_key(&conn, "p", "s", 1).unwrap();
    conn.execute_batch("ROLLBACK").unwrap();
    assert!(raw_rows(&conn, "sealed_provider_keys").is_empty());
}
