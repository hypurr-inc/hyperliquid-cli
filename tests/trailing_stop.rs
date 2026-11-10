//! `orders trailing-stop`: the signed action, the mainnet prompt, the dry-run
//! plan and how the stop is listed afterwards.

mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::orders::{assert_nothing_sent, error_text, mainnet_command, testnet_command, words};
use support::signing::{recover_signer, test_signer};
use support::{IsolatedHome, json_stdout, run_json};

const OID: u64 = 77_738_308;
const STOP: &str = "orders trailing-stop --coin BTC --side sell --size 0.01 --retracement-pct 5";
const VAULT: &str = "0x00000000000000000000000000000000000000aa";

fn trailing_stop_ok(oid: u64) -> Value {
    json!({
        "status": "ok",
        "response": {"type": "trailingStop", "data": {"oid": oid}}
    })
}

async fn placing_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server.mount_exchange(trailing_stop_ok(OID)).await;
    server
}

/// What the CLI sent besides the `allMids` read every API override gets.
async fn requests_beyond_the_probe(server: &ExchangeFixture) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            format!(
                "{} {}",
                request.url.path(),
                body["type"].as_str().unwrap_or("")
            )
        })
        .filter(|summary| summary != "/info allMids")
        .collect()
}

#[tokio::test]
async fn a_testnet_submit_posts_one_signed_trailing_stop_action_and_prints_the_oid() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    let rows = run_json(
        env.live_json(&server)
            .args(words(&format!("{STOP} --reduce-only"))),
    );

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1, "exactly one /exchange request");
    let body = &bodies[0];
    assert_eq!(
        body["action"],
        json!({
            "type": "trailingStop",
            "asset": 0,
            "isBuy": false,
            "sz": "0.01",
            "reduceOnly": true,
            "retracement": {"pct": "5%"},
            "activationPx": null,
        })
    );
    assert!(body["vaultAddress"].is_null());
    assert_eq!(recover_signer(body, Chain::Testnet), test_signer());

    let receipt = &rows[0];
    assert_eq!(receipt["oid"], OID);
    assert_eq!(receipt["coin"], "BTC");
    assert_eq!(receipt["side"], "sell");
    assert_eq!(receipt["size"], "0.01");
    assert_eq!(receipt["reduce_only"], true);
    assert_eq!(receipt["retracement"], json!({"pct": "5"}));
    assert!(receipt["activation_px"].is_null());
    assert_eq!(receipt["status"], "waiting_for_trigger");
}

#[tokio::test]
async fn a_price_distance_and_an_activation_price_use_their_wire_values() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    let rows = run_json(env.live_json(&server).args(words(
        "orders trailing-stop --coin ETH --side buy --size 0.5 --retracement-px 50.50 --activation-price 2900",
    )));

    assert_eq!(
        server.exchange_actions().await,
        [json!({
            "type": "trailingStop",
            "asset": 1,
            "isBuy": true,
            "sz": "0.5",
            "reduceOnly": false,
            "retracement": {"px": "50.5"},
            "activationPx": "2900",
        })]
    );
    assert_eq!(rows[0]["retracement"], json!({"px": "50.5"}));
    assert_eq!(rows[0]["activation_px"], "2900");
}

#[tokio::test]
async fn on_behalf_of_sends_the_vault_address() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    run_json(env.live_json(&server).args(words(
        &format!("orders trailing-stop --coin BTC --side sell --size 0.01 --retracement-pct 2.5% --on-behalf-of {VAULT}"),
    )));

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["vaultAddress"], VAULT);
    assert_eq!(bodies[0]["action"]["retracement"], json!({"pct": "2.5%"}));
}

#[tokio::test]
async fn pretty_output_lists_the_receipt_and_the_cancel_command() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    testnet_command(&env, &server, STOP)
        .assert()
        .success()
        .stdout(predicate::str::contains("77738308"))
        .stdout(predicate::str::contains("waiting_for_trigger"))
        .stdout(predicate::str::contains("5%"))
        .stderr(predicate::str::contains("Cancel it:"))
        .stderr(predicate::str::contains(
            "hyperliquid orders cancel 77738308",
        ));
}

#[tokio::test]
async fn bad_retracement_and_market_arguments_are_refused_before_any_request() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    let bad_retracements = [
        "",
        "--retracement-pct 5 --retracement-px 100",
        "--retracement-pct 0",
        "--retracement-pct 100",
        "--retracement-pct -5",
        "--retracement-px 0",
        "--retracement-px -1",
    ];
    for retracement in bad_retracements {
        let line = format!("orders trailing-stop --coin BTC --side sell --size 0.01 {retracement}");
        testnet_command(&env, &server, &line).assert().code(2);
    }
    for coin in ["HYPE/USDC", "#10", "+10"] {
        let line =
            format!("orders trailing-stop --coin {coin} --side sell --size 1 --retracement-pct 5");
        testnet_command(&env, &server, &line)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("perpetual markets only"));
    }

    assert_eq!(
        requests_beyond_the_probe(&server).await,
        Vec::<String>::new(),
        "usage errors, spot and outcome coins must fail before any lookup or exchange request"
    );
}

#[tokio::test]
async fn mainnet_json_without_yes_requires_confirmation_and_sends_nothing() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    let assert = mainnet_command(&env, &server, STOP)
        .args(["--format", "json"])
        .assert()
        .code(13);

    assert_eq!(json_stdout(&assert)["reason"], "confirmation_required");
    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn the_mainnet_prompt_shows_every_consent_row_and_declining_sends_nothing() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    mainnet_command(&env, &server, &format!("{STOP} --reduce-only"))
        .write_stdin("n\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Network: mainnet"))
        .stderr(predicate::str::contains("Signer: "))
        .stderr(predicate::str::contains("Coin: BTC"))
        .stderr(predicate::str::contains("Side: sell"))
        .stderr(predicate::str::contains("Size: 0.01"))
        .stderr(predicate::str::contains("Retracement: 5%"))
        .stderr(predicate::str::contains("starts tracking immediately"))
        .stderr(predicate::str::contains("Reduce-only: yes"))
        .stderr(predicate::str::contains(
            "Place this trailing stop on mainnet?",
        ))
        .stderr(predicate::str::contains(
            "trailing stop placement cancelled",
        ));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn accepting_the_mainnet_prompt_or_passing_yes_places_the_stop() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    mainnet_command(
        &env,
        &server,
        "orders trailing-stop --coin BTC --side buy --size 0.01 --retracement-px 100 --activation-price 49000",
    )
    .write_stdin("yes\n")
    .assert()
    .success()
    .stderr(predicate::str::contains("Activation: 49000"))
    .stderr(predicate::str::contains(
        "Retracement: 100 from the lowest mark price",
    ))
    .stdout(predicate::str::contains("77738308"));
    mainnet_command(&env, &server, &format!("{STOP} --yes"))
        .assert()
        .success()
        .stderr(predicate::str::contains("Network: mainnet").not());

    assert_eq!(server.exchange_actions().await.len(), 2);
}

#[tokio::test]
async fn a_dry_run_makes_no_exchange_request_and_reports_the_plan() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    let envelope = run_json(env.live_json(&server).args(words(&format!(
        "--dry-run orders trailing-stop --coin ETH --side sell --size 0.5 --retracement-pct 5 --reduce-only --activation-price 3500 --on-behalf-of {VAULT}"
    ))));

    assert_nothing_sent(&server).await;
    assert_eq!(envelope["command"], "orders trailing-stop");
    assert_eq!(envelope["would_execute"], "trailing_stop");
    assert!(envelope["network"].is_string(), "{envelope}");
    let args = &envelope["args"];
    assert_eq!(args["coin"], "ETH");
    assert_eq!(args["asset_id"], 1);
    assert_eq!(args["side"], "sell");
    assert_eq!(args["size"], "0.5");
    assert_eq!(args["reduce_only"], true);
    assert_eq!(args["retracement"], json!({"pct": "5"}));
    assert_eq!(args["activation_px"], "3500");
    assert_eq!(args["on_behalf_of"], VAULT);
}

#[tokio::test]
async fn a_pretty_dry_run_lists_the_arguments() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    mainnet_command(
        &env,
        &server,
        "--dry-run orders trailing-stop --coin BTC --side sell --size 0.01 --retracement-px 150",
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("trailing_stop"))
    .stdout(predicate::str::contains("Retracement"));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn an_exchange_rejection_is_labelled_untrusted_remote_data_and_not_retried() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({
            "status": "err",
            "response": "Trailing stops are disabled\u{1b}[31m for this asset"
        }))
        .await;

    let assert = env.live_json(&server).args(words(STOP)).assert().code(1);

    let error = json_stdout(&assert);
    let message = error_text(&error);
    assert!(message.contains("[untrusted remote data]"), "{message}");
    assert!(message.contains("Trailing stops are disabled"), "{message}");
    assert!(!message.contains('\u{1b}'), "{message:?}");
    assert_eq!(server.exchange_actions().await.len(), 1, "never retried");
}

#[tokio::test]
async fn an_ok_answer_without_an_oid_is_an_unknown_outcome() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;

    let assert = env.live_json(&server).args(words(STOP)).assert().code(12);

    assert_eq!(json_stdout(&assert)["reason"], "unknown_outcome");
}

#[tokio::test]
async fn open_orders_and_history_render_trailing_stop_markets() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let mut order = support::wire::basic_order("BTC", OID);
    order["side"] = json!("A");
    order["limitPx"] = json!("0.0");
    order["sz"] = json!("0.01");
    order["origSz"] = json!("0.01");
    order["orderType"] = json!("Trailing Stop Market");
    order["tif"] = Value::Null;
    order["reduceOnly"] = json!(true);
    order["isTrigger"] = json!(true);
    order["triggerPx"] = json!("0.0");
    order["triggerCondition"] = json!("N/A");
    order["isPositionTpsl"] = json!(false);
    server
        .mount_info_containing(json!({"type": "frontendOpenOrders"}), json!([order]))
        .await;
    server
        .mount_info_containing(
            json!({"type": "historicalOrders"}),
            json!([{"order": order, "status": "open", "statusTimestamp": 1_700_000_000_001_u64}]),
        )
        .await;

    for subcommand in ["open", "history"] {
        let rows = run_json(env.live_json(&server).args(["orders", subcommand]));
        assert_eq!(rows[0]["order_type"], "TrailingStopMarket", "{subcommand}");
        assert_eq!(rows[0]["oid"], OID);
    }
}
