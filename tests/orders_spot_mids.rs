mod support;

use serde_json::{Value, json};
use support::{API_OVERRIDE_ENV, IsolatedHome};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_info(server: &MockServer, request: Value, response: Value) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(request))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(server)
        .await;
}

/// A testnet whose spot book lists PURR/USDC by name and HYPE/USDC as `@1035`,
/// as the real `allMids` does.
async fn spot_server() -> MockServer {
    let server = MockServer::start().await;
    mount_info(
        &server,
        json!({"type": "allMids"}),
        json!({"BTC": "50000", "PURR/USDC": "0.09", "@1035": "40"}),
    )
    .await;
    mount_info(
        &server,
        json!({"type": "spotMeta"}),
        json!({
            "universe": [
                { "name": "PURR/USDC", "index": 0, "tokens": [1, 0] },
                { "name": "HYPE/USDC", "index": 1035, "tokens": [1105, 0] }
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
    mount_info(
        &server,
        json!({"type": "meta"}),
        json!({
            "universe": [{
                "name": "BTC", "szDecimals": 5, "maxLeverage": 50,
                "onlyIsolated": false, "marginMode": null, "growthMode": "disabled"
            }],
            "collateralToken": 0
        }),
    )
    .await;
    mount_info(&server, json!({"type": "perpDexs"}), json!([])).await;
    server
}

fn market_buy_preview(
    env: &IsolatedHome,
    server: &MockServer,
    pair: &str,
) -> assert_cmd::assert::Assert {
    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "--testnet",
            "buy",
            "--coin",
            pair,
            "--size",
            "100",
        ])
        .assert()
}

#[tokio::test]
async fn a_market_order_on_purr_reads_its_mid_by_name() {
    let env = IsolatedHome::new();
    let server = spot_server().await;

    let output = market_buy_preview(&env, &server, "PURR/USDC")
        .success()
        .get_output()
        .stdout
        .clone();
    let document: Value = serde_json::from_slice(&output).unwrap();
    // The 5% protective price above the 0.09 mid.
    assert_eq!(document["args"]["limit_px"], "0.094500");
}

#[tokio::test]
async fn a_market_order_on_any_other_pair_reads_its_mid_by_index() {
    let env = IsolatedHome::new();
    let server = spot_server().await;

    let output = market_buy_preview(&env, &server, "HYPE/USDC")
        .success()
        .get_output()
        .stdout
        .clone();
    let document: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(document["args"]["limit_px"], "42.000");
}
