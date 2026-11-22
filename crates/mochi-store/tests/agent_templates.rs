//! Agent template storage: built-ins come from code, user templates from SQLite, and deleting a
//! built-in only hides it until restored.

use mochi_domain::default_risk_settings;
use mochi_store::open_in_memory;
use mochi_store::templates::*;

fn fields(name: &str) -> AgentTemplateFields {
    AgentTemplateFields {
        name: name.into(),
        description: "mine".into(),
        strategy: "Buy dips".into(),
        timeframe: "5m".into(),
        interval_sec: 300,
        execution_mode: "SIGNAL".into(),
        risk: default_risk_settings(),
        use_news: true,
        allow_trailing_stop: false,
    }
}

#[test]
fn builtins_are_conservative() {
    let builtins = builtin_agent_templates();
    assert!(builtins.len() >= 3);
    for t in builtins {
        assert!(t.built_in && t.id.starts_with(BUILTIN_PREFIX), "{}", t.id);
        assert_eq!(t.execution_mode, "SIGNAL", "{}", t.id);
        assert!(t.risk.require_stop_loss, "{}", t.id);
        let defaults = default_risk_settings();
        assert!(t.risk.max_leverage <= defaults.max_leverage, "{}", t.id);
        assert!(t.risk.max_risk_per_trade_pct <= defaults.max_risk_per_trade_pct);
        assert!(t.risk.max_daily_loss_pct <= defaults.max_daily_loss_pct);
        assert!(t.risk.max_notional <= defaults.max_notional);
    }
}

/// An agent sees candles of its own timeframe only, so a preset that asks the model to compare
/// another timeframe asks for data it never gets.
#[test]
fn builtin_strategies_use_only_the_agents_timeframe() {
    for t in builtin_agent_templates() {
        let text = t.strategy.to_lowercase();
        for tf in ["1m", "5m", "15m", "1h", "4h"] {
            if tf == t.timeframe {
                continue;
            }
            let other_tf = text
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word == tf);
            assert!(!other_tf, "{} mentions {tf}: {}", t.id, t.strategy);
        }
    }
}

#[test]
fn user_templates_round_trip() {
    let conn = open_in_memory().unwrap();
    let builtins = builtin_agent_templates().len();
    assert_eq!(list_agent_templates(&conn).unwrap().len(), builtins);

    let saved = insert_agent_template(&conn, "tpl_1", &fields("Mine")).unwrap();
    assert!(!saved.built_in);
    assert_eq!(saved.name, "Mine");
    assert!(saved.use_news);
    assert_eq!(saved.risk, default_risk_settings());
    assert!(saved.created_at.is_some());

    let list = list_agent_templates(&conn).unwrap();
    assert_eq!(list.len(), builtins + 1);
    assert_eq!(list.last().unwrap().id, "tpl_1");

    let mut edit = fields("Renamed");
    edit.risk.max_leverage = 2.0;
    let updated = update_agent_template(&conn, "tpl_1", &edit)
        .unwrap()
        .unwrap();
    assert_eq!(updated.name, "Renamed");
    assert_eq!(updated.risk.max_leverage, 2.0);
    assert_eq!(updated.created_at, saved.created_at);

    assert!(update_agent_template(&conn, "missing", &edit)
        .unwrap()
        .is_none());
    assert!(delete_agent_template(&conn, "tpl_1").unwrap());
    assert!(!delete_agent_template(&conn, "tpl_1").unwrap());
    assert!(get_agent_template(&conn, "tpl_1").unwrap().is_none());
}

#[test]
fn builtins_hide_and_restore() {
    let conn = open_in_memory().unwrap();
    let first = builtin_agent_templates()[0].id.clone();
    insert_agent_template(&conn, "tpl_keep", &fields("Keep")).unwrap();

    assert!(update_agent_template(&conn, &first, &fields("x"))
        .unwrap()
        .is_none());
    assert!(delete_agent_template(&conn, &first).unwrap());
    assert!(!delete_agent_template(&conn, &first).unwrap());
    assert!(get_agent_template(&conn, &first).unwrap().is_none());
    assert!(!list_agent_templates(&conn)
        .unwrap()
        .iter()
        .any(|t| t.id == first));

    assert_eq!(restore_builtin_agent_templates(&conn).unwrap(), 1);
    let list = list_agent_templates(&conn).unwrap();
    assert_eq!(list[0].id, first);
    assert!(list.iter().any(|t| t.id == "tpl_keep"));
    assert!(!delete_agent_template(&conn, "builtin_unknown").unwrap());
}
