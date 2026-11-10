//! `redactSecrets` from `packages/ai-providers/src/redact.ts`: every message that leaves the
//! runtime (an HTTP error, a toast) passes through this, so a key that slipped into an error
//! text from a provider or exchange never reaches the browser.

use std::sync::LazyLock;

use regex::Regex;

const REDACTED: &str = "[REDACTED]";

// The same three patterns, in the same order. `(?-u:\b)` is JS's ASCII word boundary.
static PREFIXED_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)(sk|pk|rk)-[A-Za-z0-9_-]{8,}").unwrap());
static GOOGLE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)AIza[A-Za-z0-9_-]{16,}").unwrap());
static KEY_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    // JS whitespace includes FEFF but excludes NEL; /i without /u folds ASCII header names
    // without also folding a Kelvin sign into K. Rust's default \s and (?i) differ on both.
    let spaces =
        r"\t-\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";
    Regex::new(&format!(
        r#"((?i-u:authorization|x-api-key|x-goog-api-key|api[_-]?key))(["':={spaces}]+)((?i-u:bearer)[{spaces}]+)?[^{spaces}"',}}]+"#,
    )).unwrap()
});

/// Replaces every known secret, then anything shaped like an API key or an auth header value.
pub fn redact_secrets<'a>(text: &str, secrets: impl IntoIterator<Item = &'a str>) -> String {
    // Replacing one secret first could destroy another match and expose its remainder.
    // Collect matches against the original text, advancing one character to include self-overlap.
    let mut matches = Vec::new();
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        // Provider diagnostics often embed credentials as escaped JSON string content.
        let quoted = serde_json::to_string(secret).expect("a string always serializes");
        let escaped = &quoted[1..quoted.len() - 1];
        for needle in std::iter::once(secret).chain((escaped != secret).then_some(escaped)) {
            let mut from = 0;
            while let Some(offset) = text[from..].find(needle) {
                let start = from + offset;
                matches.push((start, start + needle.len()));
                from = start + text[start..].chars().next().unwrap().len_utf8();
            }
        }
    }
    matches.sort_unstable();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (start, end) in matches {
        if let Some(previous) = spans.last_mut() {
            if start < previous.1 {
                previous.1 = previous.1.max(end);
                continue;
            }
        }
        spans.push((start, end));
    }
    let mut out = String::new();
    let mut cursor = 0;
    for (start, end) in spans {
        out.push_str(&text[cursor..start]);
        out.push_str(REDACTED);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    let out = PREFIXED_KEY.replace_all(&out, REDACTED);
    let out = GOOGLE_KEY.replace_all(&out, REDACTED);
    KEY_HEADER
        .replace_all(&out, "${1}${2}[REDACTED]")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    #[test]
    fn removes_known_secrets_and_key_shapes() {
        let none: [&str; 0] = [];
        assert_eq!(redact_secrets("plain text", none), "plain text");
        assert_eq!(
            redact_secrets("failed with hunter2 twice: hunter2", ["hunter2", ""]),
            "failed with [REDACTED] twice: [REDACTED]"
        );
        assert_eq!(
            redact_secrets("key sk-abcdefgh123 and AIzaabcdefghijklmnop12", none),
            "key [REDACTED] and [REDACTED]"
        );
        // Too short to look like a key, or glued to a word.
        assert_eq!(
            redact_secrets("sk-short xsk-abcdefgh123", none),
            "sk-short xsk-abcdefgh123"
        );
        assert_eq!(
            redact_secrets(
                r#"Authorization: Bearer abc.def, "api_key":"xyz" X-API-KEY=q1"#,
                none
            ),
            // The `bearer ` group is matched but, as in Node, not put back.
            r#"Authorization: [REDACTED], "api_key":"[REDACTED]" X-API-KEY=[REDACTED]"#
        );
    }
}

#[cfg(test)]
mod overlap_tests {
    use super::redact_secrets;
    #[test]
    fn known_matches_cover_original_overlapping_spans_in_either_order() {
        for (text, secrets, expected) in [
            (
                "shared-credential-prefix-private-gateway-suffix",
                [
                    "shared-credential-prefix",
                    "shared-credential-prefix-private-gateway-suffix",
                ],
                "[REDACTED]",
            ),
            (
                "left-overlap-right",
                ["left-overlap", "overlap-right"],
                "[REDACTED]",
            ),
            (
                "left-overlap-right|left-overlap-right",
                ["left-overlap", "overlap-right"],
                "[REDACTED]|[REDACTED]",
            ),
            ("left-right", ["left-", "right"], "[REDACTED][REDACTED]"),
            (
                "left gap right",
                ["left", "right"],
                "[REDACTED] gap [REDACTED]",
            ),
            ("π😀secret-tail", ["π😀secret", "secret-tail"], "[REDACTED]"),
            ("ababa", ["aba", ""], "[REDACTED]"),
        ] {
            assert_eq!(redact_secrets(text, secrets), expected);
            assert_eq!(redact_secrets(text, secrets.into_iter().rev()), expected);
        }
    }
}
