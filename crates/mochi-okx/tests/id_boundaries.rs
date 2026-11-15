use mochi_okx::{iso_string, to_inst_id};

#[test]
fn signed_timestamps_cover_epoch_leap_and_extended_year_boundaries() {
    // Literal Date.toISOString outputs, independent of the Rust calendar implementation.
    for (ms, expected) in [
        (-1, "1969-12-31T23:59:59.999Z"),
        (0, "1970-01-01T00:00:00.000Z"),
        (951_782_399_999, "2000-02-28T23:59:59.999Z"),
        (951_782_400_000, "2000-02-29T00:00:00.000Z"),
        (4_107_542_400_000, "2100-03-01T00:00:00.000Z"),
        (-62_167_219_200_000, "0000-01-01T00:00:00.000Z"),
        (253_402_300_800_000, "+010000-01-01T00:00:00.000Z"),
        (-62_167_219_200_001, "-000001-12-31T23:59:59.999Z"),
    ] {
        assert_eq!(iso_string(ms), expected, "{ms}");
    }
}

#[test]
fn instrument_conversion_removes_only_one_exact_trailing_usdt() {
    for (symbol, expected) in [
        ("BTCUSDT", "BTC-USDT-SWAP"),
        ("USDTUSDT", "USDT-USDT-SWAP"),
        ("USDTBTC", "USDTBTC-USDT-SWAP"),
        ("btcusdt", "btcusdt-USDT-SWAP"),
    ] {
        assert_eq!(to_inst_id(symbol), expected, "{symbol}");
    }
}
