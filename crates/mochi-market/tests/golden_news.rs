//! The news pipeline's pure functions against `crates/golden/news.json`, which
//! `apps/server/golden/news.ts` recorded from Node over a corpus of hostile feed text, dates,
//! coin mentions and feed URLs. Every case is checked, and a failure names the input.
//!
//! Where Node's output held a lone surrogate the fixture has U+FFFD instead (`loneSurrogates`),
//! which is what this port writes; see the module docs of `mochi_market::news`.

use mochi_domain::{NewsItem, NewsSource};
use mochi_market::news::{
    default_news_sources, mentions_coin, parse_feed, parse_js_date, sanitize_headline, select_news,
    validate_feed_url,
};
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/news.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn cases<'a>(golden: &'a Value, key: &str) -> &'a [Value] {
    golden[key].as_array().unwrap()
}

#[test]
fn built_in_sources_match() {
    let golden = golden();
    let node: Vec<NewsSource> = serde_json::from_value(golden["builtIns"].clone()).unwrap();
    assert_eq!(default_news_sources(), node);
}

#[test]
fn sanitize_matches_node() {
    let golden = golden();
    let cases = cases(&golden, "sanitize");
    assert!(cases.len() > 20);
    let mut failures = Vec::new();
    for case in cases {
        let input = case["input"].as_str().unwrap();
        let output = sanitize_headline(input);
        if output != case["output"].as_str().unwrap() {
            failures.push(format!(
                "{input:?}: got {output:?}, Node {}",
                case["output"]
            ));
        }
        assert!(!output.contains(['<', '>']));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn parse_feed_matches_node() {
    let golden = golden();
    let cases = cases(&golden, "feeds");
    assert!(cases.len() > 40);
    let mut failures = Vec::new();
    for case in cases {
        let xml = case["xml"].as_str().unwrap();
        let source = case["source"].as_str().unwrap();
        let node: Vec<NewsItem> = serde_json::from_value(case["items"].clone()).unwrap();
        let rust = parse_feed(xml, source);
        if rust != node {
            failures.push(format!("{xml:?}\n  got  {rust:?}\n  Node {node:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // The corpus does reach the lone-surrogate cases.
    assert!(cases.iter().any(|c| c["loneSurrogates"] == true));
}

#[test]
fn date_parse_matches_v8() {
    let golden = golden();
    let cases = cases(&golden, "dates");
    assert!(cases.len() > 100);
    let mut failures = Vec::new();
    for case in cases {
        let text = case["text"].as_str().unwrap();
        let expected = case["ms"].as_i64();
        let parsed = parse_js_date(text);
        let local = case["local"].as_bool().unwrap();
        // A local-time date reads as UTC here; the fixture wrote Node's wall-clock reading.
        let ok = parsed.map(|d| d.ms) == expected
            && (expected.is_none() || parsed.is_some_and(|d| d.local == local));
        if !ok {
            failures.push(format!(
                "{text:?}: got {parsed:?}, Node {expected:?} local {local}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn select_news_matches_node() {
    let golden = golden();
    let select = &golden["select"];
    let now = select["now"].as_i64().unwrap();
    let items: Vec<NewsItem> = serde_json::from_value(select["items"].clone()).unwrap();
    for case in select["cases"].as_array().unwrap() {
        let base = case["base"].as_str().unwrap();
        let limit = case["limit"].as_u64().unwrap() as usize;
        let picked: Vec<String> = select_news(&items, base, now, limit)
            .into_iter()
            .map(|item| item.id)
            .collect();
        let node: Vec<String> = serde_json::from_value(case["picked"].clone()).unwrap();
        assert_eq!(picked, node, "{base} limit {limit}");
    }
}

#[test]
fn mentions_coin_matches_node() {
    let golden = golden();
    for case in cases(&golden, "mentions") {
        let title = case["title"].as_str().unwrap();
        let base = case["base"].as_str().unwrap();
        assert_eq!(
            mentions_coin(title, base),
            case["result"].as_bool().unwrap(),
            "{title:?} {base}"
        );
    }
}

#[test]
fn validate_feed_url_matches_node() {
    let golden = golden();
    let mut failures = Vec::new();
    for case in cases(&golden, "urls") {
        let url = case["url"].as_str().unwrap();
        let problem = validate_feed_url(url);
        if problem != case["problem"].as_str() {
            failures.push(format!(
                "{url:?}: got {problem:?}, Node {}",
                case["problem"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
