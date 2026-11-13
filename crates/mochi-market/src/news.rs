//! News headlines from RSS and Atom feeds: a port of `packages/market-data/src/news.ts`.
//!
//! Feed text is hostile. A headline ends up inside an AI prompt, so it is sanitised exactly as
//! Node does it, and the URL of every fetch, redirects included, must be public https.
//! `crates/golden/news.json` holds what Node makes of a corpus of hostile input and
//! `tests/golden_news.rs` checks this module against it.
//!
//! JS strings are UTF-16 and may hold lone surrogates (`&#xD800;` decodes to one). A Rust
//! `String` cannot, so a lone surrogate becomes U+FFFD. It takes the same one UTF-16 unit, so the
//! 200-unit title cut and every length still agree with Node; only that one character differs
//! (Node's JSON writes `"\ud800"`, which a browser shows as U+FFFD anyway). Entity decoding works
//! on UTF-16 units, so two entities that form a surrogate pair still make one character.
//!
//! `Date.parse` is ported from V8's date parser. One part is not: a date without a zone is local
//! time in Node, and this port reads it as UTC (see [`JsDate::local`]).

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::{join_all, FutureExt, Shared};
use mochi_domain::jsnum::{is_js_space, js_trim};
use mochi_domain::{NewsItem, NewsSource};
use regex::Regex;
use serde::Serialize;
use tokio::time::timeout;

use crate::okx_public::{tls_config, BoxFuture};

/// A cached set of headlines is this old before a read fetches the feeds again.
pub const REFRESH_MS: i64 = 5 * 60_000;
/// Node's `AbortSignal.timeout(8_000)` on each hop, the final body read included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// In UTF-16 units of the decoded text, as JS `text.length` counts.
pub const MAX_FEED_CHARS: usize = 3_000_000;
/// Every decoded UTF-16 unit takes at most 3 bytes of UTF-8 (a 4-byte character makes two
/// units, and an invalid sequence of up to 3 bytes makes one U+FFFD), plus a 3-byte BOM. So a
/// body longer than this is certainly over [`MAX_FEED_CHARS`] and need not be read to the end.
pub const MAX_BODY_BYTES: usize = 3 * MAX_FEED_CHARS + 6;
pub const MAX_REDIRECTS: usize = 3;
pub const DEFAULT_NEWS_LIMIT: usize = 8;
pub const USER_AGENT: &str = "MochiPrep/0.1 (RSS reader)";
pub const ACCEPT: &str = "application/rss+xml, application/atom+xml, application/xml, text/xml";

const MAX_TITLE_CHARS: usize = 200;
const MAX_URL_CHARS: usize = 500;
const MAX_AGE_MS: i64 = 24 * 3_600_000;
const PER_SOURCE_LIMIT: usize = 3;

/// What [`validate_feed_url`] says of a private host; a resolve-time refusal reads the same.
pub const PUBLIC_HOST_PROBLEM: &str = "Feeds must be on a public host";

/// What fetch's `AbortSignal.timeout` rejects with.
const TIMEOUT_TEXT: &str = "The operation was aborted due to timeout";

/// The built-in feeds. Only `enabled` is ever saved for them, so a corrected URL here reaches
/// existing installs too.
pub fn default_news_sources() -> Vec<NewsSource> {
    let source = |id: &str, name: &str, url: &str, enabled: bool| NewsSource {
        id: id.to_string(),
        name: name.to_string(),
        url: url.to_string(),
        enabled,
        built_in: true,
    };
    vec![
        source(
            "coindesk",
            "CoinDesk",
            "https://www.coindesk.com/arc/outboundfeeds/rss",
            true,
        ),
        source(
            "cointelegraph",
            "Cointelegraph",
            "https://cointelegraph.com/rss",
            true,
        ),
        source("decrypt", "Decrypt", "https://decrypt.co/feed", true),
        source(
            "theblock",
            "The Block",
            "https://www.theblock.co/rss.xml",
            true,
        ),
        source(
            "bitcoinmagazine",
            "Bitcoin Magazine",
            "https://bitcoinmagazine.com/feed",
            false,
        ),
        source(
            "cryptoslate",
            "CryptoSlate",
            "https://cryptoslate.com/feed/",
            false,
        ),
    ]
}

/// `COIN_NAMES`: the words besides the ticker that mean this coin.
fn coin_names(base_currency: &str) -> &'static [&'static str] {
    match base_currency {
        "BTC" => &["bitcoin"],
        "ETH" => &["ethereum", "ether"],
        "SOL" => &["solana"],
        "DOGE" => &["dogecoin"],
        "XRP" => &["ripple"],
        "ADA" => &["cardano"],
        "BNB" => &["binance coin"],
        "LTC" => &["litecoin"],
        "AVAX" => &["avalanche"],
        "SHIB" => &["shiba inu"],
        _ => &[],
    }
}

// ---- UTF-16 ---------------------------------------------------------------------------------

/// Use UTF-16 units so lengths and slicing follow the Node sanitizer.
fn to_units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// Lone surrogates become U+FFFD; pairs, even ones made of two entities, become one character.
fn from_units(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// JS `text.slice(0, max)`. A pair cut in half leaves its high surrogate in JS, which here, as
/// everywhere, is U+FFFD.
fn slice_units(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let width = c.len_utf16();
        if used + width > max {
            if used < max {
                out.push('\u{FFFD}');
            }
            break;
        }
        out.push(c);
        used += width;
    }
    out
}

/// Recognize JavaScript whitespace without treating a lone surrogate as a character.
fn is_space_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(is_js_space)
}

/// JS `trim` over UTF-16 units. Every JS space is a single BMP unit.
fn trim_units(units: &[u16]) -> &[u16] {
    let start = units
        .iter()
        .position(|u| !is_space_unit(*u))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|u| !is_space_unit(*u))
        .map_or(start, |i| i + 1);
    &units[start..end]
}

/// The first `needle` (ASCII) in `hay` at or after `from`.
fn find_units(hay: &[u16], from: usize, needle: &[u8]) -> Option<usize> {
    if from > hay.len() || needle.len() > hay.len() - from {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| {
        hay[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|(u, b)| *u == u16::from(*b))
    })
}

// ---- sanitising and parsing -----------------------------------------------------------------

/// `/[\p{Cc}\p{Cf}<>]/gu`: control and format characters (zero-width, bidi overrides, BOM,
/// soft hyphen, tag characters) and angle brackets.
static UNSAFE_CHAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{Cc}\p{Cf}<>]").expect("a valid pattern"));
/// `/<[^>]*>/g`: markup inside a title.
static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("a valid pattern"));

/// Headlines reach a model, so they are hostile until proven otherwise. Angle brackets go so a
/// title cannot close the block it sits in, and format characters so nothing can hide in it.
pub fn sanitize_headline(text: &str) -> String {
    let spaced = UNSAFE_CHAR.replace_all(text, " ");
    // `.replace(/\s+/g, ' ')`; JS `\s` is exactly the set `trim` removes.
    let mut collapsed = String::with_capacity(spaced.len());
    let mut in_space = false;
    for c in spaced.chars() {
        if is_js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    slice_units(js_trim(&collapsed), MAX_TITLE_CHARS)
}

/// One `&...;` entity at `at` (which holds `&`), as `/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi` matches
/// it: where it ends and what replaces it. Letters and digits are ASCII only, as in a JS regex
/// without the `u` flag.
fn entity_at(units: &[u16], at: usize) -> Option<(usize, Vec<u16>)> {
    let unit = |i: usize| units.get(i).copied().unwrap_or(0);
    let is = |u: u16, test: fn(&u8) -> bool| u8::try_from(u).ok().as_ref().is_some_and(test);
    let run = |from: usize, test: fn(&u8) -> bool| {
        (from..units.len())
            .find(|&i| !is(units[i], test))
            .unwrap_or(units.len())
    };
    let semicolon = u16::from(b';');
    if unit(at + 1) == u16::from(b'#') {
        // `#x` hex first; if that fails, `#\d+` cannot match at an `x` either.
        let (digits_from, radix, test): (usize, u32, fn(&u8) -> bool) =
            if matches!(unit(at + 2), 0x78 | 0x58) {
                (at + 3, 16, u8::is_ascii_hexdigit)
            } else {
                (at + 2, 10, u8::is_ascii_digit)
            };
        let end = run(digits_from, test);
        if end == digits_from || unit(end) != semicolon {
            return None;
        }
        // `parseInt` of a long run is a huge number, which fails the range check below.
        let point = units[digits_from..end].iter().fold(0u64, |acc, u| {
            let digit = char::from_u32(u32::from(*u))
                .and_then(|c| c.to_digit(radix))
                .unwrap_or(0);
            acc.saturating_mul(u64::from(radix))
                .saturating_add(u64::from(digit))
        });
        let replacement = if point <= 0x10FFFF {
            code_point_units(point as u32)
        } else {
            Vec::new()
        };
        return Some((end + 1, replacement));
    }
    let end = run(at + 1, u8::is_ascii_alphabetic);
    if end == at + 1 || unit(end) != semicolon {
        return None;
    }
    let name: String = from_units(&units[at + 1..end]).to_ascii_lowercase();
    let replacement = match name.as_str() {
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "amp" => "&",
        // Node looks the name up in a plain object, so `&constructor;` finds
        // `Object.prototype.constructor` and the replace callback writes it as text.
        "constructor" => "function Object() { [native code] }",
        // Unknown names stay as they were.
        _ => return Some((end + 1, units[at..=end].to_vec())),
    };
    Some((end + 1, to_units(replacement)))
}

/// `String.fromCodePoint`, which accepts a surrogate code point as one unit.
fn code_point_units(point: u32) -> Vec<u16> {
    if point < 0x10000 {
        vec![point as u16]
    } else {
        let offset = point - 0x10000;
        vec![
            0xD800 + (offset >> 10) as u16,
            0xDC00 + (offset & 0x3FF) as u16,
        ]
    }
}

/// `decode`: CDATA unwrapped, then entities decoded, over UTF-16 units as in JS.
fn decode(raw: &[u16]) -> Vec<u16> {
    // `/<!\[CDATA\[([\s\S]*?)\]\]>/g`. A start without a later `]]>` ends the matching: no
    // later start can have one either.
    let mut unwrapped = Vec::with_capacity(raw.len());
    let mut from = 0;
    while let Some(start) = find_units(raw, from, b"<![CDATA[") {
        let body = start + 9;
        let Some(end) = find_units(raw, body, b"]]>") else {
            break;
        };
        unwrapped.extend_from_slice(&raw[from..start]);
        unwrapped.extend_from_slice(&raw[body..end]);
        from = end + 3;
    }
    unwrapped.extend_from_slice(&raw[from..]);

    let mut out = Vec::with_capacity(unwrapped.len());
    let mut i = 0;
    while i < unwrapped.len() {
        if unwrapped[i] == u16::from(b'&') {
            if let Some((end, replacement)) = entity_at(&unwrapped, i) {
                out.extend(replacement);
                i = end;
                continue;
            }
        }
        out.push(unwrapped[i]);
        i += 1;
    }
    out
}

/// `new RegExp('<name(?:\\s[^>]*)?>([\\s\\S]*?)</name>', 'i').exec(block)`: the raw content of
/// the first element called `name`. `lower` is `block` with ASCII lowercased, which keeps every
/// byte offset; the `i` flag folds ASCII only, as JS does without `u`.
fn element_content<'a>(block: &'a str, lower: &str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&open) {
        let start = from + found;
        from = start + 1;
        let after = start + open.len();
        let content_start = match block[after..].chars().next() {
            Some('>') => after + 1,
            Some(c) if is_js_space(c) => match block[after..].find('>') {
                Some(gt) => after + gt + 1,
                None => continue,
            },
            _ => continue,
        };
        // Without a closer after this start there is none after a later one either.
        let end = content_start + lower[content_start..].find(&close)?;
        return Some(&block[content_start..end]);
    }
    None
}

/// `tag`: the decoded, trimmed content of the first of `names` the block has, or empty.
fn tag(block: &str, lower: &str, names: &[&str]) -> Vec<u16> {
    for name in names {
        if let Some(content) = element_content(block, lower, name) {
            return trim_units(&decode(&to_units(content))).to_vec();
        }
    }
    Vec::new()
}

/// `/<link[^>]*href="([^"]+)"/i`. The greedy `[^>]*` makes the last `href="` before the tag's
/// first `>` win, and the quoted value may run past that `>`.
fn href_attr<'a>(block: &'a str, lower: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(found) = lower[from..].find("<link") {
        let start = from + found;
        from = start + 1;
        let run_start = start + 5;
        let run_end = lower[run_start..]
            .find('>')
            .map_or(lower.len(), |gt| run_start + gt);
        for at in (run_start..run_end).rev() {
            if !lower.as_bytes()[at..].starts_with(b"href=\"") {
                continue;
            }
            let value = at + 6;
            if let Some(quote) = lower[value..].find('"') {
                if quote > 0 {
                    return Some(&block[value..value + quote]);
                }
            }
        }
        // Nested openers before this delimiter search a suffix of the same failed run.
        from = run_end.max(from);
    }
    None
}

/// `/^https?:\/\//i`, cut to 500 units: never a `javascript:` link.
fn web_url(text: &str) -> String {
    let bytes = text.as_bytes();
    let starts = |prefix: &[u8]| {
        bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    if starts(b"http://") || starts(b"https://") {
        slice_units(text, MAX_URL_CHARS)
    } else {
        String::new()
    }
}

/// The `<item>` and `<entry>` blocks: `/<(item|entry)[\s>][\s\S]*?<\/\1>/gi`.
fn feed_blocks(xml: &str) -> Vec<&str> {
    let lower = xml.to_ascii_lowercase();
    let opens = ["<item", "<entry"];
    let closes = ["</item>", "</entry>"];
    // The next `<item` / `<entry` at or after `pos`, found once and reused until `pos` passes
    // it, so the scan stays linear; `None` means there is none left. A name whose closer is
    // missing after some start has none after any later start either, so it is dropped too.
    let find_from = |name: usize, from: usize| lower[from..].find(opens[name]).map(|at| from + at);
    let mut next = [find_from(0, 0), find_from(1, 0)];
    let mut blocks = Vec::new();
    let mut pos = 0;
    loop {
        for (name, at) in next.iter_mut().enumerate() {
            if at.is_some_and(|found| found < pos) {
                *at = find_from(name, pos);
            }
        }
        let candidate = (0..2)
            .filter_map(|name| next[name].map(|at| (at, name)))
            .min();
        let Some((start, which)) = candidate else {
            return blocks;
        };
        let after = start + opens[which].len();
        let body = match xml[after..].chars().next() {
            Some(c) if c == '>' || is_js_space(c) => after + c.len_utf8(),
            _ => {
                pos = start + 1;
                continue;
            }
        };
        match lower[body..].find(closes[which]) {
            Some(at) => {
                let end = body + at + closes[which].len();
                blocks.push(&xml[start..end]);
                pos = end;
            }
            None => {
                next[which] = None;
                pos = start + 1;
            }
        }
    }
}

/// Reads RSS and Atom. Only headlines are kept: bodies are long, costly, and more to attack with.
pub fn parse_feed(xml: &str, source: &str) -> Vec<NewsItem> {
    feed_blocks(xml)
        .into_iter()
        .filter_map(|block| {
            let lower = block.to_ascii_lowercase();
            let raw_title = from_units(&tag(block, &lower, &["title"]));
            let title = sanitize_headline(&MARKUP.replace_all(&raw_title, " "));
            let date = tag(
                block,
                &lower,
                &["pubdate", "published", "updated", "dc:date"],
            );
            let published_at = parse_js_date(&from_units(&date))?.ms;
            if title.is_empty() {
                return None;
            }
            let mut link = tag(block, &lower, &["link"]);
            if link.is_empty() {
                link = href_attr(block, &lower).map(to_units).unwrap_or_default();
            }
            // `tag` has decoded it once already; Node decodes it again, and so does this.
            let url = web_url(&from_units(&decode(&link)));
            Some(NewsItem {
                id: format!("{source}:{title}"),
                source: source.to_string(),
                title,
                url,
                published_at,
            })
        })
        .collect()
}

fn ipv4_public(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    // This net, private, loopback, and multicast and reserved (255.255.255.255 included).
    let special = a == 0 || a == 10 || a == 127 || a >= 224;
    let refused = special
        // 100.64/10 carrier-grade NAT
        || (a == 100 && b & 0xc0 == 64)
        // link-local, cloud metadata
        || (a == 169 && b == 254)
        || (a == 172 && b & 0xf0 == 16)
        || (a == 192 && b == 168)
        // 192.0.0/24 protocol assignments
        || (a == 192 && b == 0 && c == 0)
        // 198.18/15 benchmarking
        || (a == 198 && b & 0xfe == 18);
    !refused
}

/// Whether a feed may be fetched from this address. The backend runs on the owner's machine, so
/// anything that reaches it, its LAN, a cloud metadata service or a multicast group is refused.
/// `isPublicAddress` in packages/market-data/src/public-address.ts; `crates/golden/news.json`
/// holds what Node answers for a corpus of addresses.
pub fn is_public_address(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V4(v4) => return ipv4_public(v4),
        IpAddr::V6(v6) => v6,
    };
    let g = ip.segments();
    // :: and ::1
    if g == [0; 8] || g == [0, 0, 0, 0, 0, 0, 0, 1] {
        return false;
    }
    // fc00::/7 unique local, fe80::/10 link-local, fec0::/10 site-local (deprecated, still
    // routable inside a site), ff00::/8 multicast
    if g[0] & 0xfe00 == 0xfc00
        || g[0] & 0xffc0 == 0xfe80
        || g[0] & 0xffc0 == 0xfec0
        || g[0] & 0xff00 == 0xff00
    {
        return false;
    }
    // ::ffff:0:0/96 (IPv4-mapped) and 64:ff9b::/96 (NAT64) carry an IPv4 address in the last 32
    // bits, and a connection to one reaches that IPv4 address, so it is judged by the IPv4 rules.
    let mapped = g[..5] == [0; 5] && g[5] == 0xffff;
    let nat64 = g[0] == 0x64 && g[1] == 0xff9b && g[2..6] == [0; 4];
    if mapped || nat64 {
        let [.., a, b, c, d] = ip.octets();
        return ipv4_public(Ipv4Addr::new(a, b, c, d));
    }
    // 64:ff9b:1::/48 local-use NAT64
    if g[0] == 0x64 && g[1] == 0xff9b && g[2] == 1 {
        return false;
    }
    // IPv4-compatible ::/96 (deprecated) holds an IPv4 address in its low 32 bits; :: and ::1
    // were refused above.
    if g[..6] == [0; 6] {
        let [.., a, b, c, d] = ip.octets();
        return ipv4_public(Ipv4Addr::new(a, b, c, d));
    }
    // 6to4 2002::/16 tunnels to the IPv4 address in bits 16-48.
    if g[0] == 0x2002 {
        let [_, _, a, b, c, d, ..] = ip.octets();
        return ipv4_public(Ipv4Addr::new(a, b, c, d));
    }
    true
}

/// Refuses anything but public https, because the backend fetches this URL on the user's
/// machine. Only the text is checked here; [`NetFeedTransport`] checks what the name resolves to
/// when it connects.
pub fn validate_feed_url(text: &str) -> Option<&'static str> {
    let Ok(url) = url::Url::parse(text) else {
        return Some("Not a valid URL");
    };
    if url.scheme() != "https" {
        return Some("Feeds must use https");
    }
    // Credentials in a feed URL would be sent to whichever host it points at.
    if !url.username().is_empty() || url.password().is_some() {
        return Some("Feeds must not include a username or password");
    }
    let lowered = url.host_str().unwrap_or("").to_lowercase();
    // `.replace(/^\[|\]$/g, '')`
    let host = lowered.strip_prefix('[').unwrap_or(&lowered);
    let host = host.strip_suffix(']').unwrap_or(host);
    // DNS root dots do not change whether the hostname is local.
    let host = host.trim_end_matches('.');
    let private_172 = host.strip_prefix("172.").is_some_and(|rest| {
        let b = rest.as_bytes();
        b.len() >= 3
            && b[2] == b'.'
            && matches!(
                (b[0], b[1]),
                (b'1', b'6'..=b'9') | (b'2', b'0'..=b'9') | (b'3', b'0'..=b'1')
            )
    });
    let is_private = host == "localhost"
        || host.ends_with(".local")
        || host.ends_with(".localhost")
        || !host.contains('.')
        || ["127.", "10.", "0.", "192.168.", "169.254."]
            .iter()
            .any(|prefix| host.starts_with(prefix))
        || private_172
        || host.contains(':')
        // The URL parser writes every IPv4 form (hex, octal, a bare number) as a dotted quad. An
        // IP literal is never resolved, so this is the only check it gets.
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| !ipv4_public(ip));
    is_private.then_some(PUBLIC_HOST_PROBLEM)
}

/// Whether `word` occurs in `hay` with no `is_word` byte right before or after it. A byte of a
/// non-ASCII character is never a word byte, as a non-ASCII unit never is in a JS regex class.
fn bounded(hay: &str, word: &str, is_word: fn(u8) -> bool) -> bool {
    let bytes = hay.as_bytes();
    if word.is_empty() {
        return false;
    }
    (0..bytes.len()).any(|at| {
        bytes[at..].starts_with(word.as_bytes())
            && (at == 0 || !is_word(bytes[at - 1]))
            && bytes
                .get(at + word.len())
                .is_none_or(|after| !is_word(*after))
    })
}

/// The ticker on non-alphanumeric boundaries, or one of the coin's names as a whole word in the
/// lowercased title (JS `\b`, which knows only ASCII word characters).
pub fn mentions_coin(title: &str, base_currency: &str) -> bool {
    let ticker = bounded(title, base_currency, |b| b.is_ascii_alphanumeric());
    let lower = title.to_lowercase();
    ticker
        || coin_names(base_currency)
            .iter()
            .any(|name| bounded(&lower, name, |b| b.is_ascii_alphanumeric() || b == b'_'))
}

/// Newest first, this coin before general news, with a cap per source for variety. Nothing from
/// the future and nothing older than a day.
pub fn select_news(
    items: &[NewsItem],
    base_currency: &str,
    now: i64,
    limit: usize,
) -> Vec<NewsItem> {
    let mut recent: Vec<&NewsItem> = items
        .iter()
        .filter(|item| item.published_at <= now && now - item.published_at <= MAX_AGE_MS)
        .collect();
    // Stable, like Array.prototype.sort.
    recent.sort_by_key(|item| std::cmp::Reverse(item.published_at));
    let (about, rest): (Vec<&NewsItem>, Vec<&NewsItem>) = recent
        .into_iter()
        .partition(|item| mentions_coin(&item.title, base_currency));
    let mut per_source: Vec<(&str, usize)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut chosen = Vec::new();
    for item in about.into_iter().chain(rest) {
        let key = item.title.to_lowercase();
        let used = per_source
            .iter()
            .find(|(source, _)| *source == item.source)
            .map_or(0, |(_, n)| *n);
        if used >= PER_SOURCE_LIMIT || seen.contains(&key) {
            continue;
        }
        match per_source
            .iter_mut()
            .find(|(source, _)| *source == item.source)
        {
            Some(entry) => entry.1 += 1,
            None => per_source.push((&item.source, 1)),
        }
        seen.push(key);
        chosen.push(item.clone());
        // As Node: compared after the push, so a limit of 0 never stops early.
        if chosen.len() == limit {
            break;
        }
    }
    chosen
}

// ---- Date.parse -----------------------------------------------------------------------------
// A port of V8's DateParser (src/date/dateparser.h, dateparser-inl.h, dateparser.cc): the ES5
// ISO format first, then the legacy grammar browsers share. It reads UTF-16 units, so a
// character outside the BMP counts as two word units, as in V8.

/// A parsed date: ms since the epoch, reading local time as UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsDate {
    pub ms: i64,
    /// The text gave no zone, so Node would read it in the machine's local time zone. This port
    /// has no time zone database and reads it as UTC; on a machine set to UTC the two agree.
    pub local: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeywordKind {
    Invalid,
    MonthName,
    AmPm,
    TimeZoneName,
    TimeSeparator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Number {
        value: i32,
        length: usize,
    },
    Symbol(u8),
    WhiteSpace,
    Keyword {
        kind: KeywordKind,
        value: i32,
        length: usize,
    },
    Unknown,
    End,
    Invalid,
}

impl Token {
    /// Return a numeric token value; non-numeric tokens use V8's zero sentinel.
    fn number(self) -> i32 {
        match self {
            Token::Number { value, .. } => value,
            _ => 0,
        }
    }
    fn is_number(self) -> bool {
        matches!(self, Token::Number { .. })
    }
    /// Check digit width as well as token kind for fixed-width ISO date components.
    fn is_fixed_number(self, n: usize) -> bool {
        matches!(self, Token::Number { length, .. } if length == n)
    }
    fn is_sign(self) -> bool {
        matches!(self, Token::Symbol(b'+' | b'-'))
    }
    /// `44 - symbol`: 1 for `+`, -1 for `-`.
    fn sign(self) -> i32 {
        match self {
            Token::Symbol(b'-') => -1,
            _ => 1,
        }
    }
    fn is_kind(self, wanted: KeywordKind) -> bool {
        matches!(self, Token::Keyword { kind, .. } if kind == wanted)
    }
    /// Distinguish the one-letter UTC suffix from other zero-offset zone names.
    fn is_keyword_z(self) -> bool {
        matches!(
            self,
            Token::Keyword {
                kind: KeywordKind::TimeZoneName,
                value: 0,
                length: 1
            }
        )
    }
}

/// V8's keyword table: the first three letters, lowercased; only month names may be longer.
const KEYWORDS: &[(&[u8; 3], KeywordKind, i32)] = &[
    (b"jan", KeywordKind::MonthName, 1),
    (b"feb", KeywordKind::MonthName, 2),
    (b"mar", KeywordKind::MonthName, 3),
    (b"apr", KeywordKind::MonthName, 4),
    (b"may", KeywordKind::MonthName, 5),
    (b"jun", KeywordKind::MonthName, 6),
    (b"jul", KeywordKind::MonthName, 7),
    (b"aug", KeywordKind::MonthName, 8),
    (b"sep", KeywordKind::MonthName, 9),
    (b"oct", KeywordKind::MonthName, 10),
    (b"nov", KeywordKind::MonthName, 11),
    (b"dec", KeywordKind::MonthName, 12),
    (b"am\0", KeywordKind::AmPm, 0),
    (b"pm\0", KeywordKind::AmPm, 12),
    (b"ut\0", KeywordKind::TimeZoneName, 0),
    (b"utc", KeywordKind::TimeZoneName, 0),
    (b"z\0\0", KeywordKind::TimeZoneName, 0),
    (b"gmt", KeywordKind::TimeZoneName, 0),
    (b"cdt", KeywordKind::TimeZoneName, -5),
    (b"cst", KeywordKind::TimeZoneName, -6),
    (b"edt", KeywordKind::TimeZoneName, -4),
    (b"est", KeywordKind::TimeZoneName, -5),
    (b"mdt", KeywordKind::TimeZoneName, -6),
    (b"mst", KeywordKind::TimeZoneName, -7),
    (b"pdt", KeywordKind::TimeZoneName, -7),
    (b"pst", KeywordKind::TimeZoneName, -8),
    (b"t\0\0", KeywordKind::TimeSeparator, 0),
];

/// Match V8's three-letter keyword table, allowing long spellings only for months.
fn keyword(prefix: [u32; 3], length: usize) -> (KeywordKind, i32) {
    for (word, kind, value) in KEYWORDS {
        let same = prefix
            .iter()
            .zip(word.iter())
            .all(|(p, w)| *p == u32::from(*w));
        if same && (length <= 3 || *kind == KeywordKind::MonthName) {
            return (*kind, *value);
        }
    }
    (KeywordKind::Invalid, 0)
}

/// V8's InputReader and DateStringTokenizer. A NUL unit reads as the end of the input.
struct Scanner<'a> {
    units: &'a [u16],
    index: usize,
    ch: u16,
    next: Token,
}

impl<'a> Scanner<'a> {
    /// Prime one-token lookahead before the date parser consumes input.
    fn new(units: &'a [u16]) -> Self {
        let mut scanner = Scanner {
            units,
            index: 0,
            ch: 0,
            next: Token::End,
        };
        scanner.advance();
        scanner.next = scanner.scan();
        scanner
    }

    /// Read the next UTF-16 unit, using NUL as the end-of-input sentinel.
    fn advance(&mut self) {
        self.ch = self.units.get(self.index).copied().unwrap_or(0);
        self.index += 1;
    }

    fn is_digit(&self) -> bool {
        (u16::from(b'0')..=u16::from(b'9')).contains(&self.ch)
    }

    fn is_space(&self) -> bool {
        is_space_unit(self.ch)
    }

    /// Digits as a number; leading zeros skipped, and only the first 9 significant digits count.
    fn read_number(&mut self) -> i32 {
        while self.ch == u16::from(b'0') {
            self.advance();
        }
        let mut n: i32 = 0;
        let mut digits = 0;
        while self.is_digit() {
            if digits < 9 {
                n = n * 10 + i32::from(self.ch - u16::from(b'0'));
            }
            digits += 1;
            self.advance();
        }
        n
    }

    /// Tokenize numbers, keywords and separators while skipping nested parenthesized text.
    fn scan(&mut self) -> Token {
        let start = self.index;
        if self.ch == 0 {
            return Token::End;
        }
        if self.is_digit() {
            let value = self.read_number();
            return Token::Number {
                value,
                length: self.index - start,
            };
        }
        for symbol in *b":-+.)" {
            if self.ch == u16::from(symbol) {
                self.advance();
                return Token::Symbol(symbol);
            }
        }
        if self.ch >= u16::from(b'A') && !self.is_space() {
            let mut prefix = [0u32; 3];
            let mut length = 0;
            while self.ch >= u16::from(b'A') && !self.is_space() {
                if length < 3 {
                    // V8's AsciiAlphaToLower, applied to every unit.
                    prefix[length] = u32::from(self.ch | 0x20);
                }
                length += 1;
                self.advance();
            }
            let (kind, value) = keyword(prefix, length);
            return Token::Keyword {
                kind,
                value,
                length,
            };
        }
        if self.is_space() {
            self.advance();
            return Token::WhiteSpace;
        }
        if self.ch == u16::from(b'(') {
            // Parenthesised text, nested, is skipped.
            let mut balance = 0;
            loop {
                if self.ch == u16::from(b')') {
                    balance -= 1;
                } else if self.ch == u16::from(b'(') {
                    balance += 1;
                }
                self.advance();
                if balance <= 0 || self.ch == 0 {
                    break;
                }
            }
            return Token::Unknown;
        }
        self.advance();
        Token::Unknown
    }

    /// Consume lookahead and scan its replacement without rewinding the input.
    fn next_token(&mut self) -> Token {
        let token = self.next;
        self.next = self.scan();
        token
    }

    fn peek(&self) -> Token {
        self.next
    }

    /// Consume the requested delimiter only when it is the current lookahead.
    fn skip_symbol(&mut self, symbol: u8) -> bool {
        if self.next == Token::Symbol(symbol) {
            self.next_token();
            true
        } else {
            false
        }
    }
}

fn is_month(x: i32) -> bool {
    (1..=12).contains(&x)
}
fn is_day(x: i32) -> bool {
    (1..=31).contains(&x)
}
fn is_hour(x: i32) -> bool {
    (0..=23).contains(&x)
}
fn is_minute(x: i32) -> bool {
    (0..=59).contains(&x)
}
fn is_hour12(x: i32) -> bool {
    (0..=12).contains(&x)
}
fn is_millisecond(x: i32) -> bool {
    (0..=999).contains(&x)
}

/// The first three significant digits of a fraction, from its value and digit count.
fn read_milliseconds(token: Token) -> i32 {
    let Token::Number { value, length } = token else {
        return 0;
    };
    match length {
        1 => value * 100,
        2 => value * 10,
        3 => value,
        _ => value / 10i32.pow(length.min(9) as u32 - 3),
    }
}

#[derive(Default)]
struct DayComposer {
    comp: [i32; 3],
    index: usize,
    named_month: Option<i32>,
    is_iso_date: bool,
}

impl DayComposer {
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    /// Append a date or time component only while its fixed-size composer has room.
    fn add(&mut self, n: i32) -> bool {
        if self.index < 3 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    /// (year, month 1..12, day). Missing parts are 1, a missing year is 2001's `1`.
    fn write(&mut self) -> Option<(i32, i32, i32)> {
        if self.index < 1 {
            return None;
        }
        while self.index < 3 {
            self.comp[self.index] = 1;
            self.index += 1;
        }
        let [a, b, c] = self.comp;
        let (mut year, month, day) = match self.named_month {
            None if self.is_iso_date || !is_day(a) => (a, b, c),
            None => (c, a, b),
            Some(month) if !is_day(a) => (a, month, b),
            Some(month) => (b, month, a),
        };
        if !self.is_iso_date {
            if (0..=49).contains(&year) {
                year += 2000;
            } else if (50..=99).contains(&year) {
                year += 1900;
            }
        }
        (is_month(month) && is_day(day)).then_some((year, month, day))
    }
}

#[derive(Default)]
struct TimeComposer {
    comp: [i32; 4],
    index: usize,
    hour_offset: Option<i32>,
}

impl TimeComposer {
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    /// Check whether this value can fill the next missing time or offset component.
    fn is_expecting(&self, n: i32) -> bool {
        (self.index == 1 && is_minute(n))
            || (self.index == 2 && is_minute(n))
            || (self.index == 3 && is_millisecond(n))
    }
    /// Append a date or time component only while its fixed-size composer has room.
    fn add(&mut self, n: i32) -> bool {
        if self.index < 4 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    /// Finish the time by padding all omitted components with zero.
    fn add_final(&mut self, n: i32) -> bool {
        if !self.add(n) {
            return false;
        }
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        true
    }
    /// ms into the day.
    fn write(&mut self) -> Option<i64> {
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        let [mut hour, minute, second, ms] = self.comp;
        if let Some(offset) = self.hour_offset {
            if !is_hour12(hour) {
                return None;
            }
            hour = hour % 12 + offset;
        }
        let valid = is_hour(hour) && is_minute(minute) && is_minute(second) && is_millisecond(ms);
        // 24:00:00.000 is the end of the day.
        if !valid && (hour != 24 || minute != 0 || second != 0 || ms != 0) {
            return None;
        }
        Some(
            i64::from(hour) * 3_600_000
                + i64::from(minute) * 60_000
                + i64::from(second) * 1000
                + i64::from(ms),
        )
    }
}

#[derive(Default)]
struct ZoneComposer {
    sign: Option<i32>,
    hour: Option<i32>,
    minute: Option<i32>,
}

impl ZoneComposer {
    /// Store an hour offset as sign and absolute components for later validation.
    fn set(&mut self, offset_hours: i32) {
        let sign = if offset_hours < 0 { -1 } else { 1 };
        self.sign = Some(sign);
        self.hour = Some(offset_hours * sign);
        self.minute = Some(0);
    }
    /// Check whether this value can fill the next missing time or offset component.
    fn is_expecting(&self, n: i32) -> bool {
        self.hour.is_some() && self.minute.is_none() && is_minute(n)
    }
    fn is_utc(&self) -> bool {
        self.hour == Some(0) && self.minute == Some(0)
    }
    fn is_empty(&self) -> bool {
        self.hour.is_none() && self.minute.is_none()
    }
    /// Seconds east of UTC, or `Ok(None)` for local time.
    fn write(&self) -> Result<Option<i64>, ()> {
        let Some(sign) = self.sign else {
            return Ok(None);
        };
        // V8 adds these as `unsigned`, which wraps, and refuses more than Smi::kMaxValue.
        let total = (self.hour.unwrap_or(0) as u32)
            .wrapping_mul(3600)
            .wrapping_add((self.minute.unwrap_or(0) as u32).wrapping_mul(60));
        if total > i32::MAX as u32 {
            return Err(());
        }
        Ok(Some(i64::from(sign) * i64::from(total)))
    }
}

/// `ParseES5DateTime`: the ISO format. Returns the first token it did not handle, `End` when it
/// read the whole text, or `Invalid`.
fn parse_es5(
    scanner: &mut Scanner,
    day: &mut DayComposer,
    time: &mut TimeComposer,
    zone: &mut ZoneComposer,
) -> Token {
    if scanner.peek().is_sign() {
        let sign_token = scanner.next_token();
        if !scanner.peek().is_fixed_number(6) {
            return sign_token;
        }
        let sign = sign_token.sign();
        let year = scanner.next_token().number();
        if sign < 0 && year == 0 {
            return sign_token;
        }
        day.add(sign * year);
    } else if scanner.peek().is_fixed_number(4) {
        day.add(scanner.next_token().number());
    } else {
        return scanner.next_token();
    }
    if scanner.skip_symbol(b'-') {
        if !scanner.peek().is_fixed_number(2) || !is_month(scanner.peek().number()) {
            return scanner.next_token();
        }
        day.add(scanner.next_token().number());
        if scanner.skip_symbol(b'-') {
            if !scanner.peek().is_fixed_number(2) || !is_day(scanner.peek().number()) {
                return scanner.next_token();
            }
            day.add(scanner.next_token().number());
        }
    }
    if !scanner.peek().is_kind(KeywordKind::TimeSeparator) {
        if scanner.peek() != Token::End {
            return scanner.next_token();
        }
    } else if parse_iso_time(scanner, time, zone).is_none() {
        return Token::Invalid;
    }
    // A date-only form is UTC; a date-time without an offset is local.
    if zone.is_empty() && time.is_empty() {
        zone.set(0);
    }
    day.is_iso_date = true;
    Token::End
}

/// Read the ISO time after `T`; 24 is allowed only for exactly midnight.
fn parse_iso_time(
    scanner: &mut Scanner,
    time: &mut TimeComposer,
    zone: &mut ZoneComposer,
) -> Option<()> {
    scanner.next_token();
    let hour = scanner.peek();
    if !hour.is_fixed_number(2) || !(0..=24).contains(&hour.number()) {
        return None;
    }
    // 24:00[:00[.000]] only.
    let hour_is_24 = hour.number() == 24;
    time.add(scanner.next_token().number());
    if !scanner.skip_symbol(b':') {
        return None;
    }
    let minute = scanner.peek();
    if !minute.is_fixed_number(2)
        || !is_minute(minute.number())
        || (hour_is_24 && minute.number() > 0)
    {
        return None;
    }
    time.add(scanner.next_token().number());
    if scanner.skip_symbol(b':') {
        let second = scanner.peek();
        if !second.is_fixed_number(2)
            || !is_minute(second.number())
            || (hour_is_24 && second.number() > 0)
        {
            return None;
        }
        time.add(scanner.next_token().number());
        if scanner.skip_symbol(b'.') {
            let fraction = scanner.peek();
            if !fraction.is_number() || (hour_is_24 && fraction.number() > 0) {
                return None;
            }
            time.add(read_milliseconds(scanner.next_token()));
        }
    }
    parse_iso_zone(scanner, zone)?;
    if scanner.peek() != Token::End {
        return None;
    }
    Some(())
}

/// Read an optional ISO offset, accepting both hh:mm and V8's hhmm extension.
fn parse_iso_zone(scanner: &mut Scanner, zone: &mut ZoneComposer) -> Option<()> {
    if scanner.peek().is_keyword_z() {
        scanner.next_token();
        zone.set(0);
    } else if scanner.peek().is_sign() {
        zone.sign = Some(scanner.next_token().sign());
        if scanner.peek().is_fixed_number(4) {
            // The hhmm extension.
            let hour_minute = scanner.next_token().number();
            let (hour, minute) = (hour_minute / 100, hour_minute % 100);
            if !is_hour(hour) || !is_minute(minute) {
                return None;
            }
            zone.hour = Some(hour);
            zone.minute = Some(minute);
        } else {
            if !scanner.peek().is_fixed_number(2) || !is_hour(scanner.peek().number()) {
                return None;
            }
            zone.hour = Some(scanner.next_token().number());
            if !scanner.skip_symbol(b':') {
                return None;
            }
            if !scanner.peek().is_fixed_number(2) || !is_minute(scanner.peek().number()) {
                return None;
            }
            zone.minute = Some(scanner.next_token().number());
        }
    }
    Some(())
}

/// The legacy grammar, from the token the ISO parser stopped at. `None` when the text is not a
/// date.
fn parse_legacy(
    scanner: &mut Scanner,
    first: Token,
    day: &mut DayComposer,
    time: &mut TimeComposer,
    zone: &mut ZoneComposer,
) -> Option<()> {
    let mut has_read_number = !day.is_empty();
    let mut token = first;
    while token != Token::End {
        match token {
            Token::Number { value: n, .. } => {
                has_read_number = true;
                parse_legacy_number(scanner, n, day, time, zone)?;
            }
            Token::Keyword { kind, value, .. } => {
                if kind == KeywordKind::AmPm && !time.is_empty() {
                    time.hour_offset = Some(value);
                } else if kind == KeywordKind::MonthName {
                    day.named_month = Some(value);
                    scanner.skip_symbol(b'-');
                } else if kind == KeywordKind::TimeZoneName && has_read_number {
                    zone.set(value);
                } else {
                    // A word that means nothing is allowed only before the first number, and
                    // must not touch it.
                    if has_read_number || scanner.peek().is_number() {
                        return None;
                    }
                }
            }
            Token::Symbol(_) if token.is_sign() && (zone.is_utc() || !time.is_empty()) => {
                parse_legacy_zone(scanner, token, zone)?;
                has_read_number = true;
            }
            Token::Symbol(b'+' | b'-' | b')') if has_read_number => return None,
            Token::Invalid => return None,
            // Other symbols, spaces and anything else are ignored.
            _ => {}
        }
        token = scanner.next_token();
    }
    Some(())
}

/// Interpret a legacy number according to the following separator and composer state.
fn parse_legacy_number(
    scanner: &mut Scanner,
    n: i32,
    day: &mut DayComposer,
    time: &mut TimeComposer,
    zone: &mut ZoneComposer,
) -> Option<()> {
    if scanner.skip_symbol(b':') {
        if scanner.skip_symbol(b':') {
            // n::
            if !time.is_empty() {
                return None;
            }
            time.add(n);
            time.add(0);
        } else {
            // n:
            if !time.add(n) {
                return None;
            }
            if scanner.peek() == Token::Symbol(b'.') {
                scanner.next_token();
            }
        }
    } else if scanner.skip_symbol(b'.') && time.is_expecting(n) {
        time.add(n);
        if !scanner.peek().is_number() {
            return None;
        }
        let ms = read_milliseconds(scanner.next_token());
        if ms < 0 {
            return None;
        }
        time.add_final(ms);
    } else if zone.is_expecting(n) {
        zone.minute = Some(n);
    } else if time.is_expecting(n) {
        time.add_final(n);
        // The time must end at the end, a space, `Z` or a sign.
        let peek = scanner.peek();
        if peek != Token::End
            && peek != Token::WhiteSpace
            && !peek.is_keyword_z()
            && !peek.is_sign()
        {
            return None;
        }
    } else {
        if !day.add(n) {
            return None;
        }
        scanner.skip_symbol(b'-');
    }
    Some(())
}

/// Interpret a legacy signed offset, leaving a colon's minutes for the next token.
fn parse_legacy_zone(scanner: &mut Scanner, token: Token, zone: &mut ZoneComposer) -> Option<()> {
    // An offset, after UTC or a time.
    zone.sign = Some(token.sign());
    let (mut n, mut length) = (0, 0);
    if let Token::Number { value, length: l } = scanner.peek() {
        scanner.next_token();
        n = value;
        length = l;
    }
    if scanner.peek() == Token::Symbol(b':') {
        zone.hour = Some(n);
        zone.minute = None;
    } else if length == 1 || length == 2 {
        zone.hour = Some(n);
        zone.minute = Some(0);
    } else if length == 3 || length == 4 {
        zone.hour = Some(n / 100);
        zone.minute = Some(n % 100);
    } else {
        return None;
    }
    Some(())
}

/// Days from 1970-01-01 to the first of `month` (1..12) in `year`, proleptic Gregorian.
fn days_from_civil(year: i64, month: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// JS `Date.parse`: ms since the epoch, or `None` for NaN.
pub fn parse_js_date(text: &str) -> Option<JsDate> {
    let units = to_units(text);
    let mut scanner = Scanner::new(&units);
    let mut day = DayComposer::default();
    let mut time = TimeComposer::default();
    let mut zone = ZoneComposer::default();
    let unhandled = parse_es5(&mut scanner, &mut day, &mut time, &mut zone);
    if unhandled == Token::Invalid {
        return None;
    }
    parse_legacy(&mut scanner, unhandled, &mut day, &mut time, &mut zone)?;
    let (year, month, date) = day.write()?;
    let ms_of_day = time.write()?;
    let offset = zone.write().ok()?;
    // MakeDay refuses years past a million; TimeClip below refuses far less than that anyway.
    if !(-1_000_000..=1_000_000).contains(&year) {
        return None;
    }
    let days = days_from_civil(i64::from(year), i64::from(month)) + i64::from(date) - 1;
    let mut ms = days * 86_400_000 + ms_of_day;
    if let Some(seconds) = offset {
        ms -= seconds * 1000;
    }
    // TimeClip.
    if ms.abs() > 8_640_000_000_000_000 {
        return None;
    }
    Some(JsDate {
        ms,
        local: offset.is_none(),
    })
}

// ---- fetching -------------------------------------------------------------------------------

/// One response, as far as the news fetcher reads it.
#[derive(Debug, Clone, Default)]
pub struct FeedReply {
    pub status: u16,
    /// Every `Location` header, joined with ", " as fetch's `headers.get` does.
    pub location: Option<String>,
    /// Read only for a 2xx status, and at most [`MAX_BODY_BYTES`] of it.
    pub body: Vec<u8>,
    /// The body went on past [`MAX_BODY_BYTES`]; the rest was not read.
    pub truncated: bool,
}

/// How the news service reaches a feed.
pub trait FeedTransport: Send + Sync {
    /// One GET of exactly `url`, sending [`USER_AGENT`] and [`ACCEPT`]: no cookies, no proxy,
    /// and never a redirect followed, because each hop must be checked first. It is anonymous
    /// because [`validate_feed_url`] refuses a URL with a username or password before any hop
    /// gets here. Only a 2xx body is read; any other reply is dropped unread, which closes its
    /// connection. Errors are the text a failed `fetch` gives in Node.
    fn get(&self, url: String) -> BoxFuture<'static, Result<FeedReply, String>>;
}

/// Resolves a host name to its addresses: the system resolver, or a fake in tests.
pub type HostLookup =
    Arc<dyn Fn(String) -> BoxFuture<'static, io::Result<Vec<IpAddr>>> + Send + Sync>;

/// Why [`PublicOnlyResolver`] refused a name; found again among the request error's sources.
#[derive(Debug)]
struct NonPublicAddress;

impl std::fmt::Display for NonPublicAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(PUBLIC_HOST_PROBLEM)
    }
}

impl std::error::Error for NonPublicAddress {}

/// The news client's resolver. It refuses the whole name if any address it resolves to is not
/// public, and otherwise hands back only addresses it checked. reqwest connects to exactly what
/// this returns, with no second lookup, so a DNS answer that changes between the check and the
/// connection (rebinding) cannot reach a private address.
///
/// An IP-literal host never reaches a resolver. That is fine: [`validate_feed_url`], which runs
/// on every hop before the transport does, refuses every IPv6 literal and every non-public IPv4
/// literal.
struct PublicOnlyResolver {
    lookup: HostLookup,
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let found = (self.lookup)(name.as_str().to_string());
        Box::pin(async move {
            let ips = found.await?;
            if ips.iter().any(|ip| !is_public_address(*ip)) {
                return Err(Box::new(NonPublicAddress) as Box<dyn std::error::Error + Send + Sync>);
            }
            if ips.is_empty() {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no addresses").into());
            }
            // Port 0: reqwest puts in the URL's port.
            let addrs: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

fn system_lookup() -> HostLookup {
    Arc::new(|host: String| {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
            Ok(addrs.map(|addr| addr.ip()).collect())
        })
    })
}

/// Whether a request failed because the resolver refused a non-public address.
fn refused_non_public(error: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if current.is::<NonPublicAddress>() {
            return true;
        }
        source = current.source();
    }
    false
}

/// The real network, with rustls over the Mozilla root store, as the OKX feed uses, connecting
/// only to public addresses (see [`PublicOnlyResolver`]).
pub struct NetFeedTransport {
    client: reqwest::Client,
}

impl Default for NetFeedTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl NetFeedTransport {
    /// Build an HTTP client with automatic redirects disabled so each hop is validated.
    pub fn new() -> Self {
        Self::with_lookup(system_lookup())
    }

    /// As [`NetFeedTransport::new`], resolving names through `lookup`; tests pass a fake DNS.
    pub fn with_lookup(lookup: HostLookup) -> Self {
        let client = reqwest::Client::builder()
            .dns_resolver(PublicOnlyResolver { lookup })
            .use_preconfigured_tls((*tls_config()).clone())
            // Node's fetch ignores proxy variables; so does this.
            .no_proxy()
            // `redirect: 'manual'`: the fetcher validates every hop itself.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client with a preconfigured TLS config always builds");
        NetFeedTransport { client }
    }
}

impl FeedTransport for NetFeedTransport {
    /// Fetch one hop with fixed headers and a bounded body; never forward credentials.
    fn get(&self, url: String) -> BoxFuture<'static, Result<FeedReply, String>> {
        let request = self
            .client
            .get(&url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, ACCEPT);
        Box::pin(async move {
            // Node's undici says only "fetch failed". A refused address reads as the text
            // check's refusal does.
            let mut response = request.send().await.map_err(|error| {
                if refused_non_public(&error) {
                    format!("{PUBLIC_HOST_PROBLEM}: {url}")
                } else {
                    "fetch failed".to_string()
                }
            })?;
            let status = response.status().as_u16();
            let locations: Vec<String> = response
                .headers()
                .get_all(reqwest::header::LOCATION)
                .iter()
                // fetch reads header bytes as Latin-1.
                .map(|value| value.as_bytes().iter().map(|b| char::from(*b)).collect())
                .collect();
            let location = (!locations.is_empty()).then(|| locations.join(", "));
            let mut reply = FeedReply {
                status,
                location,
                ..FeedReply::default()
            };
            if (200..300).contains(&status) {
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| "fetch failed".to_string())?
                {
                    reply.body.extend_from_slice(&chunk);
                    if reply.body.len() > MAX_BODY_BYTES {
                        reply.body.truncate(MAX_BODY_BYTES);
                        reply.truncated = true;
                        break;
                    }
                }
            }
            Ok(reply)
        })
    }
}

/// Follows redirects by hand: each hop must pass the same check as the URL the user entered.
/// At most [`MAX_REDIRECTS`] are followed, and each hop has [`FETCH_TIMEOUT`].
pub async fn fetch_following(
    transport: &dyn FeedTransport,
    start: &str,
) -> Result<FeedReply, String> {
    let mut url = start.to_string();
    for _hop in 0..=MAX_REDIRECTS {
        if let Some(problem) = validate_feed_url(&url) {
            return Err(format!("{problem}: {url}"));
        }
        let reply = match timeout(FETCH_TIMEOUT, transport.get(url.clone())).await {
            Err(_) => return Err(TIMEOUT_TEXT.to_string()),
            Ok(reply) => reply?,
        };
        let redirect = (300..400).contains(&reply.status);
        let Some(location) = reply
            .location
            .as_deref()
            .filter(|l| redirect && !l.is_empty())
        else {
            return Ok(reply);
        };
        // `new URL(location, url).href`
        url = url::Url::parse(&url)
            .and_then(|base| base.join(location))
            .map_err(|_| "Invalid URL".to_string())?
            .to_string();
    }
    Err("Too many redirects".to_string())
}

/// One feed's headlines, or why it could not be read.
pub async fn fetch_source(
    transport: &dyn FeedTransport,
    source: &NewsSource,
) -> Result<Vec<NewsItem>, String> {
    let reply = fetch_following(transport, &source.url).await?;
    if !(200..300).contains(&reply.status) {
        return Err(format!("HTTP {}", reply.status));
    }
    // `response.text()`: UTF-8 whatever the charset says, a BOM dropped, bad bytes U+FFFD.
    let body = reply
        .body
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(&reply.body);
    let text = String::from_utf8_lossy(body);
    if reply.truncated || text.encode_utf16().count() > MAX_FEED_CHARS {
        return Err("feed too large".to_string());
    }
    Ok(parse_feed(&text, &source.name))
}

// ---- the service ----------------------------------------------------------------------------

/// The `/api/news` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewsResult {
    pub items: Vec<NewsItem>,
    /// Set when a source failed. The items are still usable, just possibly incomplete.
    pub error: Option<String>,
}

/// ms since the epoch. Injected so tests can move time.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn unix_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

type Refresh = Shared<BoxFuture<'static, ()>>;

struct Cache {
    items: Vec<NewsItem>,
    fetched_at: i64,
    last_error: Option<String>,
    /// The refresh in flight, which every reader that needs one awaits.
    refreshing: Option<Refresh>,
}

struct Inner {
    transport: Arc<dyn FeedTransport>,
    now: Clock,
    cache: Mutex<Cache>,
}

/// The data behind a poisoned lock is still consistent: it is only ever replaced whole.
fn lock(cache: &Mutex<Cache>) -> MutexGuard<'_, Cache> {
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Fetches feeds on demand and caches them in memory for 5 minutes, so analysis never waits on
/// news more than it must. Headlines are never stored on disk. Cheap to clone; clones share the
/// cache.
#[derive(Clone)]
pub struct NewsService {
    inner: Arc<Inner>,
}

impl Default for NewsService {
    fn default() -> Self {
        Self::new()
    }
}

impl NewsService {
    /// The real network and the wall clock.
    pub fn new() -> Self {
        Self::with_transport(Arc::new(NetFeedTransport::new()), Arc::new(unix_ms))
    }

    /// Create an empty cache with injectable transport and clock for deterministic tests.
    pub fn with_transport(transport: Arc<dyn FeedTransport>, now: Clock) -> Self {
        NewsService {
            inner: Arc::new(Inner {
                transport,
                now,
                cache: Mutex::new(Cache {
                    items: Vec::new(),
                    fetched_at: 0,
                    last_error: None,
                    refreshing: None,
                }),
            }),
        }
    }

    /// Call after sources change, so the next read does not serve the old set. As in Node, a
    /// refresh already in flight still stamps the cache fresh when it finishes.
    pub fn invalidate(&self) {
        lock(&self.inner.cache).fetched_at = 0;
    }

    /// The headlines for one coin. `sources` is the whole list, enabled or not; it is read only
    /// if this call starts a refresh. Concurrent readers share one refresh, which runs in its own
    /// task, so a reader that goes away does not cancel it.
    pub async fn for_symbol(
        &self,
        sources: Vec<NewsSource>,
        base_currency: &str,
        limit: usize,
    ) -> NewsResult {
        let refresh = {
            let mut cache = lock(&self.inner.cache);
            if (self.inner.now)() - cache.fetched_at >= REFRESH_MS {
                let inner = &self.inner;
                Some(
                    cache
                        .refreshing
                        .get_or_insert_with(|| start_refresh(inner.clone(), sources))
                        .clone(),
                )
            } else {
                None
            }
        };
        if let Some(refresh) = refresh {
            refresh.await;
        }
        let cache = lock(&self.inner.cache);
        NewsResult {
            items: select_news(&cache.items, base_currency, (self.inner.now)(), limit),
            error: cache.last_error.clone(),
        }
    }
}

/// Detach a refresh from readers and share its completion among concurrent requests.
fn start_refresh(inner: Arc<Inner>, sources: Vec<NewsSource>) -> Refresh {
    let task = tokio::spawn(refresh(inner, sources));
    let done: BoxFuture<'static, ()> = Box::pin(async move {
        let _ = task.await;
    });
    done.shared()
}

/// Clears `refreshing` however the refresh ends, a panic included, so the next read can start
/// another.
struct RefreshDone(Arc<Inner>);

impl Drop for RefreshDone {
    fn drop(&mut self) {
        lock(&self.0.cache).refreshing = None;
    }
}

/// Fetch enabled sources concurrently and retain old headlines on a total outage.
async fn refresh(inner: Arc<Inner>, sources: Vec<NewsSource>) {
    let _done = RefreshDone(inner.clone());
    let enabled: Vec<NewsSource> = sources.into_iter().filter(|s| s.enabled).collect();
    let results = join_all(
        enabled
            .iter()
            .map(|source| fetch_source(inner.transport.as_ref(), source)),
    )
    .await;
    let failed: Vec<&str> = enabled
        .iter()
        .zip(&results)
        .filter(|(_, result)| result.is_err())
        .map(|(source, _)| source.name.as_str())
        .collect();
    let items: Vec<NewsItem> = results
        .into_iter()
        .flat_map(Result::unwrap_or_default)
        .collect();
    let mut cache = lock(&inner.cache);
    // A total outage keeps the last good headlines rather than leaving an agent with none.
    if !items.is_empty() || enabled.is_empty() {
        cache.items = items;
    }
    cache.last_error =
        (!failed.is_empty()).then(|| format!("Could not read: {}", failed.join(", ")));
    cache.fetched_at = (inner.now)();
}

#[cfg(test)]
mod resolve_tests {
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr;

    use reqwest::dns::{Name, Resolve};

    use super::*;

    fn resolver(answer: &[&str]) -> PublicOnlyResolver {
        let ips: Vec<IpAddr> = answer.iter().map(|ip| ip.parse().unwrap()).collect();
        PublicOnlyResolver {
            lookup: Arc::new(move |_name: String| {
                let ips = ips.clone();
                Box::pin(async move { Ok(ips) })
            }),
        }
    }

    async fn resolve(answer: &[&str]) -> Result<Vec<SocketAddr>, String> {
        let name = Name::from_str("feed.test").unwrap();
        match resolver(answer).resolve(name).await {
            Ok(addrs) => Ok(addrs.collect()),
            Err(error) => Err(error.to_string()),
        }
    }

    #[test]
    fn public_address_table() {
        for (address, public) in [
            ("0.0.0.0", false),
            ("1.1.1.1", true),
            ("10.0.0.5", false),
            ("100.63.255.255", true),
            ("100.64.0.1", false),
            ("100.127.255.255", false),
            ("100.128.0.0", true),
            ("127.0.0.1", false),
            ("169.254.169.254", false),
            ("172.15.255.255", true),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("172.32.0.0", true),
            ("192.0.0.8", false),
            ("192.0.2.1", true),
            ("192.168.1.1", false),
            ("198.17.255.255", true),
            ("198.18.0.1", false),
            ("198.19.255.255", false),
            ("198.20.0.0", true),
            ("224.0.0.1", false),
            ("240.0.0.1", false),
            ("255.255.255.255", false),
            ("93.184.215.14", true),
            ("::", false),
            ("::1", false),
            ("::2", false),
            ("2606:4700::1111", true),
            ("fc00::1", false),
            ("fdff::1", false),
            ("fe80::1", false),
            ("febf::1", false),
            ("fec0::1", false),
            ("ff02::1", false),
            ("::ffff:192.168.1.1", false),
            ("::ffff:8.8.8.8", true),
            ("64:ff9b::10.0.0.1", false),
            ("64:ff9b::8.8.8.8", true),
            ("2002:808:808::1", true),
            ("2002:a00:1::1", false),
            ("2002:7f00:1::", false),
            ("2003::1", true),
            ("::8.8.8.8", true),
            ("::10.0.0.1", false),
            ("::1:808:808", true),
            ("64:ff9b:1::", false),
            ("64:ff9b:2::1", true),
            ("fec0::", false),
            ("feff:ffff::1", false),
            ("fe7f::1", true),
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert_eq!(is_public_address(ip), public, "{address}");
        }
    }

    #[tokio::test]
    async fn any_non_public_address_refuses_the_name() {
        for answer in [
            &["127.0.0.1"][..],
            &["10.0.0.5"],
            &["::1"],
            &["::ffff:192.168.1.1"],
            &["93.184.215.14", "10.0.0.5"],
        ] {
            assert_eq!(
                resolve(answer).await.unwrap_err(),
                PUBLIC_HOST_PROBLEM,
                "{answer:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_public_only_name_resolves_to_exactly_the_checked_addresses() {
        let addrs = resolve(&["93.184.215.14", "2606:4700::1111"])
            .await
            .unwrap();
        assert_eq!(
            addrs,
            [
                "93.184.215.14:0".parse::<SocketAddr>().unwrap(),
                "[2606:4700::1111]:0".parse().unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn an_empty_answer_is_a_dns_failure_not_a_refusal() {
        let error = resolve(&[]).await.unwrap_err();
        assert_ne!(error, PUBLIC_HOST_PROBLEM);
    }
}
