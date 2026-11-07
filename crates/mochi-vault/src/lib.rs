//! Envelope encryption for secrets at rest, byte-compatible with `apps/server/src/vault.ts`.
//!
//! One random data key seals every secret; the master password only wraps that data key. The
//! stored formats are shared with the Node backend, so existing installs keep opening: do not
//! change a constant, an encoding or the order of fields here without changing Node too.
//!
//! No error or `Debug` output in this crate carries a key, a password or a plaintext.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

const VERSION: &str = "v1";
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;
const SALT_LEN: usize = 16;

/// Authenticated context for the wrapped data key.
pub const WRAP_CONTEXT: &str = "mochiprep:vault-key";

/// The AES-256 data key that seals every secret.
pub type DataKey = [u8; 32];

/// scrypt cost. The serde names match the JSON Node stores in `vault_keys.params`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    #[serde(rename = "N")]
    pub n: u32,
    pub r: u32,
    pub p: u32,
}

/// About 128 MiB and a few hundred ms per guess: slow enough to make offline guessing expensive.
pub const DEFAULT_KDF: KdfParams = KdfParams {
    n: 131_072,
    r: 8,
    p: 1,
};

/// The data key wrapped by a password-derived key. This is the JSON stored in `vault_keys.params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordWrap {
    pub salt: String,
    pub kdf: KdfParams,
    pub wrapped_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    /// The wrapped key failed its GCM check. The tag doubles as the password check.
    #[error("Wrong master password")]
    WrongPassword,
    #[error("Unrecognized sealed value")]
    Malformed,
    #[error("Could not decrypt sealed value")]
    Decrypt,
    #[error("Invalid key derivation parameters")]
    Kdf,
}

pub fn provider_context(id: &str) -> String {
    format!("provider-key:{id}")
}

pub fn exchange_context(account_id: &str) -> String {
    format!("okx-credentials:{account_id}")
}

pub fn new_data_key() -> DataKey {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    key
}

/// AES-256-GCM. `context` is authenticated but not stored: a ciphertext copied into another row
/// fails to open instead of being misused.
pub fn seal(key: &DataKey, plaintext: &str, context: &str) -> String {
    let mut iv = [0u8; IV_LEN];
    OsRng.fill_bytes(&mut iv);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let payload = Payload {
        msg: plaintext.as_bytes(),
        aad: context.as_bytes(),
    };
    // Encryption only fails for inputs beyond GCM's size limit, which a secret never reaches.
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&iv), payload)
        .expect("AES-GCM encryption of a short secret cannot fail");
    // The crate appends the tag to the ciphertext; the stored format keeps it in its own field.
    let (body, tag) = encrypted.split_at(encrypted.len() - TAG_LEN);
    [
        VERSION.to_string(),
        STANDARD.encode(iv),
        STANDARD.encode(tag),
        STANDARD.encode(body),
    ]
    .join(".")
}

pub fn open(key: &DataKey, sealed: &str, context: &str) -> Result<String, VaultError> {
    let parts: Vec<&str> = sealed.split('.').collect();
    let [version, iv, tag, body] = parts[..] else {
        return Err(VaultError::Malformed);
    };
    if version != VERSION || iv.is_empty() || tag.is_empty() {
        return Err(VaultError::Malformed);
    }
    let iv = STANDARD.decode(iv).map_err(|_| VaultError::Malformed)?;
    let tag = STANDARD.decode(tag).map_err(|_| VaultError::Malformed)?;
    let body = STANDARD.decode(body).map_err(|_| VaultError::Malformed)?;
    if iv.len() != IV_LEN || tag.len() != TAG_LEN {
        return Err(VaultError::Malformed);
    }

    let mut ciphertext = body;
    ciphertext.extend_from_slice(&tag);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let payload = Payload {
        msg: &ciphertext,
        aad: context.as_bytes(),
    };
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&iv), payload)
        .map_err(|_| VaultError::Decrypt)?;
    String::from_utf8(plaintext).map_err(|_| VaultError::Malformed)
}

/// CPU- and memory-heavy (about 128 MiB at the default cost): call from a blocking thread.
fn derive(password: &str, salt: &[u8], kdf: KdfParams) -> Result<DataKey, VaultError> {
    if !kdf.n.is_power_of_two() || kdf.n < 2 {
        return Err(VaultError::Kdf);
    }
    let log_n = u8::try_from(kdf.n.trailing_zeros()).map_err(|_| VaultError::Kdf)?;
    let params = scrypt::Params::new(log_n, kdf.r, kdf.p, 32).map_err(|_| VaultError::Kdf)?;
    let mut derived = [0u8; 32];
    // NFC first, as Node does, so the same typed password derives the same key on every platform.
    let password: String = password.nfc().collect();
    scrypt::scrypt(password.as_bytes(), salt, &params, &mut derived)
        .map_err(|_| VaultError::Kdf)?;
    Ok(derived)
}

/// Wraps the data key under a key derived from the password with a fresh random salt.
/// Call through `spawn_blocking`: the derivation is slow by design.
pub fn wrap_with_password(
    data_key: &DataKey,
    password: &str,
    kdf: KdfParams,
) -> Result<PasswordWrap, VaultError> {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let kek = derive(password, &salt, kdf)?;
    // Node wraps the base64 text of the key, not the raw bytes; keep that for compatibility.
    let wrapped_key = seal(&kek, &STANDARD.encode(data_key), WRAP_CONTEXT);
    Ok(PasswordWrap {
        salt: STANDARD.encode(salt),
        kdf,
        wrapped_key,
    })
}

/// The GCM tag doubles as the password check, so nothing about the password is stored.
/// Call through `spawn_blocking`: the derivation is slow by design.
pub fn unwrap_with_password(wrap: &PasswordWrap, password: &str) -> Result<DataKey, VaultError> {
    let salt = STANDARD
        .decode(&wrap.salt)
        .map_err(|_| VaultError::Malformed)?;
    let kek = derive(password, &salt, wrap.kdf)?;
    let encoded_key = match open(&kek, &wrap.wrapped_key, WRAP_CONTEXT) {
        Ok(text) => text,
        Err(VaultError::Decrypt) => return Err(VaultError::WrongPassword),
        Err(other) => return Err(other),
    };
    let bytes = STANDARD
        .decode(encoded_key)
        .map_err(|_| VaultError::Malformed)?;
    bytes.try_into().map_err(|_| VaultError::Malformed)
}
