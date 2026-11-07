use mochi_vault::{
    exchange_context, new_data_key, open, provider_context, seal, unwrap_with_password,
    wrap_with_password, KdfParams, PasswordWrap, VaultError, DEFAULT_KDF,
};
use serde_json::{json, Value};
use unicode_normalization::UnicodeNormalization;

// Decomposed (NFD) on purpose: the vault must normalize before scrypt.
const PASSWORD: &str = "mochi-pa\u{308}sswo\u{308}rd-e\u{301}";
// Small N keeps the tests fast; the format is identical to the default cost.
const FAST: KdfParams = KdfParams {
    n: 1024,
    r: 8,
    p: 1,
};

fn golden_path(name: &str) -> String {
    format!("{}/../golden/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn golden(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(golden_path(name)).unwrap()).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn wrap_of(fixture: &Value) -> PasswordWrap {
    serde_json::from_value(fixture["wrap"].clone()).unwrap()
}

#[test]
fn opens_what_node_sealed() {
    let fixture = golden("vault.json");
    let wrap = wrap_of(&fixture);
    let expected = fixture["dataKeyHex"].as_str().unwrap();

    // The fixture must really differ, or normalization would go untested.
    assert_ne!(fixture["password"], fixture["passwordNfc"]);
    for field in ["password", "passwordNfc"] {
        let key = unwrap_with_password(&wrap, fixture[field].as_str().unwrap()).unwrap();
        assert_eq!(hex(&key), expected, "{field}");
    }

    let key = unwrap_with_password(&wrap, fixture["password"].as_str().unwrap()).unwrap();
    for entry in fixture["sealed"].as_array().unwrap() {
        let opened = open(
            &key,
            entry["sealed"].as_str().unwrap(),
            entry["context"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(opened, entry["plaintext"].as_str().unwrap());
    }
}

#[test]
fn wrong_password_is_reported_as_such() {
    let wrap = wrap_of(&golden("vault.json"));
    let error = unwrap_with_password(&wrap, "nope").unwrap_err();
    assert!(matches!(error, VaultError::WrongPassword));
    assert_eq!(error.to_string(), "Wrong master password");
}

#[test]
fn context_and_tampering_are_refused() {
    let fixture = golden("vault.json");
    let key = unwrap_with_password(&wrap_of(&fixture), PASSWORD).unwrap();
    let first = &fixture["sealed"][0];
    let sealed = first["sealed"].as_str().unwrap();
    let context = first["context"].as_str().unwrap();
    let other_context = fixture["sealed"][1]["context"].as_str().unwrap();

    assert!(open(&key, sealed, other_context).is_err());

    // Flip a bit in the first byte of the body. Changing a base64 character could leave the
    // decoded bytes identical (padding bits), so go through the bytes instead.
    let parts: Vec<&str> = sealed.split('.').collect();
    let mut body = base64_decode(parts[3]);
    body[0] ^= 1;
    let tampered = format!(
        "{}.{}.{}.{}",
        parts[0],
        parts[1],
        parts[2],
        base64_encode(&body)
    );
    assert!(open(&key, &tampered, context).is_err());

    let wrong_version = sealed.replacen("v1.", "v2.", 1);
    assert!(open(&key, &wrong_version, context).is_err());
    assert!(open(&key, "garbage", context).is_err());
    assert!(open(&key, "v1.a.b", context).is_err());
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.decode(text).unwrap()
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(bytes)
}

#[test]
fn round_trip() {
    let key = new_data_key();
    let sealed = seal(&key, "héllo", "ctx");
    assert!(sealed.starts_with("v1."));
    assert_eq!(sealed.split('.').count(), 4);
    assert_eq!(open(&key, &sealed, "ctx").unwrap(), "héllo");
    assert_ne!(
        seal(&key, "héllo", "ctx"),
        sealed,
        "each seal uses a fresh IV"
    );
}

#[test]
fn wrap_round_trip_and_json_shape() {
    let key = new_data_key();
    let wrap = wrap_with_password(&key, PASSWORD, FAST).unwrap();
    assert_eq!(unwrap_with_password(&wrap, PASSWORD).unwrap(), key);
    let json = serde_json::to_value(&wrap).unwrap();
    assert_eq!(json["kdf"], json!({ "N": 1024, "r": 8, "p": 1 }));
    assert!(json["wrappedKey"].is_string() && json["salt"].is_string());
}

#[test]
fn non_power_of_two_cost_is_a_kdf_error() {
    let key = new_data_key();
    let bad = KdfParams {
        n: 1000,
        r: 8,
        p: 1,
    };
    assert!(matches!(
        wrap_with_password(&key, PASSWORD, bad),
        Err(VaultError::Kdf)
    ));
}

#[test]
fn contexts_and_defaults() {
    assert_eq!(provider_context("prov_1"), "provider-key:prov_1");
    assert_eq!(exchange_context("acct_1"), "okx-credentials:acct_1");
    assert_eq!(mochi_vault::WRAP_CONTEXT, "mochiprep:vault-key");
    assert_eq!(
        DEFAULT_KDF,
        KdfParams {
            n: 131_072,
            r: 8,
            p: 1
        }
    );
}

#[test]
fn writes_fixture_for_node() {
    if std::env::var("MOCHI_UPDATE_GOLDEN").as_deref() != Ok("1") {
        return;
    }
    let key = new_data_key();
    let sealed_case = |context: &str, plaintext: &str| json!({ "context": context, "plaintext": plaintext, "sealed": seal(&key, plaintext, context) });
    let fixture = json!({
        "password": PASSWORD,
        "passwordNfc": PASSWORD.nfc().collect::<String>(),
        "dataKeyHex": hex(&key),
        "wrap": wrap_with_password(&key, PASSWORD, FAST).unwrap(),
        "sealed": [
            sealed_case("provider-key:prov_1", "sk-rust-ai-key"),
            sealed_case(
                "okx-credentials:acct_1",
                r#"{"apiKey":"k","secretKey":"s","passphrase":"p","environment":"OKX_DEMO"}"#,
            ),
        ],
    });
    let text = serde_json::to_string_pretty(&fixture).unwrap() + "\n";
    std::fs::write(golden_path("rust-vault.json"), text).unwrap();
}
