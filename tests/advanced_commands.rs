mod support;

use predicates::prelude::*;
use serde_json::Value;
use support::{API_OVERRIDE_ENV, IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mock_spot_quote_server() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "@1339": "100"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "meta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [],
            "collateralToken": 0
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "perpDexs"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "spotMeta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [
                { "tokens": [1, 2], "index": 1339, "name": "HYPE/USDH" }
            ],
            "tokens": [
                {
                    "name": "USDC",
                    "index": 0,
                    "tokenId": "0x00000000000000000000000000000000",
                    "szDecimals": 6,
                    "weiDecimals": 6,
                    "evmContract": null
                },
                {
                    "name": "HYPE",
                    "index": 1,
                    "tokenId": "0x00000000000000000000000000000001",
                    "szDecimals": 2,
                    "weiDecimals": 8,
                    "evmContract": null
                },
                {
                    "name": "USDH",
                    "index": 2,
                    "tokenId": "0x00000000000000000000000000000002",
                    "szDecimals": 2,
                    "weiDecimals": 6,
                    "evmContract": null
                }
            ]
        })))
        .mount(&server)
        .await;

    server
}

async fn mock_advanced_server() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "xyz:TOKEN": "1.23",
            "builderdex:TSLA": "250",
            "@1339": "100"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "perpDexs"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            null,
            {
                "name": "xyz",
                "deployerFeeScale": "1"
            },
            {
                "name": "builderdex",
                "deployerFeeScale": "1"
            }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "spotMeta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [],
            "tokens": [
                {
                    "name": "USDC",
                    "index": 0,
                    "tokenId": "0x00000000000000000000000000000000",
                    "szDecimals": 6,
                    "weiDecimals": 6,
                    "evmContract": null
                },
                {
                    "name": "USDH",
                    "index": 1,
                    "tokenId": "0x00000000000000000000000000000001",
                    "szDecimals": 6,
                    "weiDecimals": 6,
                    "evmContract": null
                }
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "meta", "dex": "xyz"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [
                {
                    "name": "xyz:TOKEN",
                    "szDecimals": 2,
                    "maxLeverage": 10,
                    "onlyIsolated": false,
                    "marginMode": null,
                    "growthMode": "disabled"
                }
            ],
            "collateralToken": 0
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "meta", "dex": "builderdex"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [
                {
                    "name": "TSLA",
                    "szDecimals": 3,
                    "maxLeverage": 10,
                    "onlyIsolated": false,
                    "marginMode": null,
                    "growthMode": "disabled"
                }
            ],
            "collateralToken": 1
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "meta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [],
            "collateralToken": 0
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "gossipPriorityAuctionStatus"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            [null, null, null, null, null],
            [
                {
                    "startTimeSeconds": 1710000000,
                    "durationSeconds": 180,
                    "startGas": "100",
                    "currentGas": "0.01",
                    "endGas": "10"
                }
            ]
        ])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": { "type": "default" }
        })))
        .mount(&server)
        .await;

    server
}

#[tokio::test]
async fn perps_list_dex_filters_to_hip3_markets() {
    let server = mock_advanced_server().await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["perps", "list", "--dex", "xyz"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TOKEN"))
        .stdout(predicate::str::contains("110000"));
}

#[tokio::test]
async fn perps_get_dex_accepts_unprefixed_token_from_prefixed_market() {
    let server = mock_advanced_server().await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["perps", "get", "TOKEN", "--dex", "xyz"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TOKEN"))
        .stdout(predicate::str::contains("110000"));
}

#[tokio::test]
async fn orders_create_dry_run_resolves_hip3_dex_markets() {
    let server = mock_advanced_server().await;

    let output = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "create",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--type",
            "market",
            "--amount",
            "12",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["args"]["asset_id"], 110000);
    assert_eq!(json["args"]["resolved_asset"], "xyz:TOKEN");
    assert_eq!(json["args"]["amount_unit"], "USDC");
}

async fn hip3_order_attempt(fail_abstraction: bool) -> (assert_cmd::assert::Assert, MockServer) {
    let server = mock_advanced_server().await;
    let (abstraction_status, abstraction_body) = if fail_abstraction {
        (500, serde_json::json!(null))
    } else {
        (200, serde_json::json!("disabled"))
    };
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "userAbstraction"}),
        ))
        .respond_with(ResponseTemplate::new(abstraction_status).set_body_json(abstraction_body))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "clearinghouseState"}),
        ))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let assert = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "orders",
            "create",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--price",
            "1.23",
            "--size",
            "10",
            "--testnet",
        ])
        .assert();
    (assert, server)
}

async fn exchange_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/exchange")
        .count()
}

#[tokio::test]
async fn hip3_margin_helper_does_not_transfer_when_the_dex_balance_cannot_be_read() {
    let (assert, server) = hip3_order_attempt(false).await;
    assert.code(12).stderr(predicate::str::contains(
        "The Hyperliquid API failed to process the request.",
    ));
    assert_eq!(
        exchange_requests(&server).await,
        0,
        "a failed balance read must not plan a transfer or place the order"
    );
}

#[tokio::test]
async fn hip3_margin_helper_does_not_skip_the_transfer_when_the_account_mode_cannot_be_read() {
    let (assert, server) = hip3_order_attempt(true).await;
    assert.code(12);
    assert_eq!(exchange_requests(&server).await, 0);
}

#[tokio::test]
async fn orders_create_dry_run_resolves_non_xyz_hip3_dex_markets() {
    let server = mock_advanced_server().await;

    let output = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "create",
            "--coin",
            "builderdex:TSLA",
            "--side",
            "buy",
            "--type",
            "market",
            "--amount",
            "12",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["args"]["asset_id"], 120000);
    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
    assert_eq!(json["args"]["amount_unit"], "USDH");
}

#[tokio::test]
async fn orders_tpsl_dry_run_resolves_hip3_dex_markets() {
    let server = mock_advanced_server().await;

    let output = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "tpsl",
            "--coin",
            "builderdex:TSLA",
            "--side",
            "sell",
            "--size",
            "0.1",
            "--take-profit",
            "260",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
    assert_eq!(json["args"]["legs"][0]["order"]["a"], 120000);
}

#[tokio::test]
async fn orders_twap_create_dry_run_resolves_hip3_dex_markets() {
    let server = mock_advanced_server().await;

    let output = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "twap-create",
            "--coin",
            "builderdex:TSLA",
            "--side",
            "buy",
            "--size",
            "0.1",
            "--duration",
            "300",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["args"]["asset_id"], 120000);
    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
}

#[tokio::test]
async fn orders_twap_cancel_dry_run_resolves_hip3_dex_markets() {
    let server = mock_advanced_server().await;

    let output = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "twap-cancel",
            "--coin",
            "builderdex:TSLA",
            "12345",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["args"]["twap_id"], 12345);
    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
}

#[tokio::test]
async fn orders_create_dry_run_reports_non_usdc_spot_quote_amount_unit() {
    let server = mock_spot_quote_server().await;

    let output = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "create",
            "--coin",
            "HYPE/USDH",
            "--side",
            "buy",
            "--type",
            "market",
            "--amount",
            "10",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["args"]["asset_id"], 11339);
    assert_eq!(json["args"]["resolved_asset"], "HYPE/USDH");
    assert_eq!(json["args"]["amount_unit"], "USDH");
}

#[tokio::test]
async fn perps_list_unknown_dex_exits_13() {
    let server = mock_advanced_server().await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["perps", "list", "--dex", "nonexistent"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("Unknown DEX: nonexistent"));
}

#[tokio::test]
async fn prio_status_shows_auction_status() {
    let server = mock_advanced_server().await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["prio", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Current Gas"))
        .stdout(predicate::str::contains("0.01"));
}

#[test]
fn prio_bid_without_required_parameters_exits_2_with_usage() {
    IsolatedHome::new()
        .command()
        .args(["prio", "bid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--max"))
        .stderr(predicate::str::contains("--ip"));
}

#[tokio::test]
async fn prio_bid_with_explicit_parameters_submits_signed_bid() {
    let server = mock_advanced_server().await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "prio",
            "bid",
            "--slot",
            "0",
            "--max",
            "1",
            "--ip",
            "203.0.113.10",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("submitted"))
        .stdout(predicate::str::contains("203.0.113.10"));

    let requests = server.received_requests().await.unwrap();
    let exchange_request = requests
        .iter()
        .find(|request| request.url.path() == "/exchange")
        .expect("expected /exchange request");
    let body: Value = serde_json::from_slice(&exchange_request.body).unwrap();
    assert_eq!(body["action"]["type"], "gossipPriorityBid");
    assert_eq!(body["action"]["slotId"], 0);
    assert_eq!(body["action"]["ip"], "203.0.113.10");
    assert_eq!(body["action"]["maxGas"], 1_000_001_u64);
}

#[tokio::test]
async fn prio_bid_cap_below_current_gas_does_not_exit_zero_or_submit() {
    let server = mock_advanced_server().await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "prio",
            "bid",
            "--slot",
            "0",
            "--max",
            "0.00000001",
            "--ip",
            "203.0.113.10",
            "--testnet",
        ])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("priority bid not submitted"));

    let exchange_requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/exchange")
        .count();
    assert_eq!(exchange_requests, 0, "bid cap rejection must not submit");
}

#[tokio::test]
async fn prio_bid_compares_decimal_current_gas_after_converting_to_wei() {
    let server = mock_advanced_server().await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "prio",
            "bid",
            "--slot",
            "0",
            "--max",
            "0.009",
            "--ip",
            "203.0.113.10",
            "--testnet",
        ])
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "current gas 0.01 is at or above --max 0.009",
        ));

    let exchange_requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/exchange")
        .count();
    assert_eq!(
        exchange_requests, 0,
        "decimal current gas must be compared as wei before bidding"
    );
}

/// The bid cap is counted in HyperCore wei (10^8 per HYPE) whatever the spot
/// token list says: this server has no `spotMeta` at all, so a bid that needed
/// the token's decimals would fail, and one that fell back to HyperEVM's 10^18
/// would send a cap 10^10 times too large.
#[tokio::test]
async fn prio_bid_counts_the_cap_in_hypercore_wei_without_a_token_lookup() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"BTC": "1"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "gossipPriorityAuctionStatus"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            [null, null, null, null, null],
            [{"startTimeSeconds": 1710000000, "durationSeconds": 180, "startGas": "100", "endGas": "10"}]
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": {"type": "default"}
        })))
        .mount(&server)
        .await;

    IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "--format",
            "json",
            "prio",
            "bid",
            "--max",
            "1.5",
            "--ip",
            "203.0.113.10",
            "--testnet",
        ])
        .assert()
        .success();

    let requests = server.received_requests().await.unwrap();
    let exchange_request = requests
        .iter()
        .find(|request| request.url.path() == "/exchange")
        .expect("expected /exchange request");
    let body: Value = serde_json::from_slice(&exchange_request.body).unwrap();
    assert_eq!(body["action"]["maxGas"], 150_000_000_u64);
}

#[tokio::test]
async fn orders_bracket_dry_run_resolves_hip3_dex_markets() {
    let server = mock_advanced_server().await;

    let output = IsolatedHome::new()
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "bracket",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--size",
            "0.5",
            "--take-profit",
            "+5%",
            "--stop-loss",
            "-2%",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["args"]["entry_plan"]["resolved_asset"], "xyz:TOKEN");
    assert_eq!(
        json["args"]["protection_plan"]["resolved_asset"],
        "xyz:TOKEN"
    );
}
