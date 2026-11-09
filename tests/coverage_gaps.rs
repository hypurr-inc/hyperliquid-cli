//! Behavior the changelog promises that no other test pinned: the lesser-used
//! commands' dry-run names, `subaccount list`, order precision on the
//! generated-price paths, chase's market rule, the exit code each command gives
//! a rate-limit or credential refusal, `orders history`'s raw fallback, and the
//! `report-incident` labels.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, order_reply};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout, wire};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

const RATE_LIMIT: &str = "Too many requests: rate limit exceeded";
const CREDENTIALS: &str = "Unauthorized: invalid key";

// ---- dry-run names, subaccount list, report-incident ----

#[tokio::test]
async fn outcome_shortcuts_preview_under_their_own_command_name() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), wire::outcome_meta(&[10]))
        .await;

    for (command, side) in [("outcome-buy", "buy"), ("outcome-sell", "sell")] {
        let assert = env
            .command_with_server(&server)
            .args([
                "--format",
                "json",
                "--dry-run",
                command,
                "--coin",
                "#10",
                "--price",
                "0.5",
                "--size",
                "1",
            ])
            .assert()
            .success();

        let preview = json_stdout(&assert);
        assert_eq!(preview["command"], command);
        assert_eq!(preview["would_execute"], "submit_order");
        assert_eq!(preview["args"]["side"], side);
        assert_eq!(preview["args"]["outcome"]["asset_id"], 100000010);
    }
}

#[test]
fn outcome_shortcuts_refuse_a_market_that_is_not_an_outcome() {
    let env = IsolatedHome::new();

    for command in ["outcome-buy", "outcome-sell"] {
        env.command()
            .args([
                "--format",
                "json",
                "--dry-run",
                command,
                "--coin",
                "BTC",
                "--price",
                "50000",
                "--size",
                "1",
            ])
            .assert()
            .code(13)
            .stdout(predicates::str::contains("invalid outcome notation 'BTC'"));
    }
}

#[tokio::test]
async fn subaccount_list_prints_the_accounts_of_a_master_address() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let master = expected_address(VALID_PRIVATE_KEY);
    let subaccount = "0x00000000000000000000000000000000000000aa";
    server
        .mount_info_containing(
            json!({"type": "subAccounts", "user": master.to_lowercase()}),
            json!([{
                "name": "market-maker-1",
                "subAccountUser": subaccount,
                "master": master,
                "clearinghouseState": wire::clearinghouse_state("900", 5),
                "spotState": {"balances": []}
            }]),
        )
        .await;

    let assert = env
        .command_with_server(&server)
        .args(["--format", "json", "subaccount", "list", &master])
        .assert()
        .success();

    let rows = json_stdout(&assert);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["name"], "market-maker-1");
    assert_eq!(
        rows[0]["address"].as_str().unwrap().to_lowercase(),
        subaccount
    );
    assert_eq!(rows[0]["withdrawable"], "900");
    assert_eq!(rows[0]["positions_count"], 1);
    assert_eq!(server.info_requests("subAccounts").await.len(), 1);
}

#[test]
fn report_incident_labels_the_issue_bug_and_agent_incident() {
    let env = IsolatedHome::new();

    let assert = env
        .command()
        .args(["--format", "json", "report-incident", "--error", "boom"])
        .assert()
        .success();

    let url = json_stdout(&assert)["url"].as_str().unwrap().to_string();
    assert!(
        url.ends_with("&labels=bug%2Cagent-incident"),
        "the labels end the URL: {url}"
    );
}

// ---- price and size precision on generated orders ----

#[tokio::test]
async fn batch_create_truncates_each_leg_to_its_market_precision() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1), wire::resting_status(2)])
        .await;
    let orders = env.tmp_path().join("orders.json");
    std::fs::write(
        &orders,
        json!({"orders": [
            {"coin": "BTC", "side": "buy", "price": "1234.5678", "size": "0.000123"},
            {"coin": "ETH", "side": "sell", "price": "3000.123456", "size": "0.01239"},
        ]})
        .to_string(),
    )
    .unwrap();

    env.live_json(&server)
        .args(["orders", "batch-create", "--orders-file", "orders.json"])
        .current_dir(env.tmp_path())
        .assert()
        .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    let legs = &actions[0]["orders"];
    // BTC: szDecimals 5 -> one price decimal, five decimals of size.
    assert_eq!(legs[0]["p"], "1234.5");
    assert_eq!(legs[0]["s"], "0.00012");
    // ETH: szDecimals 4 -> five significant figures, four decimals of size.
    assert_eq!(legs[1]["p"], "3000.1");
    assert_eq!(legs[1]["s"], "0.0123");
}

#[tokio::test]
async fn batch_create_rejects_a_leg_that_truncates_to_zero_before_sending() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    let orders = env.tmp_path().join("orders.json");
    std::fs::write(
        &orders,
        json!({"orders": [{"coin": "BTC", "side": "buy", "price": "50000", "size": "0.000001"}]})
            .to_string(),
    )
    .unwrap();

    env.live_json(&server)
        .args(["orders", "batch-create", "--orders-file", "orders.json"])
        .current_dir(env.tmp_path())
        .assert()
        .code(2)
        .stdout(predicates::str::contains("BTC"));

    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn chase_truncates_its_quote_price_and_size() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "allMids"}),
            json!({"BTC": "1234.56789", "ETH": "3000"}),
        )
        .await;
    // The quote is reported filled in full, which ends the chase at once.
    server
        .mount_exchange(order_reply(vec![wire::filled_status("0.1234567", 1)]))
        .await;

    let assert = env
        .live_json(&server)
        .args([
            "orders",
            "chase",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--size",
            "0.1234567",
            "--timeout",
            "10s",
        ])
        .assert()
        .success();

    assert_eq!(json_stdout(&assert)["status"], "filled");
    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    let order = &actions[0]["orders"][0];
    // mid 1234.56789 less 5 bps is 1233.9506...: one price decimal at BTC's 5
    // size decimals, and the size keeps five decimals.
    assert_eq!(order["p"], "1233.9");
    assert_eq!(order["s"], "0.12345");
}

#[tokio::test]
async fn chase_refuses_a_spot_market_before_sending_anything() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({
                "universe": [{"name": "PURR/USDC", "index": 0, "tokens": [1, 0]}],
                "tokens": [
                    {"name": "USDC", "index": 0, "tokenId": "0x00000000000000000000000000000000",
                     "szDecimals": 6, "weiDecimals": 6, "evmContract": null},
                    {"name": "PURR", "index": 1, "tokenId": "0x00000000000000000000000000000001",
                     "szDecimals": 0, "weiDecimals": 0, "evmContract": null}
                ]
            }),
        )
        .await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;

    let assert = env
        .live_json(&server)
        .args([
            "orders",
            "chase",
            "--coin",
            "PURR/USDC",
            "--side",
            "buy",
            "--size",
            "10",
            "--timeout",
            "10s",
        ])
        .assert()
        .code(13);

    let error = json_stdout(&assert)["error"].as_str().unwrap().to_string();
    assert!(
        error.contains("orders chase supports perpetual markets only"),
        "{error}"
    );
    assert!(server.exchange_actions().await.is_empty());
}

/// Chase never sends a builder fee, so a builder default the other order
/// commands reject as half-configured does not stop it.
#[tokio::test]
async fn chase_does_not_read_the_default_builder_configuration() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(order_reply(vec![wire::filled_status("0.5", 1)]))
        .await;

    env.live_json(&server)
        .args([
            "orders",
            "chase",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--size",
            "0.5",
            "--timeout",
            "10s",
        ])
        .env(
            hyperliquid_cli::env::DEFAULT_BUILDER_ADDRESS,
            "0x00000000000000000000000000000000000000bb",
        )
        .assert()
        .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert!(actions[0]["builder"].is_null(), "{}", actions[0]);
}

#[tokio::test]
async fn bracket_truncates_its_entry_and_the_protection_prices() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    // The entry fills at once, so the protection legs follow.
    server
        .mount_exchange_once(order_reply(vec![wire::filled_status("0.00012", 1)]))
        .await;
    server
        .mount_exchange(order_reply(vec![
            wire::resting_status(2),
            wire::resting_status(3),
        ]))
        .await;
    // The protection prices come from the position's entry price.
    let mut state = wire::clearinghouse_state("900", 5);
    state["assetPositions"][0]["position"]["entryPx"] = json!("1234.5678");
    server
        .mount_info_containing(json!({"type": "clearinghouseState"}), state)
        .await;

    let assert = env
        .live_json(&server)
        .args([
            "orders",
            "bracket",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--entry",
            "limit",
            "--price",
            "1234.5678",
            "--size",
            "0.000123",
            "--take-profit",
            "+10%",
            "--stop-loss",
            "-5%",
        ])
        .assert()
        .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 2, "entry, then the protection legs");
    let entry = &actions[0]["orders"][0];
    assert_eq!(entry["p"], "1234.5");
    assert_eq!(entry["s"], "0.00012");
    let protection = actions[1]["orders"].as_array().unwrap();
    assert_eq!(protection.len(), 2);
    // Entry 1234.5678: +10% is 1358.02458 and -5% is 1172.82...; one price decimal.
    assert_eq!(protection[0]["t"]["trigger"]["triggerPx"], "1358");
    assert_eq!(protection[0]["p"], "1358");
    assert_eq!(protection[1]["t"]["trigger"]["triggerPx"], "1172.8");
    assert_eq!(protection[1]["p"], "1172.8");
    assert_eq!(json_stdout(&assert)["status"], "armed");
}

// ---- rate-limit and credential exit codes ----

/// Every command that reads an exchange status gives a rate limit exit 11 and
/// a refused credential exit 10, and any other rejection neither.
async fn assert_rejection_exit_codes(
    args: &[&str],
    prepare: impl AsyncFn(&ExchangeFixture),
    reply: impl Fn(&str) -> Value,
) {
    for (text, expected) in [
        (RATE_LIMIT, Some(11)),
        (CREDENTIALS, Some(10)),
        ("Order has invalid size.", None),
    ] {
        let env = IsolatedHome::new();
        let server = ExchangeFixture::start().await;
        prepare(&server).await;
        server.mount_exchange(reply(text)).await;

        let assert = env.live_json(&server).args(args).assert();
        let code = assert.get_output().status.code().unwrap();
        match expected {
            Some(expected) => assert_eq!(code, expected, "{args:?} on {text:?}"),
            None => assert!(
                code != 10 && code != 11 && code != 0,
                "{args:?} on {text:?} exited {code}"
            ),
        }
        assert_eq!(
            server.exchange_actions().await.len(),
            1,
            "{args:?} sends one action and does not retry a rejection"
        );
    }
}

#[tokio::test]
async fn cancel_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        &["orders", "cancel", "12345"],
        async |server| {
            server
                .mount_info_containing(json!({"type": "orderStatus"}), wire::order_status_reply(wire::basic_order("BTC", 12345)))
                .await;
        },
        |text| json!({"status": "ok", "response": {"type": "cancel", "data": {"statuses": [{"error": text}]}}}),
    )
    .await;
}

#[tokio::test]
async fn modify_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        &[
            "orders", "modify", "12345", "--price", "51000", "--size", "0.05",
        ],
        async |server| {
            server
                .mount_info_containing(
                    json!({"type": "orderStatus"}),
                    wire::order_status_reply(wire::basic_order("BTC", 12345)),
                )
                .await;
        },
        |text| order_reply(vec![json!({"error": text})]),
    )
    .await;
}

#[tokio::test]
async fn twap_create_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        &[
            "orders", "twap-create", "--coin", "BTC", "--side", "buy", "--size", "1.0",
            "--duration", "3600",
        ],
        async |_| {},
        |text| json!({"status": "ok", "response": {"type": "twapOrder", "data": {"status": {"error": text}}}}),
    )
    .await;
}

#[tokio::test]
async fn tpsl_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        &[
            "orders",
            "tpsl",
            "--coin",
            "BTC",
            "--side",
            "sell",
            "--size",
            "0.1",
            "--take-profit",
            "6100",
            "--stop-loss",
            "4900",
        ],
        async |_| {},
        |text| order_reply(vec![json!({"error": text}), json!({"error": text})]),
    )
    .await;
}

#[tokio::test]
async fn trailing_stop_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        &[
            "orders",
            "trailing-stop",
            "--coin",
            "BTC",
            "--side",
            "sell",
            "--size",
            "0.01",
            "--retracement-pct",
            "5",
        ],
        async |_| {},
        |text| json!({"status": "err", "response": text}),
    )
    .await;
}

// ---- orders history ----

fn history_mock(response: ResponseTemplate) -> Mock {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(json!({"type": "historicalOrders"})))
        .respond_with(response)
}

/// A bare order list is not the wrapped shape the SDK decodes: only that
/// mismatch sends the CLI to the raw read.
#[tokio::test]
async fn orders_history_falls_back_to_the_raw_list_on_a_shape_mismatch() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    history_mock(ResponseTemplate::new(200).set_body_json(json!([wire::basic_order("BTC", 5)])))
        .mount(&server)
        .await;

    let assert = env
        .live_command(&server)
        .args(["--format", "json", "orders", "history"])
        .assert()
        .success();

    assert_eq!(json_stdout(&assert)[0]["oid"], 5);
    assert_eq!(
        server.info_requests("historicalOrders").await.len(),
        2,
        "the SDK read, then the raw read"
    );
}

#[tokio::test]
async fn orders_history_does_not_retry_a_malformed_body_as_a_raw_list() {
    // Cut off mid-document (an EOF error) and not JSON at all (a syntax error).
    for body in [r#"[{"order": {"coin": "BTC""#, "<html>bad gateway</html>"] {
        let env = IsolatedHome::new();
        let server = ExchangeFixture::start().await;
        history_mock(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let assert = env
            .live_command(&server)
            .args(["--format", "json", "orders", "history"])
            .assert()
            .failure();

        assert!(!assert.get_output().stdout.is_empty());
        assert_eq!(
            server.info_requests("historicalOrders").await.len(),
            1,
            "{body}: a body that is not JSON is an API failure, not a shape to retry"
        );
    }
}
