//! A market order on a spot pair prices its protective limit from the pair's
//! mid, which `allMids` lists by name for PURR/USDC and by `@index` for the
//! rest.

mod support;

use serde_json::json;
use support::IsolatedHome;
use support::exchange::ExchangeFixture;
use support::orders::dry_run;

/// A testnet whose spot book lists PURR/USDC by name and HYPE/USDC as `@1035`,
/// as the real `allMids` does.
async fn spot_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "allMids"}),
            json!({"BTC": "50000", "PURR/USDC": "0.09", "@1035": "40"}),
        )
        .await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({
                "universe": [
                    {"name": "PURR/USDC", "index": 0, "tokens": [1, 0]},
                    {"name": "HYPE/USDC", "index": 1035, "tokens": [1105, 0]}
                ],
                "tokens": [
                    {
                        "name": "USDC", "index": 0,
                        "tokenId": "0x00000000000000000000000000000000",
                        "szDecimals": 6, "weiDecimals": 6, "evmContract": null
                    },
                    {
                        "name": "PURR", "index": 1,
                        "tokenId": "0x00000000000000000000000000000002",
                        "szDecimals": 0, "weiDecimals": 5, "evmContract": null
                    },
                    {
                        "name": "HYPE", "index": 1105,
                        "tokenId": "0x00000000000000000000000000000001",
                        "szDecimals": 2, "weiDecimals": 18, "evmContract": null
                    }
                ]
            }),
        )
        .await;
    server
}

/// The protective price of a dry-run market buy of 100 of `pair`.
async fn protective_price(pair: &str) -> serde_json::Value {
    let (env, server) = (IsolatedHome::new(), spot_exchange().await);
    let preview = dry_run(&env, &server, &format!("buy --coin {pair} --size 100"));
    preview["args"]["limit_px"].clone()
}

#[tokio::test]
async fn a_market_order_on_purr_reads_its_mid_by_name() {
    // The 5% protective price above the 0.09 mid.
    assert_eq!(protective_price("PURR/USDC").await, "0.094500");
}

#[tokio::test]
async fn a_market_order_on_any_other_pair_reads_its_mid_by_index() {
    // The 5% protective price above the 40 mid.
    assert_eq!(protective_price("HYPE/USDC").await, "42.000");
}
