//! `--results-only`, `--max-results` and `--select`: what an agent gets when it
//! narrows a command's JSON. The projection is one generic pipeline, so it is
//! pinned once on a document; each command's selectable field names are pinned
//! by running the command.

mod support;

use hyperliquid_cli::output::JsonRenderOptions;
use serde_json::{Value, json};

use support::exchange::ExchangeFixture;
use support::{API_OVERRIDE_ENV, IsolatedHome, run_json, wire};

const USER: &str = "0x0000000000000000000000000000000000000001";

#[test]
fn results_only_then_max_results_then_select_apply_to_a_document() {
    let options = JsonRenderOptions::from_cli(Some("coin,price"), true, Some(2));
    let document = json!({
        "results": [
            {"name": "BTC", "mid": "100", "max_leverage": 50},
            {"name": "ETH", "mid": "10", "max_leverage": 25},
            {"name": "SOL", "mid": "1", "max_leverage": 20}
        ],
        "next_page": "abc"
    });

    assert_eq!(
        options.apply(document),
        json!([
            {"coin": "BTC", "price": "100"},
            {"coin": "ETH", "price": "10"}
        ])
    );
}

/// `--select <fields>` on `args` against `server`: the JSON the command printed.
fn select(server: &ExchangeFixture, fields: &str, args: &[&str]) -> Value {
    run_json(
        IsolatedHome::new()
            .json_command()
            .env(API_OVERRIDE_ENV, server.uri())
            .args(["--select", fields])
            .args(args),
    )
}

#[tokio::test]
async fn select_keeps_the_requested_fields_of_a_list() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), wire::malformed_spot_meta())
        .await;
    server
        .mount_info(
            json!({"type": "userFills", "user": USER}),
            json!([wire::fill("BTC", 42)]),
        )
        .await;

    let markets = select(&server, "symbol,base", &["spot", "list"]);
    assert_eq!(markets, json!([{"symbol": "PURR/USDC", "base": "PURR"}]));

    let fills = select(&server, "coin,time", &["account", "fills", USER]);
    assert_eq!(
        fills,
        json!([{"coin": "BTC", "time": 1_700_000_000_000_u64}])
    );
}

#[tokio::test]
async fn select_keeps_the_requested_fields_of_a_single_document() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "l2Book", "coin": "BTC"}),
            json!({
                "coin": "BTC",
                "time": 123,
                "levels": [
                    [{"px": "100", "sz": "1", "n": 2}],
                    [{"px": "101", "sz": "1.5", "n": 3}]
                ]
            }),
        )
        .await;

    let book = select(&server, "coin,bids", &["book", "BTC"]);

    assert_eq!(book["coin"], "BTC");
    assert!(book["bids"].is_array(), "{book}");
    assert!(book.get("asks").is_none(), "{book}");
}

#[tokio::test]
async fn max_results_limits_candles_before_they_are_selected() {
    let server = ExchangeFixture::start().await;
    let candle = |start: u64, close: &str| {
        json!({
            "t": start, "T": start + 1, "s": "BTC", "i": "1h",
            "o": "100", "h": "110", "l": "90", "c": close, "v": "42", "n": 7
        })
    };
    server
        .mount_info_containing(
            json!({"type": "candleSnapshot", "req": {"coin": "BTC", "interval": "1h"}}),
            json!([candle(1, "105"), candle(3, "111")]),
        )
        .await;

    let candles = select(
        &server,
        "timestamp,close",
        &["--max-results", "1", "candles", "BTC"],
    );

    assert_eq!(candles, json!([{"timestamp": 1, "close": "105"}]));
}
