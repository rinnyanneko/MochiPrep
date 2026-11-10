use mochi_ai::{ProviderConfig, ProviderKind};
use mochi_store::{open_database, rows};
use serde_json::{json, Value};
#[test]
fn ai_roundtrips_node_database_and_cascades() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("node.sqlite");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/node-created.sqlite"),
        &path,
    )
    .unwrap();
    let conn = open_database(&path).unwrap();
    let provider = ProviderConfig {
        id: "p".into(),
        name: "Gateway".into(),
        kind: ProviderKind::Openai,
        base_url: None,
        default_model: "m".into(),
        input_price_per_m_tok: Some(1.5),
        output_price_per_m_tok: None,
        extra_headers: Some(
            serde_json::from_value(json!({"x-gateway":"synthetic secret"})).unwrap(),
        ),
        created_at: 1,
    };
    rows::insert_provider(&conn, &provider).unwrap();
    assert_eq!(
        serde_json::to_value(rows::get_provider(&conn, "p").unwrap().unwrap()).unwrap(),
        serde_json::to_value(&provider).unwrap()
    );
    let mut provider = provider;
    provider.name = "Changed".into();
    provider.created_at = 99;
    rows::save_provider(&conn, &provider).unwrap();
    assert_eq!(
        rows::get_provider(&conn, "p").unwrap().unwrap().created_at,
        1
    );
    rows::save_provider_key(&conn, "p", "synthetic-key", 2).unwrap();
    rows::save_provider_key(&conn, "p", "replacement-key", 3).unwrap();
    assert_eq!(
        rows::get_provider_keys(&conn).unwrap().get("p").unwrap(),
        "replacement-key"
    );
    let views = serde_json::to_string(&rows::list_provider_views(&conn, false).unwrap()).unwrap();
    assert!(!views.contains("synthetic secret"));
    assert!(!views.contains("replacement-key"));
    let mut agent:rows::Agent=serde_json::from_value(json!({"id":"a","name":"Agent","providerId":"p","model":"m","symbol":"BTCUSDT","timeframe":"1m","strategy":"","accountId":"acct_golden","risk":{},"intervalSec":60,"executionMode":"SIGNAL","useNews":false,"allowTrailingStop":false,"status":"IDLE","running":false,"nextRunAt":null,"lastRunAt":null,"statusDetail":null,"consecutiveErrors":0,"isDemo":true,"createdAt":1})).unwrap();
    rows::insert_agent(&conn, &agent).unwrap();
    agent.name = "Renamed".into();
    agent.is_demo = false;
    agent.created_at = 99;
    rows::save_agent(&conn, &agent).unwrap();
    let got = rows::get_agent(&conn, "a").unwrap().unwrap();
    assert_eq!(got.name, "Renamed");
    assert!(got.is_demo);
    assert_eq!(got.created_at, 1);
    assert!(rows::delete_provider(&conn, "p").is_err());
    let mut decision = json!({"id":"d","agentId":"a","accountId":"acct_golden","ts":10,"outcome":"SIGNAL","decision":{"trailingStop":null},"estimatedCost":"0.1"});
    rows::insert_decision(&conn, &decision).unwrap();
    assert_eq!(
        rows::get_decision(&conn, "d").unwrap(),
        Some(decision.clone())
    );
    decision["outcome"] = json!("DISMISSED");
    decision["ts"] = json!(99);
    rows::save_decision(&conn, &decision).unwrap();
    assert!(rows::list_pending_signals(&conn).unwrap().is_empty());
    assert_eq!(rows::get_decision(&conn, "d").unwrap(), Some(decision));
    assert_eq!(
        conn.query_row("SELECT ts FROM decisions WHERE id='d'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        10
    );
    conn.execute(
        "INSERT INTO sealed_provider_keys VALUES ('p','sealed',1)",
        [],
    )
    .unwrap();
    rows::delete_provider_key(&conn, "p").unwrap();
    assert!(!rows::get_provider_keys(&conn).unwrap().contains_key("p"));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM sealed_provider_keys", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    rows::save_provider_key(&conn, "p", "last", 4).unwrap();
    rows::delete_agent(&conn, "a").unwrap();
    assert!(rows::get_decision(&conn, "d").unwrap().is_none());
    rows::delete_provider(&conn, "p").unwrap();
    assert!(rows::get_provider(&conn, "p").unwrap().is_none());
    assert!(!rows::get_provider_keys(&conn).unwrap().contains_key("p"));
    // The old migration fixture deliberately contains an invalid lowercase mock kind.
    assert!(rows::list_providers(&conn).is_err());
    rows::delete_provider(&conn, "prov_golden").unwrap();
    assert!(rows::list_providers(&conn).unwrap().is_empty());
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../golden/ai-state.json")).unwrap()
}
fn seeded() -> rusqlite::Connection {
    let conn = mochi_store::open_in_memory().unwrap();
    conn.execute("INSERT INTO accounts (id,name,source,initial_balance,wallet_balance,used_margin,realized_pnl,fees_paid,funding_paid,config,created_at) VALUES ('account','Account','REPLAY','10000','10000','0','0','0','0','{}',1)",[]).unwrap();
    let fixture = fixture();
    let inputs = &fixture["inputs"];
    rows::insert_provider(
        &conn,
        &serde_json::from_value(inputs["provider"].clone()).unwrap(),
    )
    .unwrap();
    rows::insert_agent(
        &conn,
        &serde_json::from_value(inputs["agent"].clone()).unwrap(),
    )
    .unwrap();
    for decision in inputs["decisions"].as_array().unwrap() {
        rows::insert_decision(&conn, decision).unwrap();
    }
    conn
}
fn sql_snapshot(conn: &rusqlite::Connection) -> Value {
    let mut out = serde_json::Map::new();
    for table in ["providers", "agents", "decisions"] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let names: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        let rows = stmt
            .query_map([], |row| {
                let mut object = serde_json::Map::new();
                for (index, name) in names.iter().enumerate() {
                    use rusqlite::types::ValueRef;
                    let value = match row.get_ref(index)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(n) => json!(n),
                        ValueRef::Real(n) => json!(n),
                        ValueRef::Text(s) => json!(std::str::from_utf8(s).unwrap()),
                        ValueRef::Blob(_) => panic!("no blobs in AI tables"),
                    };
                    object.insert(name.clone(), value);
                }
                Ok(Value::Object(object))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        out.insert(table.into(), json!(rows));
    }
    Value::Object(out)
}
fn decoded_bodies(values: Vec<Box<serde_json::value::RawValue>>) -> Value {
    json!(values
        .into_iter()
        .map(|v| serde_json::from_str::<Value>(v.get()).unwrap())
        .collect::<Vec<_>>())
}
#[test]
fn ai_sql_writes_and_reads_match_node() {
    let fixture = fixture();
    let conn = seeded();
    let input = &fixture["inputs"];
    assert_eq!(sql_snapshot(&conn), fixture["inserted"]);
    assert_eq!(
        json!(rows::list_providers(&conn).unwrap()),
        fixture["reads"]["providers"]
    );
    assert_eq!(
        json!(rows::list_agents(&conn).unwrap()),
        fixture["reads"]["agents"]
    );
    assert_eq!(
        decoded_bodies(rows::list_decisions(&conn, None, 100).unwrap()),
        fixture["reads"]["decisions"]
    );
    assert_eq!(
        decoded_bodies(rows::list_pending_signals(&conn).unwrap()),
        fixture["reads"]["pending"]
    );
    rows::save_provider(
        &conn,
        &serde_json::from_value(input["providerSave"].clone()).unwrap(),
    )
    .unwrap();
    rows::save_agent(
        &conn,
        &serde_json::from_value(input["agentSave"].clone()).unwrap(),
    )
    .unwrap();
    rows::save_decision(&conn, &input["decisionSave"]).unwrap();
    assert_eq!(sql_snapshot(&conn), fixture["saved"]);
    assert_eq!(
        json!(rows::get_provider(&conn, "provider").unwrap()),
        fixture["savedReads"]["provider"]
    );
    assert_eq!(
        json!(rows::get_agent(&conn, "agent").unwrap()),
        fixture["savedReads"]["agent"]
    );
    assert_eq!(
        json!(rows::get_decision(&conn, "decision-absent").unwrap()),
        fixture["savedReads"]["decision"]
    );
    assert_eq!(
        decoded_bodies(rows::list_pending_signals(&conn).unwrap()),
        fixture["savedReads"]["pending"]
    );
    rows::delete_agent(&conn, "agent").unwrap();
    assert_eq!(sql_snapshot(&conn), fixture["deletedAgent"]);
    rows::delete_provider(&conn, "provider").unwrap();
    assert_eq!(sql_snapshot(&conn), fixture["deletedProvider"]);
}
#[test]
fn grouped_deletes_roll_back_on_failure() {
    let conn = seeded();
    conn.execute_batch("CREATE TRIGGER deny_agent_delete BEFORE DELETE ON agents BEGIN SELECT RAISE(ABORT,'denied'); END;").unwrap();
    assert!(rows::delete_agent(&conn, "agent").is_err());
    assert_eq!(rows::list_decisions(&conn, None, 100).unwrap().len(), 2);
    assert!(rows::get_agent(&conn, "agent").unwrap().is_some());
    rows::save_provider_key(&conn, "provider", "synthetic-key", 1).unwrap();
    conn.execute(
        "INSERT INTO sealed_provider_keys VALUES ('provider','synthetic-sealed',1)",
        [],
    )
    .unwrap();
    conn.execute_batch("CREATE TRIGGER deny_sealed_delete BEFORE DELETE ON sealed_provider_keys BEGIN SELECT RAISE(ABORT,'denied'); END;").unwrap();
    assert!(rows::delete_provider_key(&conn, "provider").is_err());
    assert!(rows::get_provider_keys(&conn)
        .unwrap()
        .contains_key("provider"));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM sealed_provider_keys", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

fn seed_keys(conn: &rusqlite::Connection) {
    rows::save_provider_key(conn, "provider", "synthetic-key", 1).unwrap();
    conn.execute(
        "INSERT INTO sealed_provider_keys VALUES ('provider','synthetic-sealed',1)",
        [],
    )
    .unwrap();
}
fn sealed_key_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM sealed_provider_keys", [], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn nested_agent_delete_is_restored_by_outer_rollback() {
    let conn = seeded();
    let before = sql_snapshot(&conn);
    let outer = conn.unchecked_transaction().unwrap();
    rows::delete_agent(&outer, "agent").unwrap();
    assert!(rows::get_agent(&outer, "agent").unwrap().is_none());
    assert!(rows::list_decisions(&outer, None, 100).unwrap().is_empty());
    outer.rollback().unwrap();
    assert_eq!(sql_snapshot(&conn), before);
}

#[test]
fn nested_key_delete_is_restored_by_outer_rollback() {
    let conn = seeded();
    seed_keys(&conn);
    let outer = conn.unchecked_transaction().unwrap();
    rows::delete_provider_key(&outer, "provider").unwrap();
    assert!(rows::get_provider_keys(&outer).unwrap().is_empty());
    assert_eq!(sealed_key_count(&outer), 0);
    outer.rollback().unwrap();
    assert_eq!(
        rows::get_provider_keys(&conn).unwrap()["provider"],
        "synthetic-key"
    );
    assert_eq!(sealed_key_count(&conn), 1);
}

#[test]
fn failed_nested_agent_delete_preserves_outer_transaction() {
    let conn = seeded();
    conn.execute_batch("CREATE TRIGGER deny_agent_delete BEFORE DELETE ON agents BEGIN SELECT RAISE(ABORT,'denied agent delete'); END;").unwrap();
    let outer = conn.unchecked_transaction().unwrap();
    outer
        .execute("UPDATE accounts SET name='outer-before'", [])
        .unwrap();
    let error = rows::delete_agent(&outer, "agent").unwrap_err();
    assert!(error.to_string().contains("denied agent delete"), "{error}");
    assert_eq!(rows::list_decisions(&outer, None, 100).unwrap().len(), 2);
    assert!(rows::get_agent(&outer, "agent").unwrap().is_some());
    assert_eq!(
        outer
            .query_row("SELECT name FROM accounts", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "outer-before"
    );
    outer
        .execute("UPDATE providers SET name='outer-after'", [])
        .unwrap();
    outer.commit().unwrap();
    assert_eq!(
        rows::get_provider(&conn, "provider").unwrap().unwrap().name,
        "outer-after"
    );
    assert_eq!(
        conn.query_row("SELECT name FROM accounts", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "outer-before"
    );
}

#[test]
fn failed_nested_key_delete_preserves_outer_transaction() {
    let conn = seeded();
    seed_keys(&conn);
    conn.execute_batch("CREATE TRIGGER deny_sealed_delete BEFORE DELETE ON sealed_provider_keys BEGIN SELECT RAISE(ABORT,'denied sealed delete'); END;").unwrap();
    let outer = conn.unchecked_transaction().unwrap();
    outer
        .execute("UPDATE accounts SET name='outer-before'", [])
        .unwrap();
    let error = rows::delete_provider_key(&outer, "provider").unwrap_err();
    assert!(
        error.to_string().contains("denied sealed delete"),
        "{error}"
    );
    assert_eq!(
        rows::get_provider_keys(&outer).unwrap()["provider"],
        "synthetic-key"
    );
    assert_eq!(sealed_key_count(&outer), 1);
    assert_eq!(
        outer
            .query_row("SELECT name FROM accounts", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "outer-before"
    );
    outer
        .execute("UPDATE providers SET name='outer-after'", [])
        .unwrap();
    outer.commit().unwrap();
    assert_eq!(
        rows::get_provider(&conn, "provider").unwrap().unwrap().name,
        "outer-after"
    );
    assert_eq!(
        conn.query_row("SELECT name FROM accounts", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "outer-before"
    );
}
