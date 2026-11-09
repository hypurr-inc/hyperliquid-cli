//! HIP-3 dex scoping on account and market reads.

mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::{IsolatedHome, mount_override_healthcheck};
use wiremock::matchers::{body_json, body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USER: &str = "0x0000000000000000000000000000000000000001";

fn json_stdout(output: Vec<u8>) -> Value {
    serde_json::from_slice(&output).expect("stdout should be valid JSON")
}

/// The minimum `/info` reads `orders open` needs to build an asset resolver.
async fn mount_resolver_metadata(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "meta" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "universe": [{ "name": "BTC", "szDecimals": 5, "maxLeverage": 40, "onlyIsolated": false }],
            "collateralToken": 0
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "spotMeta" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "universe": [],
            "tokens": [{
                "name": "USDC",
                "index": 0,
                "tokenId": "0x00000000000000000000000000000000",
                "szDecimals": 6,
                "weiDecimals": 6,
                "evmContract": null
            }]
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "perpDexs" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([null, { "name": "xyz" }])))
        .mount(server)
        .await;
}

#[tokio::test]
async fn mids_scopes_all_mids_to_a_dex() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    // The loopback API override probes the endpoint with an unqualified allMids.
    // Match that body exactly so the dex-scoped call below is not shadowed.
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "allMids" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "BTC": "50000" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "perpDexs" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([null, { "name": "xyz" }])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            json!({ "type": "allMids", "dex": "xyz" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "xyz:XYZ100": "123.5" })))
        .expect(1)
        .mount(&server)
        .await;

    let output = env
        .command_with_server(&server)
        .args(["--format", "json", "mids", "--dex", "xyz"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(json_stdout(output)["xyz:XYZ100"], "123.5");
    server.verify().await;
}

#[tokio::test]
async fn positions_list_scopes_clearinghouse_state_to_a_dex() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    // The loopback API override probes the endpoint with an unqualified allMids.
    mount_override_healthcheck(&server).await;
    mount_resolver_metadata(&server).await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            json!({ "type": "clearinghouseState", "user": USER, "dex": "xyz" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "marginSummary": {
                "accountValue": "10",
                "totalNtlPos": "0",
                "totalRawUsd": "10",
                "totalMarginUsed": "0"
            },
            "crossMarginSummary": {
                "accountValue": "10",
                "totalNtlPos": "0",
                "totalRawUsd": "10",
                "totalMarginUsed": "0"
            },
            "crossMaintenanceMarginUsed": "0",
            "withdrawable": "10",
            "assetPositions": [],
            "time": 1_700_000_000_000_u64
        })))
        .expect(1)
        .mount(&server)
        .await;

    env.command_with_server(&server)
        .args([
            "--format",
            "json",
            "--ows-signer",
            USER,
            "positions",
            "list",
            "--dex",
            "xyz",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("[]"));

    server.verify().await;
}

#[tokio::test]
async fn orders_open_scopes_frontend_open_orders_to_a_dex() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    // The loopback API override probes the endpoint with an unqualified allMids.
    mount_override_healthcheck(&server).await;
    mount_resolver_metadata(&server).await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            json!({ "type": "frontendOpenOrders", "user": USER, "dex": "xyz" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    env.command_with_server(&server)
        .args([
            "--format",
            "json",
            "--ows-signer",
            USER,
            "orders",
            "open",
            "--dex",
            "xyz",
        ])
        .assert()
        .success();

    server.verify().await;
}

#[tokio::test]
async fn portfolio_dex_view_skips_account_wide_reads() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    // The loopback API override probes the endpoint with an unqualified allMids.
    mount_override_healthcheck(&server).await;
    mount_resolver_metadata(&server).await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            json!({ "type": "clearinghouseState", "user": USER, "dex": "xyz" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "marginSummary": {
                "accountValue": "42",
                "totalNtlPos": "0",
                "totalRawUsd": "42",
                "totalMarginUsed": "0"
            },
            "crossMarginSummary": {
                "accountValue": "42",
                "totalNtlPos": "0",
                "totalRawUsd": "42",
                "totalMarginUsed": "0"
            },
            "crossMaintenanceMarginUsed": "0",
            "withdrawable": "42",
            "assetPositions": [],
            "time": 1_700_000_000_000_u64
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Spot balances and vault equities are account-wide; a dex view must not
    // report them, so these endpoints are never called.
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            json!({ "type": "spotClearinghouseState" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "balances": [] })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(json!({ "type": "userVaultEquities" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&server)
        .await;

    let output = env
        .command_with_server(&server)
        .args([
            "--format",
            "json",
            "account",
            "portfolio",
            USER,
            "--dex",
            "xyz",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let portfolio = json_stdout(output);
    assert_eq!(portfolio["summary"]["account_value"], "42");
    assert_eq!(portfolio["spot_balances"].as_array().unwrap().len(), 0);
    server.verify().await;
}

#[tokio::test]
async fn blank_dex_is_the_default_dex() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    // Both the override probe and the command send an unqualified allMids: a
    // blank --dex must not add a dex field.
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "allMids" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "BTC": "50000" })))
        .expect(2)
        .mount(&server)
        .await;

    let output = env
        .command_with_server(&server)
        .args(["--format", "json", "mids", "--dex", ""])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(json_stdout(output)["BTC"], "50000");
    server.verify().await;
}

#[tokio::test]
async fn blank_dex_keeps_account_wide_portfolio_rows() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    mount_override_healthcheck(&server).await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(
            json!({ "type": "clearinghouseState", "user": USER }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "marginSummary": {
                "accountValue": "11",
                "totalNtlPos": "0",
                "totalRawUsd": "11",
                "totalMarginUsed": "0"
            },
            "crossMarginSummary": {
                "accountValue": "11",
                "totalNtlPos": "0",
                "totalRawUsd": "11",
                "totalMarginUsed": "0"
            },
            "crossMaintenanceMarginUsed": "0",
            "withdrawable": "11",
            "assetPositions": [],
            "time": 1_700_000_000_000_u64
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(
            json!({ "type": "spotClearinghouseState", "user": USER }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "balances": [{
                "coin": "USDC", "token": 0, "hold": "0", "total": "7", "entryNtl": "0"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(
            json!({ "type": "userVaultEquities", "user": USER }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let output = env
        .command_with_server(&server)
        .args([
            "--format",
            "json",
            "account",
            "portfolio",
            USER,
            "--dex",
            "",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let portfolio = json_stdout(output);
    assert_eq!(portfolio["summary"]["account_value"], "11");
    assert_eq!(portfolio["spot_balances"].as_array().unwrap().len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn unknown_dex_fails_before_reading() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    mount_override_healthcheck(&server).await;
    mount_resolver_metadata(&server).await;

    env.command_with_server(&server)
        .args(["--format", "json", "mids", "--dex", "nope"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("Unknown DEX"));

    server.verify().await;
}

#[tokio::test]
async fn dex_is_sent_in_its_canonical_spelling() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    // Exact body: the generic healthcheck mock would also match the scoped call.
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "allMids" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "BTC": "1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(json!({ "type": "perpDexs" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([null, { "name": "xyz" }])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            json!({ "type": "allMids", "dex": "xyz" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "xyz:XYZ100": "1" })))
        .expect(1)
        .mount(&server)
        .await;

    env.command_with_server(&server)
        .args(["--format", "json", "mids", "--dex", "XYZ"])
        .assert()
        .success();

    server.verify().await;
}

#[tokio::test]
async fn unknown_dex_fails_before_a_watch_starts() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    mount_override_healthcheck(&server).await;
    mount_resolver_metadata(&server).await;

    env.command_with_server(&server)
        .args([
            "--format",
            "json",
            "--non-interactive",
            "--ows-signer",
            USER,
            "positions",
            "list",
            "--watch",
            "--max-ticks",
            "1",
            "--dex",
            "nope",
        ])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("Unknown DEX"));

    env.command_with_server(&server)
        .args([
            "--format",
            "json",
            "--non-interactive",
            "--ows-signer",
            USER,
            "orders",
            "open",
            "--watch",
            "--max-ticks",
            "1",
            "--dex",
            "nope",
        ])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("Unknown DEX"));

    server.verify().await;
}

#[tokio::test]
async fn positions_watch_sends_the_canonical_dex() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    mount_override_healthcheck(&server).await;
    mount_resolver_metadata(&server).await;
    // Only the canonical spelling may reach the snapshot read; max_ticks bounds
    // the watch to that single tick, so no websocket event is awaited.
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            json!({ "type": "clearinghouseState", "user": USER, "dex": "xyz" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "marginSummary": {
                "accountValue": "10",
                "totalNtlPos": "0",
                "totalRawUsd": "10",
                "totalMarginUsed": "0"
            },
            "crossMarginSummary": {
                "accountValue": "10",
                "totalNtlPos": "0",
                "totalRawUsd": "10",
                "totalMarginUsed": "0"
            },
            "crossMaintenanceMarginUsed": "0",
            "withdrawable": "10",
            "assetPositions": [],
            "time": 1_700_000_000_000_u64
        })))
        .expect(1)
        .mount(&server)
        .await;

    env.command_with_server(&server)
        .args([
            "--format",
            "json",
            "--non-interactive",
            "--ows-signer",
            USER,
            "positions",
            "list",
            "--watch",
            "--max-ticks",
            "1",
            "--dex",
            "XYZ",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("[]"));

    server.verify().await;
}
