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
    Regex::new(
        r#"(?i)(authorization|x-api-key|x-goog-api-key|api[_-]?key)(["':=\s]+)(bearer\s+)?[^\s"',}]+"#,
    )
    .unwrap()
});

/// Replaces every known secret, then anything shaped like an API key or an auth header value.
pub fn redact_secrets<'a>(text: &str, secrets: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret, REDACTED);
        }
    }
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
