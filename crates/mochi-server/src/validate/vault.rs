//! Master-password bodies (`VaultCreateSchema` and friends in `packages/shared/src/schemas.ts`).
//! Passwords are taken as sent: no trim, since the vault NFC-normalises them when deriving.
use super::*;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// `masterPassword`: `z.string().min(10, 'Use at least 10 characters').max(200)`.
const MASTER_PASSWORD: StringRule = StringRule {
    min: Some(10),
    max: Some(200),
    min_message: Some("Use at least 10 characters"),
    ..StringRule::PLAIN
};
/// `z.string().min(1).max(200)`: any existing password, however short it was when set.
const PASSWORD: StringRule = StringRule {
    min: Some(1),
    max: Some(200),
    ..StringRule::PLAIN
};
const CONFIRMATION: StringRule = StringRule {
    max: Some(100),
    ..StringRule::PLAIN
};

// Passwords: no Debug, so they cannot reach a log, and wiped when the request is done.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct VaultCreateInput {
    pub password: String,
}
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct VaultPasswordInput {
    pub password: String,
}
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct VaultChangeInput {
    pub current: String,
    pub next: String,
}
#[derive(Debug, Deserialize)]
pub struct VaultResetInput {
    pub confirmation: String,
}

pub(super) fn vault_create(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("password", |v| check_string(v, &MASTER_PASSWORD));
    fields.strict()
}
pub(super) fn vault_password(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("password", |v| check_string(v, &PASSWORD));
    fields.strict()
}
pub(super) fn vault_change(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("current", |v| check_string(v, &PASSWORD));
    fields.required("next", |v| check_string(v, &MASTER_PASSWORD));
    fields.strict()
}
pub(super) fn vault_reset(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("confirmation", |v| check_string(v, &CONFIRMATION));
    fields.strict()
}
