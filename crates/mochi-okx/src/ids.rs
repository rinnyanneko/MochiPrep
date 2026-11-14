//! OKX ids and request signing, as `packages/exchanges/src/okx-client.ts` derives them.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// `BTCUSDT` -> `BTC-USDT-SWAP`. Only one trailing `USDT` is dropped, as Node's `/USDT$/`.
pub fn to_inst_id(symbol: &str) -> String {
    let base = symbol.strip_suffix("USDT").unwrap_or(symbol);
    format!("{base}-USDT-SWAP")
}

/// clOrdId allows only 32 alphanumerics. Hashing keeps it deterministic for any local key.
pub fn to_cl_ord_id(client_order_id: &str) -> String {
    hex32(client_order_id)
}

/// Distinct hash input from `to_cl_ord_id` so an order and its algo can never share an id.
pub fn to_algo_cl_ord_id(local_order_id: &str) -> String {
    hex32(&format!("algo:{local_order_id}"))
}

fn hex32(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.truncate(32);
    hex
}

/// base64(HMAC-SHA256(secret, timestamp + method + requestPath + body)); `request_path` includes
/// the query string.
pub fn sign(
    secret_key: &str,
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &str,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())
        .expect("HMAC takes a key of any length");
    mac.update(timestamp.as_bytes());
    mac.update(method.as_bytes());
    mac.update(request_path.as_bytes());
    mac.update(body.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

/// `new Date(ms).toISOString()`, the form OKX wants in `OK-ACCESS-TIMESTAMP`.
pub fn iso_string(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs())
    };
    format!(
        "{year_text}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1_000 % 60,
        in_day % 1_000
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}
