//! `book`, `spread`, `funding` and `candles` take a DEX-qualified market
//! (`xyz:XYZ100`) and look it up on its dex: they read `perpDexs`, then that
//! dex's `meta`, then query the market by its qualified name.
//!
//! Only `candles` runs to the end against a mock. `book`, `spread` and
//! `funding` finish on a WebSocket that always connects to the public
//! endpoint (the API override does not apply), so they are checked up to the
//! lookup, with a market the dex does not list: the command must refuse it
//! after reading the dex and before it reaches the socket. A listed market is
//! then subscribed under its qualified name, which `subscribe_streams.rs`
//! checks for the `subscribe` market streams.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::reads::{read, read_json};

const MARKET: &str = "xyz:XYZ100";

async fn xyz_exchange() -> ExchangeFixture {
    ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await
}

async fn dex_meta_reads(server: &ExchangeFixture) -> Vec<Value> {
    server
        .info_requests("meta")
        .await
        .into_iter()
        .filter(|body| body["dex"] == "xyz")
        .collect()
}

#[tokio::test]
async fn candles_on_a_hip3_market_query_the_dex_market_by_its_qualified_name() {
    let server = xyz_exchange().await;
    server
        .mount_info_containing(
            json!({"type": "candleSnapshot", "req": {"coin": MARKET, "interval": "1h"}}),
            json!([{
                "t": 1_700_000_000_000_u64,
                "T": 1_700_003_599_999_u64,
                "s": MARKET,
                "i": "1h",
                "o": "120.5",
                "h": "125",
                "l": "119",
                "c": "123.5",
                "v": "42",
                "n": 7
            }]),
        )
        .await;

    let rows = read_json(&server, &["candles", MARKET, "--limit", "1"]);

    assert_eq!(rows[0]["coin"], MARKET);
    assert_eq!(rows[0]["close"], "123.5");
    assert_eq!(dex_meta_reads(&server).await.len(), 1);
    assert_eq!(server.info_requests("perpDexs").await.len(), 1);
    assert_eq!(server.info_requests("candleSnapshot").await.len(), 1);
}

#[tokio::test]
async fn a_market_the_dex_does_not_list_is_not_found_after_reading_the_dex() {
    let server = xyz_exchange().await;
    let market = "xyz:NOPE";

    for (index, command) in [
        ["book", market],
        ["spread", market],
        ["funding", market],
        ["candles", market],
    ]
    .into_iter()
    .enumerate()
    {
        read(&server, &command).code(13);

        assert_eq!(
            dex_meta_reads(&server).await.len(),
            index + 1,
            "{command:?} reads the dex's meta once"
        );
        assert_eq!(server.info_requests("perpDexs").await.len(), index + 1);
    }
}
