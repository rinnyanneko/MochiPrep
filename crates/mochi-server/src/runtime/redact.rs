//! Shared provider/runtime redaction; one implementation covers overlapping secrets.
pub use mochi_ai::redact_secrets;

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
