use mochi_vault::{
    exchange_context, new_data_key, open, provider_context, seal, unwrap_with_password,
    wrap_with_password, DataKey, KdfParams, PasswordWrap, VaultError, DEFAULT_KDF,
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
        assert_eq!(hex(key.as_bytes()), expected, "{field}");
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

    assert_eq!(open(&key, sealed, other_context), Err(VaultError::Decrypt));

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
    assert_eq!(open(&key, &tampered, context), Err(VaultError::Decrypt));

    // `open` is deliberately stricter than Node about IV and tag lengths: Node's
    // createDecipheriv accepts other IV sizes and short tags, Rust refuses them as Malformed.
    let wrong_version = sealed.replacen("v1.", "v2.", 1);
    assert_eq!(
        open(&key, &wrong_version, context),
        Err(VaultError::Malformed)
    );
    assert_eq!(open(&key, "garbage", context), Err(VaultError::Malformed));
    assert_eq!(open(&key, "v1.a.b", context), Err(VaultError::Malformed));
    let short_iv = format!(
        "{}.{}.{}.{}",
        parts[0],
        base64_encode(&[1u8; 8]),
        parts[2],
        parts[3]
    );
    assert_eq!(open(&key, &short_iv, context), Err(VaultError::Malformed));
    let short_tag = format!(
        "{}.{}.{}.{}",
        parts[0],
        parts[1],
        base64_encode(&[1u8; 8]),
        parts[3]
    );
    assert_eq!(open(&key, &short_tag, context), Err(VaultError::Malformed));
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
        "dataKeyHex": hex(key.as_bytes()),
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

#[test]
fn data_key_debug_does_not_print_bytes() {
    let key = DataKey::from_bytes([0xab; 32]);
    let text = format!("{key:?}");
    assert_eq!(text, "DataKey(…)");
    assert!(!text.contains("ab") && !text.contains("171"));
}

#[test]
fn malformed_wrapped_key_after_derivation_is_wrong_password() {
    let key = new_data_key();
    let good = wrap_with_password(&key, PASSWORD, FAST).unwrap();

    // Node answers all of these with "Wrong master password" once the key derived.
    let garbage = PasswordWrap {
        wrapped_key: "garbage".into(),
        ..good.clone()
    };
    assert_eq!(
        unwrap_with_password(&garbage, PASSWORD),
        Err(VaultError::WrongPassword)
    );

    // Authentic wrapped key whose inner text is not base64, or not 32 bytes.
    let kek_wrap = |inner: &str| {
        let salt = base64_decode(&good.salt);
        // Independent derivation, so the test does not lean on the code under test.
        let mut raw = [0u8; 32];
        let params = scrypt::Params::new(10, 8, 1, 32).unwrap();
        let password: String = PASSWORD.nfc().collect();
        scrypt::scrypt(password.as_bytes(), &salt, &params, &mut raw).unwrap();
        let kek = DataKey::from_bytes(raw);
        PasswordWrap {
            wrapped_key: seal(&kek, inner, mochi_vault::WRAP_CONTEXT),
            ..good.clone()
        }
    };
    for inner in ["!!!not base64!!!", &base64_encode(&[1u8; 16])] {
        assert_eq!(
            unwrap_with_password(&kek_wrap(inner), PASSWORD),
            Err(VaultError::WrongPassword)
        );
    }

    // A bad salt or bad cost is not a password problem.
    let bad_salt = PasswordWrap {
        salt: "***".into(),
        ..good.clone()
    };
    assert_eq!(
        unwrap_with_password(&bad_salt, PASSWORD),
        Err(VaultError::Malformed)
    );
    let bad_kdf = PasswordWrap {
        kdf: KdfParams {
            n: 1000,
            r: 8,
            p: 1,
        },
        ..good
    };
    assert_eq!(
        unwrap_with_password(&bad_kdf, PASSWORD),
        Err(VaultError::Kdf)
    );
}

#[test]
fn oversized_scrypt_cost_is_refused_before_allocating() {
    let key = new_data_key();
    for kdf in [
        KdfParams {
            n: 1 << 30,
            r: 8,
            p: 1,
        },
        KdfParams {
            n: 1 << 21,
            r: 1,
            p: 1,
        },
        KdfParams {
            n: 1024,
            r: 17,
            p: 1,
        },
        KdfParams {
            n: 1024,
            r: 8,
            p: 17,
        },
        // 128 * 2^20 * 16 = 2 GiB: each field is within its cap but the memory is not.
        KdfParams {
            n: 1 << 20,
            r: 16,
            p: 1,
        },
    ] {
        assert_eq!(
            wrap_with_password(&key, PASSWORD, kdf),
            Err(VaultError::Kdf),
            "{kdf:?}"
        );
    }
}

#[test]
fn node_default_and_test_kdfs_are_accepted() {
    let key = new_data_key();
    for kdf in [FAST, DEFAULT_KDF] {
        let wrap = wrap_with_password(&key, PASSWORD, kdf).unwrap();
        assert_eq!(unwrap_with_password(&wrap, PASSWORD).unwrap(), key);
    }
}
