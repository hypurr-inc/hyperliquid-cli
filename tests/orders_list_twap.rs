mod support;

use predicates::prelude::*;
use support::{
    API_OVERRIDE_ENV, IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY, exchange_actions,
    unix_millis, wire,
};
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn historical_order_update(coin: &str, oid: u64) -> serde_json::Value {
    serde_json::json!({
        "order": wire::basic_order(coin, oid),
        "status": "filled",
        "statusTimestamp": 1700000000001_u64
    })
}

async fn mock_orders_server(
    open_orders: Vec<serde_json::Value>,
    historical_orders: Vec<serde_json::Value>,
) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "BTC": "50000",
            "ETH": "3000"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "spotMeta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(wire::spot_meta_usdc_only()))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "meta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [
                {
                    "name": "BTC",
                    "szDecimals": 5,
                    "maxLeverage": 50,
                    "onlyIsolated": false,
                    "marginMode": null,
                    "growthMode": "disabled"
                },
                {
                    "name": "ETH",
                    "szDecimals": 4,
                    "maxLeverage": 50,
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
        .and(body_partial_json(serde_json::json!({"type": "perpDexs"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "frontendOpenOrders"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(open_orders))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "historicalOrders"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(historical_orders))
        .mount(&server)
        .await;

    server
}

async fn mock_trading_server(exchange_response: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "BTC": "50000",
            "ETH": "3000"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "spotMeta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [],
            "tokens": []
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "meta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [
                {
                    "name": "BTC",
                    "szDecimals": 5,
                    "maxLeverage": 50,
                    "onlyIsolated": false,
                    "marginMode": null,
                    "growthMode": "disabled"
                },
                {
                    "name": "ETH",
                    "szDecimals": 4,
                    "maxLeverage": 50,
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
        .and(body_partial_json(serde_json::json!({"type": "perpDexs"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(exchange_response))
        .mount(&server)
        .await;

    server
}

async fn mock_twap_server_without_exchange() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "BTC": "50000",
            "ETH": "3000"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "spotMeta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [],
            "tokens": []
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "meta"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "universe": [
                {
                    "name": "BTC",
                    "szDecimals": 5,
                    "maxLeverage": 50,
                    "onlyIsolated": false,
                    "marginMode": null,
                    "growthMode": "disabled"
                },
                {
                    "name": "ETH",
                    "szDecimals": 4,
                    "maxLeverage": 50,
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
        .and(body_partial_json(serde_json::json!({"type": "perpDexs"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    server
}

#[tokio::test]
async fn orders_open_pretty_empty_state_uses_theme_colors() {
    let env = IsolatedHome::new();
    let server = mock_orders_server(vec![], vec![]).await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .env("FORCE_COLOR", "1")
        .args(["orders", "open"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[36mCoin\u{1b}[0m"))
        .stdout(predicate::str::contains("No open orders").not())
        .stderr(predicate::str::contains("No open orders."));
}

#[tokio::test]
async fn orders_open_table_and_json_empty_state_are_uncolored() {
    let env = IsolatedHome::new();
    let server = mock_orders_server(vec![], vec![]).await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["--format", "table", "orders", "open"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Coin"))
        .stdout(predicate::str::contains("No open orders").not())
        .stdout(predicate::str::contains("\u{1b}[").not())
        .stderr(predicate::str::contains("No open orders."));

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["--format", "json", "orders", "open"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[]"))
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[tokio::test]
async fn orders_open_shows_open_orders() {
    let env = IsolatedHome::new();
    let server = mock_orders_server(vec![wire::basic_order("BTC", 12345)], vec![]).await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "open"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stdout(predicate::str::contains("12345"));
}

#[tokio::test]
async fn orders_open_selects_coin_price_and_size_json() {
    let env = IsolatedHome::new();
    let server = mock_orders_server(vec![wire::basic_order("BTC", 12345)], vec![]).await;

    let output = env
        .command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "--testnet",
            "--format",
            "json",
            "--select",
            "coin,price,size",
            "orders",
            "open",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let first = json
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap();

    assert_eq!(
        first.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["coin", "price", "size"]
    );
    assert_eq!(first["coin"], "BTC");
    assert_eq!(first["price"], "50000");
    assert_eq!(first["size"], "0.1");
}

#[tokio::test]
async fn orders_history_shows_historical_orders() {
    let env = IsolatedHome::new();
    let server = mock_orders_server(vec![], vec![wire::basic_order("ETH", 54321)]).await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "history"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ETH"))
        .stdout(predicate::str::contains("54321"));
}

#[tokio::test]
async fn orders_history_accepts_wrapped_live_order_updates() {
    let env = IsolatedHome::new();
    let server = mock_orders_server(vec![], vec![historical_order_update("ETH", 54321)]).await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "history"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ETH"))
        .stdout(predicate::str::contains("54321"));
}

#[tokio::test]
async fn orders_twap_create_outputs_twap_id() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "twapOrder",
            "data": {
                "status": {
                    "running": {
                        "twapId": 77738308
                    }
                }
            }
        }
    }))
    .await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "orders",
            "twap-create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--size",
            "1.0",
            "--duration",
            "3600",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("77738308"))
        .stdout(predicate::str::contains("running"))
        .stderr(predicate::str::contains("Mainnet TWAP confirmation required").not());

    let actions = exchange_actions(&server).await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["type"], "twapOrder");
    assert_eq!(actions[0]["twap"]["a"], 0);
    assert_eq!(actions[0]["twap"]["b"], true);
    assert_eq!(actions[0]["twap"]["m"], 60);
    assert_eq!(actions[0]["twap"]["t"], false);
}

#[tokio::test]
async fn orders_twap_create_supports_randomize_flag() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "twapOrder",
            "data": {
                "status": {
                    "running": {
                        "twapId": 77738399
                    }
                }
            }
        }
    }))
    .await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "orders",
            "twap-create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--size",
            "1.0",
            "--duration",
            "3600",
            "--randomize",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("77738399"));

    let actions = exchange_actions(&server).await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["twap"]["t"], true);
}

#[tokio::test]
async fn orders_twap_create_normalizes_size_and_reports_matching_units() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "twapOrder",
            "data": {
                "status": {
                    "running": {
                        "twapId": 77738311
                    }
                }
            }
        }
    }))
    .await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "orders",
            "twap-create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--size",
            "0.00070",
            "--duration",
            "300",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("77738311"))
        .stdout(predicate::str::contains("300"))
        .stdout(predicate::str::contains("5"));

    let requests = server.received_requests().await.unwrap();
    let exchange_request = requests
        .iter()
        .find(|request| request.url.path() == "/exchange")
        .expect("expected twap exchange request");
    let body: serde_json::Value = serde_json::from_slice(&exchange_request.body).unwrap();
    assert_eq!(body["action"]["twap"]["s"], "0.0007");
    assert_eq!(body["action"]["twap"]["m"], 5);
}

#[tokio::test]
async fn orders_twap_create_rejects_unsupported_durations_before_submission() {
    let env = IsolatedHome::new();
    let server = mock_twap_server_without_exchange().await;

    for (duration, message) in [
        ("299", "TWAP duration must be at least 300 seconds"),
        (
            "301",
            "TWAP duration must be a whole-minute value in seconds",
        ),
        ("604860", "cannot exceed 7 days"),
    ] {
        env.command()
            .env(API_OVERRIDE_ENV, server.uri())
            .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
            .args([
                "orders",
                "twap-create",
                "--coin",
                "BTC",
                "--side",
                "buy",
                "--size",
                "1.0",
                "--duration",
                duration,
                "--testnet",
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(message));
    }
}

#[tokio::test]
async fn orders_twap_create_mainnet_prompts_and_aborts_without_submission() {
    let env = IsolatedHome::new();
    let server = mock_twap_server_without_exchange().await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .write_stdin("n\n")
        .args([
            "orders",
            "twap-create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--size",
            "1.0",
            "--duration",
            "3600",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "Mainnet TWAP confirmation required",
        ))
        .stderr(predicate::str::contains("BTC"))
        .stderr(predicate::str::contains("buy"))
        .stderr(predicate::str::contains("1.0"))
        .stderr(predicate::str::contains("3600"))
        .stderr(predicate::str::contains("mainnet"))
        .stderr(predicate::str::contains("TWAP creation cancelled"));
}

#[tokio::test]
async fn orders_twap_create_mainnet_yes_bypasses_confirmation_prompt() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "twapOrder",
            "data": {
                "status": {
                    "running": {
                        "twapId": 77738309
                    }
                }
            }
        }
    }))
    .await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "orders",
            "twap-create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--size",
            "1.0",
            "--duration",
            "3600",
            "--yes",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("77738309"))
        .stderr(predicate::str::contains("Mainnet TWAP confirmation required").not());
}

#[tokio::test]
async fn orders_twap_cancel_requires_coin() {
    let env = IsolatedHome::new();

    env.command()
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "twap-cancel", "42", "--testnet"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--coin"))
        .stderr(predicate::str::contains("required"));
}

#[tokio::test]
async fn twap_cancel_with_non_btc_coin() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "twapCancel",
            "data": {
                "status": "success"
            }
        }
    }))
    .await;

    Mock::given(method("POST"))
        .and(path("/exchange"))
        .and(body_string_contains("\"type\":\"twapCancel\""))
        .and(body_string_contains("\"a\":1"))
        .and(body_string_contains("\"t\":42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": {
                "type": "twapCancel",
                "data": {
                    "status": "success"
                }
            }
        })))
        .with_priority(1)
        .expect(1)
        .named("ETH twapCancel exchange request")
        .mount(&server)
        .await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "twap-cancel", "42", "--coin", "ETH", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ETH"))
        .stdout(predicate::str::contains("42"))
        .stdout(predicate::str::contains("cancelled"));

    let requests = server.received_requests().await.unwrap();
    let exchange_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/exchange")
        .collect();
    assert_eq!(
        exchange_requests.len(),
        1,
        "expected exactly one /exchange request"
    );

    let body: serde_json::Value = serde_json::from_slice(&exchange_requests[0].body).unwrap();
    assert_eq!(body["action"]["type"], "twapCancel");
    assert_eq!(body["action"]["a"], 1);
    assert_eq!(body["action"]["t"], 42);
}

#[tokio::test]
async fn orders_schedule_cancel_mainnet_prompts_and_aborts_without_submission() {
    let env = IsolatedHome::new();
    let server = mock_twap_server_without_exchange().await;

    for flags in [&["--in", "5m"][..], &["--clear"][..]] {
        env.command()
            .env(API_OVERRIDE_ENV, server.uri())
            .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
            .write_stdin("n\n")
            .args(["orders", "schedule-cancel"])
            .args(flags)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(
                "Mainnet schedule-cancel confirmation required",
            ))
            .stderr(predicate::str::contains("action cancelled"));
    }
}

#[test]
fn orders_schedule_cancel_requires_yes_without_an_interactive_terminal() {
    let env = IsolatedHome::new();

    for (agent, flags) in [(false, &["--in", "5m"][..]), (true, &["--clear"][..])] {
        let mut command = env.command();
        command.env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY);
        if agent {
            command.env("HYPERLIQUID_AGENT", "1");
        }
        let output = command
            .args(["--format", "json", "orders", "schedule-cancel"])
            .args(flags)
            .assert()
            .code(13)
            .get_output()
            .stdout
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(json["category"], "unsupported", "{flags:?}");
        assert_eq!(json["exit_code"], 13, "{flags:?}");
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("without confirmation in non-interactive mode"),
            "{flags:?}"
        );
    }
}

#[tokio::test]
async fn orders_schedule_cancel_mainnet_yes_bypasses_confirmation_prompt() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "default"
        }
    }))
    .await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "schedule-cancel", "--in", "5m", "--yes"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Mainnet schedule-cancel confirmation required").not());

    let actions = exchange_actions(&server).await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["type"], "scheduleCancel");
}

#[tokio::test]
async fn orders_schedule_cancel_outputs_scheduled_time() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "default"
        }
    }))
    .await;

    let before = unix_millis();
    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "schedule-cancel", "--in", "5m", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("scheduled"))
        .stdout(predicate::str::contains("Scheduled Time"));
    let after = unix_millis();

    let requests = server.received_requests().await.unwrap();
    let exchange_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/exchange")
        .collect();
    assert_eq!(exchange_requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&exchange_requests[0].body).unwrap();
    assert_eq!(body["action"]["type"], "scheduleCancel");
    let time = body["action"]["time"]
        .as_u64()
        .expect("scheduled time in milliseconds");
    assert!(
        (before + 299_000..=after + 301_000).contains(&time),
        "{time} is not five minutes after the request"
    );
    assert!(body["vaultAddress"].is_null());
}

#[tokio::test]
async fn orders_schedule_cancel_clear_outputs_cleared_status() {
    let env = IsolatedHome::new();
    let server = mock_trading_server(serde_json::json!({
        "status": "ok",
        "response": {
            "type": "default"
        }
    }))
    .await;

    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["orders", "schedule-cancel", "--clear", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cleared"));

    let requests = server.received_requests().await.unwrap();
    let exchange_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/exchange")
        .collect();
    assert_eq!(exchange_requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&exchange_requests[0].body).unwrap();
    assert_eq!(body["action"]["type"], "scheduleCancel");
    assert!(body["action"]["time"].is_null());
}

#[tokio::test]
async fn orders_schedule_cancel_on_behalf_sets_vault_address() {
    let env = IsolatedHome::new();
    let subaccount = "0x0000000000000000000000000000000000000123";

    for (flags, clears) in [(&["--in", "5m"][..], false), (&["--clear"][..], true)] {
        let server = mock_trading_server(serde_json::json!({
            "status": "ok",
            "response": {
                "type": "default"
            }
        }))
        .await;

        env.command()
            .env(API_OVERRIDE_ENV, server.uri())
            .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
            .args(["orders", "schedule-cancel"])
            .args(flags)
            .args(["--on-behalf-of", subaccount, "--testnet"])
            .assert()
            .success();

        let requests = server.received_requests().await.unwrap();
        let exchange = requests
            .iter()
            .find(|request| request.url.path() == "/exchange")
            .expect("expected exchange request");
        let body: serde_json::Value = serde_json::from_slice(&exchange.body).unwrap();
        assert_eq!(body["vaultAddress"], subaccount, "{flags:?}");
        assert_eq!(body["action"]["time"].is_null(), clears, "{flags:?}");
    }
}

#[test]
fn orders_twap_create_missing_params_exits_2() {
    let env = IsolatedHome::new();
    env.command()
        .args(["orders", "twap-create", "--coin", "BTC", "--side", "buy"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--size"))
        .stderr(predicate::str::contains("--duration"));
}
