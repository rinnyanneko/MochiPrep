//! Exchange-account bodies: `OkxConnectSchema` and `ArmSchema` in
//! `packages/shared/src/schemas.ts`.
use super::*;
use mochi_domain::{default_live_limits, AccountVenue, RiskSettings};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// `secret`: `z.string().trim().min(1).max(200)`.
const SECRET: StringRule = StringRule::trimmed(200);
const CONFIRMATION: StringRule = StringRule {
    max: Some(100),
    ..StringRule::PLAIN
};

/// `POST /api/exchange/okx/connect`. No `Debug`, so the key cannot reach a log, and the key is
/// wiped when the connect is done.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(rename_all = "camelCase")]
pub struct OkxConnectInput {
    pub api_key: String,
    pub secret_key: String,
    pub passphrase: String,
    #[zeroize(skip)]
    pub name: String,
    /// `OKX_DEMO` or `OKX_LIVE`: the schema refuses `PAPER`.
    #[zeroize(skip)]
    pub environment: AccountVenue,
    #[zeroize(skip)]
    pub limits: RiskSettings,
    #[zeroize(skip)]
    pub remember: bool,
}

#[derive(Debug, Deserialize)]
pub struct ArmInput {
    pub confirmation: String,
}

/// `OkxCredentialsSchema.extend({...})`: the credentials' keys first, then the extension's.
pub(super) fn okx_connect(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("apiKey", |v| check_string(v, &SECRET));
    fields.required("secretKey", |v| check_string(v, &SECRET));
    fields.required("passphrase", |v| check_string(v, &SECRET));
    fields.required("name", |v| check_string(v, &StringRule::trimmed(60)));
    fields.required("environment", |v| check_enum(v, &["OKX_DEMO", "OKX_LIVE"]));
    fields.defaulted(
        "limits",
        serde_json::to_value(default_live_limits()).expect("finite default limits"),
        ai::risk,
    );
    fields.defaulted("remember", json!(false), check_bool);
    fields.strict()
}

pub(super) fn arm(input: Input) -> Checked {
    let mut fields = Fields::new(input)?;
    fields.required("confirmation", |v| check_string(v, &CONFIRMATION));
    fields.strict()
}
