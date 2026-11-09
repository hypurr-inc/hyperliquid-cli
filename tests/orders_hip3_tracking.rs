//! Orders that are placed on a HIP-3 market and then polled or listed must read
//! the market's own dex: its mids and its open orders are not in the default
//! dex's lists.

mod support;

use serde_json::{Value, json};
use support::{API_OVERRIDE_ENV, IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY, json_stdout};
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

    // 1.23 less 5 bps is 1.229385, which is finer than the dex market's tick
    // grid; live chase sends it truncated, so the preview shows it truncated.
    assert_eq!(json_stdout(&assert)["args"]["first_price"], "1.2293");
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

#[tokio::test]
async fn chase_dry_run_rejects_a_size_the_live_chase_would_reject() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;

    // The market's lot is 2 decimals: 0.001 truncates to zero.
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
            "0.001",
            "--timeout",
            "1s",
        ],
    )
    .assert()
    .code(2);

    let error = json_stdout(&assert)["error"].as_str().unwrap().to_string();
    assert!(error.contains("size is too small"), "{error}");
}

#[tokio::test]
async fn bracket_protects_the_whole_entry_when_it_fills_while_resting() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .and(body_partial_json(
            json!({"action": {"grouping": "positionTpsl"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ok",
            "response": {
                "type": "order",
                "data": {"statuses": ["waitingForTrigger", "waitingForTrigger"]}
            }
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    // The entry is on the dex's book when placed and gone at the first poll,
    // with no partial fill seen in between.
    mount_priority_info(
        &server,
        json!({"type": "frontendOpenOrders", "dex": "xyz"}),
        json!([]),
    )
    .await;
    mount_priority_info(
        &server,
        json!({"type": "clearinghouseState"}),
        json!({
            "marginSummary": {
                "accountValue": "10000", "totalNtlPos": "24",
                "totalRawUsd": "10000", "totalMarginUsed": "2"
            },
            "crossMarginSummary": {
                "accountValue": "10000", "totalNtlPos": "24",
                "totalRawUsd": "10000", "totalMarginUsed": "2"
            },
            "crossMaintenanceMarginUsed": "1",
            "withdrawable": "10000",
            "assetPositions": [{
                "type": "oneWay",
                "position": {
                    "coin": "xyz:TOKEN",
                    "szi": "20",
                    "leverage": {"type": "cross", "value": 5},
                    "entryPx": "1.2",
                    "positionValue": "24",
                    "unrealizedPnl": "0",
                    "returnOnEquity": "0",
                    "liquidationPx": null,
                    "marginUsed": "2",
                    "maxLeverage": 10,
                    "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                }
            }],
            "time": 1_700_000_000_000_u64
        }),
    )
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
    .success();

    let document = json_stdout(&assert);
    assert_eq!(document["status"], "armed");
    assert_eq!(document["protected_size"], "20");

    let actions: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/exchange")
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap()["action"].clone())
        .collect();
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert_eq!(actions[1]["grouping"], "positionTpsl");
    for leg in actions[1]["orders"].as_array().unwrap() {
        assert_eq!(leg["s"], "20");
    }
}

async fn mount_priority_info(server: &MockServer, request: Value, response: Value) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(request))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .with_priority(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn cancel_all_for_a_coin_the_dex_does_not_list_fails_instead_of_cancelling_nothing() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"success": null})).await;

    let assert = command_against(
        &env,
        &server,
        &[
            "orders",
            "cancel-all",
            "--coin",
            "NOPE",
            "--dex",
            "xyz",
            "--yes",
        ],
    )
    .assert()
    .code(13);

    let error = json_stdout(&assert)["error"].as_str().unwrap().to_string();
    assert!(error.contains("NOPE"), "{error}");
    let exchange_posts = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/exchange")
        .count();
    assert_eq!(exchange_posts, 0);
}

#[tokio::test]
async fn cancel_all_names_the_dex_in_its_canonical_spelling() {
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
            "XYZ",
            "--yes",
        ],
    )
    .assert()
    .success();

    assert_eq!(json_stdout(&assert)[0]["cancelled_orders"], 1);
    let open = info_requests(&server, "frontendOpenOrders").await;
    assert!(open.iter().all(|body| body["dex"] == "xyz"), "{open:?}");
}

/// An account with nothing on `xyz` and the given raw `userAbstraction`: unless
/// the mode has the exchange move collateral itself, every order there needs a
/// helper transfer first.
async fn mount_unfunded_dex_account_in_mode(server: &MockServer, raw_mode: &str) {
    mount_priority_info(server, json!({"type": "userAbstraction"}), json!(raw_mode)).await;
    mount_priority_info(
        server,
        json!({"type": "clearinghouseState", "dex": "xyz"}),
        json!({
            "marginSummary": {
                "accountValue": "0", "totalNtlPos": "0",
                "totalRawUsd": "0", "totalMarginUsed": "0"
            },
            "crossMarginSummary": {
                "accountValue": "0", "totalNtlPos": "0",
                "totalRawUsd": "0", "totalMarginUsed": "0"
            },
            "crossMaintenanceMarginUsed": "0",
            "withdrawable": "0",
            "assetPositions": [],
            "time": 1_700_000_000_000_u64
        }),
    )
    .await;
}

/// An account that does not pool margin across dexes, with nothing on `xyz`.
async fn mount_unfunded_dex_account(server: &MockServer) {
    mount_unfunded_dex_account_in_mode(server, "default").await;
}

fn helper_transfer(amount: &str) -> Value {
    json!({
        "action": "send_asset",
        "source": "perp",
        "dest": "dex:xyz",
        "token": "USDC",
        "amount": amount,
    })
}

#[tokio::test]
async fn create_dry_run_previews_the_hip3_helper_transfer() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;
    mount_unfunded_dex_account(&server).await;

    let assert = command_against(
        &env,
        &server,
        &[
            "--dry-run",
            "orders",
            "create",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--price",
            "1.2",
            "--size",
            "10",
        ],
    )
    .assert()
    .success();

    // 10 x 1.2 = 12 of notional at 10x leverage with a 20% buffer is 1.44.
    assert_eq!(
        json_stdout(&assert)["args"]["side_effects"],
        json!([helper_transfer("1.44")])
    );
}

#[tokio::test]
async fn modes_where_the_exchange_moves_dex_collateral_need_no_helper_transfer() {
    // dexAbstraction is the discontinued mode in which the exchange moves
    // collateral to the dex an order trades on; the CLI must not also do it.
    for raw_mode in ["portfolioMargin", "dexAbstraction"] {
        let env = IsolatedHome::new();
        let server = hip3_server(json!({"resting": {"oid": OID}})).await;
        mount_unfunded_dex_account_in_mode(&server, raw_mode).await;

        let assert = command_against(
            &env,
            &server,
            &[
                "--dry-run",
                "orders",
                "create",
                "--coin",
                "xyz:TOKEN",
                "--side",
                "buy",
                "--price",
                "1.2",
                "--size",
                "10",
            ],
        )
        .assert()
        .success();

        assert!(
            json_stdout(&assert)["args"].get("side_effects").is_none(),
            "{raw_mode}"
        );
    }
}

#[tokio::test]
async fn a_unified_account_needs_no_helper_transfer() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;

    let assert = command_against(
        &env,
        &server,
        &[
            "--dry-run",
            "orders",
            "create",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--price",
            "1.2",
            "--size",
            "10",
        ],
    )
    .assert()
    .success();

    assert!(json_stdout(&assert)["args"].get("side_effects").is_none());
}

#[tokio::test]
async fn scale_on_a_hip3_market_never_moves_funds() {
    let env = IsolatedHome::new();
    let server = hip3_server(json!({"resting": {"oid": OID}})).await;
    mount_unfunded_dex_account(&server).await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ok",
            "response": {
                "type": "order",
                "data": {"statuses": [{"resting": {"oid": 1}}, {"resting": {"oid": 2}}]}
            }
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    let scale = [
        "orders",
        "scale",
        "--coin",
        "xyz:TOKEN",
        "--side",
        "buy",
        "--start-price",
        "1.2",
        "--end-price",
        "1.1",
        "--total-size",
        "20",
        "--orders",
        "2",
    ];

    let dry_run = command_against(&env, &server, &[&["--dry-run"], &scale[..]].concat())
        .assert()
        .success();
    assert!(json_stdout(&dry_run)["args"].get("side_effects").is_none());

    command_against(&env, &server, &[&scale[..], &["--yes"]].concat())
        .assert()
        .success();

    let types: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/exchange")
        .map(|request| {
            serde_json::from_slice::<Value>(&request.body).unwrap()["action"]["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(types, ["order"]);
}
