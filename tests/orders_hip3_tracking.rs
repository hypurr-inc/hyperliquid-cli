//! Orders that are placed on a HIP-3 market and then polled or listed must read
//! the market's own dex: its mids and its open orders are not in the default
//! dex's lists.

mod support;

use serde_json::{Value, json};
use support::{API_OVERRIDE_ENV, IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OID: u64 = 7;

fn open_order() -> Value {
    json!({
        "timestamp": 1_700_000_000_000_u64,
        "coin": "xyz:TOKEN",
        "side": "B",
        "limitPx": "1.2",
        "sz": "20",
        "oid": OID,
        "origSz": "20",
        "cloid": null,
        "orderType": "Limit",
        "tif": "Alo",
        "reduceOnly": false,
    })
}

async fn mount_info(server: &MockServer, request: Value, response: Value) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(request))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(server)
        .await;
}

/// A server where the market `xyz:TOKEN` lives on the dex `xyz`. The default
/// dex has no mids and no open orders, so a read that loses the dex finds
/// nothing. `exchange_status` answers every order the CLI sends.
async fn hip3_server(exchange_status: Value) -> MockServer {
    let server = MockServer::start().await;
    mount_info(
        &server,
        json!({"type": "perpDexs"}),
        json!([null, {"name": "xyz", "deployerFeeScale": "1"}]),
    )
    .await;
    mount_info(
        &server,
        json!({"type": "spotMeta"}),
        json!({
            "universe": [],
            "tokens": [{
                "name": "USDC",
                "index": 0,
                "tokenId": "0x00000000000000000000000000000000",
                "szDecimals": 6,
                "weiDecimals": 6,
                "evmContract": null
            }]
        }),
    )
    .await;
    mount_info(
        &server,
        json!({"type": "meta", "dex": "xyz"}),
        json!({
            "universe": [{
                "name": "xyz:TOKEN",
                "szDecimals": 2,
                "maxLeverage": 10,
                "onlyIsolated": false,
                "marginMode": null,
                "growthMode": "disabled"
            }],
            "collateralToken": 0
        }),
    )
    .await;
    mount_info(
        &server,
        json!({"type": "meta"}),
        json!({"universe": [], "collateralToken": 0}),
    )
    .await;
    mount_info(
        &server,
        json!({"type": "allMids", "dex": "xyz"}),
        json!({"xyz:TOKEN": "1.23"}),
    )
    .await;
    mount_info(&server, json!({"type": "allMids"}), json!({"BTC": "50000"})).await;
    mount_info(
        &server,
        json!({"type": "frontendOpenOrders", "dex": "xyz"}),
        json!([open_order()]),
    )
    .await;
    mount_info(&server, json!({"type": "frontendOpenOrders"}), json!([])).await;
    mount_info(
        &server,
        json!({"type": "userAbstraction"}),
        json!("unifiedAccount"),
    )
    .await;
    mount_info(
        &server,
        json!({"type": "clearinghouseState"}),
        json!({
            "marginSummary": {
                "accountValue": "10000", "totalNtlPos": "0",
                "totalRawUsd": "10000", "totalMarginUsed": "0"
            },
            "crossMarginSummary": {
                "accountValue": "10000", "totalNtlPos": "0",
                "totalRawUsd": "10000", "totalMarginUsed": "0"
            },
            "crossMaintenanceMarginUsed": "0",
            "withdrawable": "10000",
            "assetPositions": [],
            "time": 1_700_000_000_000_u64
        }),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ok",
            "response": {"type": "order", "data": {"statuses": [exchange_status]}}
        })))
        .mount(&server)
        .await;
    server
}

async fn info_requests(server: &MockServer, request_type: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/info")
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| body["type"] == request_type)
        .collect()
}

fn command_against(env: &IsolatedHome, server: &MockServer, args: &[&str]) -> assert_cmd::Command {
    let mut command = env.command();
    command
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["--format", "json", "--testnet"])
        .args(args);
    command
}

fn json_stdout(assert: &assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("one JSON document on stdout")
}

#[tokio::test]
async fn chase_polls_its_quote_on_the_market_dex() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;

    let assert = command_against(
        &env,
        &server,
        &[
            "orders",
            "chase",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--size",
            "20",
            "--timeout",
            "1s",
            "--interval",
            "1s",
        ],
    )
    .assert()
    .success();

    // The quote rests on the dex's book. Read from the default dex it would
    // vanish and chase would report it filled.
    assert_eq!(json_stdout(&assert)["status"], "timeout");
    let mids = info_requests(&server, "allMids").await;
    assert!(mids.iter().any(|body| body["dex"] == "xyz"), "{mids:?}");
    let open = info_requests(&server, "frontendOpenOrders").await;
    assert!(!open.is_empty());
    assert!(open.iter().all(|body| body["dex"] == "xyz"), "{open:?}");
}

#[tokio::test]
async fn chase_dry_run_prices_from_the_market_dex_mid() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;

    let assert = command_against(
        &env,
        &server,
        &[
            "--dry-run",
            "orders",
            "chase",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--size",
            "20",
            "--timeout",
            "1s",
        ],
    )
    .assert()
    .success();

    assert_eq!(json_stdout(&assert)["args"]["first_price"], "1.229385");
}

#[tokio::test]
async fn chase_dry_run_fails_when_the_market_does_not_exist() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;

    command_against(
        &env,
        &server,
        &[
            "--dry-run",
            "orders",
            "chase",
            "--coin",
            "xyz:NOPE",
            "--side",
            "buy",
            "--size",
            "20",
            "--timeout",
            "1s",
        ],
    )
    .assert()
    .code(13);
}

#[tokio::test]
async fn chase_quote_rejected_for_a_rate_limit_exits_11() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"error": "Too many requests: rate limit exceeded"})).await;

    command_against(
        &env,
        &server,
        &[
            "orders",
            "chase",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--size",
            "20",
            "--timeout",
            "1s",
        ],
    )
    .assert()
    .code(11);
}

#[tokio::test]
async fn chase_quote_rejected_for_another_reason_keeps_its_wording_and_exit_13() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"error": "Order has invalid size."})).await;

    let assert = command_against(
        &env,
        &server,
        &[
            "orders",
            "chase",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--size",
            "20",
            "--timeout",
            "1s",
        ],
    )
    .assert()
    .code(13);

    assert_eq!(
        json_stdout(&assert)["error"],
        "Unsupported input: chase quote rejected: [untrusted remote data] Order has invalid size."
    );
}

#[tokio::test]
async fn bracket_entry_polls_the_market_dex_and_cancels_an_unfilled_entry() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;

    let assert = command_against(
        &env,
        &server,
        &[
            "orders",
            "bracket",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--entry",
            "limit",
            "--price",
            "1.2",
            "--size",
            "20",
            "--take-profit",
            "+10%",
            "--stop-loss",
            "-5%",
            "--entry-timeout",
            "1s",
        ],
    )
    .assert()
    .code(15);

    // The entry stays on the dex's book until the timeout cancels it; read
    // from the default dex it would look filled and be "protected".
    let document = json_stdout(&assert);
    assert_eq!(document["status"], "entry_cancelled");
    assert_eq!(document["category"], "partial_results");
    assert_eq!(document["exit_code"], 15);
    assert_eq!(document["entry_order_id"], OID);
    let open = info_requests(&server, "frontendOpenOrders").await;
    assert!(!open.is_empty());
    assert!(open.iter().all(|body| body["dex"] == "xyz"), "{open:?}");
}

#[tokio::test]
async fn bracket_that_cannot_check_its_entry_reports_next_commands_under_next() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;
    // Outranks the open-orders mocks above; every other request is still answered.
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(json!({"type": "frontendOpenOrders"})))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&server)
        .await;

    let assert = command_against(
        &env,
        &server,
        &[
            "orders",
            "bracket",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--entry",
            "limit",
            "--price",
            "1.2",
            "--size",
            "20",
            "--take-profit",
            "+10%",
            "--stop-loss",
            "-5%",
            "--entry-timeout",
            "5s",
        ],
    )
    .assert()
    .code(15);

    let document = json_stdout(&assert);
    assert_eq!(document["status"], "entry_open_unprotected");
    assert_eq!(document["category"], "partial_results");
    assert_eq!(document["reason"], "partial_failure");
    assert_eq!(document["entry_order_id"], OID);
    assert_eq!(document["next"][0]["command"], "hyperliquid orders open");
    assert_eq!(
        document["next"][1]["command"],
        format!("hyperliquid orders cancel {OID}")
    );
    assert!(document.get("next_commands").is_none());
}

#[tokio::test]
async fn bracket_entry_rejected_for_a_rate_limit_exits_11() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"error": "rate limit exceeded"})).await;

    command_against(
        &env,
        &server,
        &[
            "orders",
            "bracket",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--entry",
            "limit",
            "--price",
            "1.2",
            "--size",
            "20",
            "--take-profit",
            "+10%",
            "--stop-loss",
            "-5%",
        ],
    )
    .assert()
    .code(11);
}

#[tokio::test]
async fn cancel_all_with_a_dex_lists_that_dexs_open_orders() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"success": null})).await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ok",
            "response": {"type": "cancel", "data": {"statuses": ["success"]}}
        })))
        .with_priority(1)
        .mount(&server)
        .await;

    let assert = command_against(
        &env,
        &server,
        &[
            "orders",
            "cancel-all",
            "--coin",
            "TOKEN",
            "--dex",
            "xyz",
            "--yes",
        ],
    )
    .assert()
    .success();

    assert_eq!(json_stdout(&assert)[0]["cancelled_orders"], 1);
    let open = info_requests(&server, "frontendOpenOrders").await;
    assert!(!open.is_empty());
    assert!(open.iter().all(|body| body["dex"] == "xyz"), "{open:?}");
}
