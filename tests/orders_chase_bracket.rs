//! `orders chase` and `orders bracket`: usage errors that suggest a runnable
//! command, duration validation, the precision of the prices they generate and
//! what they leave out of the orders they send.

mod support;

use predicates::prelude::*;
use serde_json::json;
use support::exchange::{ExchangeFixture, order_reply};
use support::orders::{assert_nothing_sent, error_text, suggested_invocation, words};
use support::{IsolatedHome, json_stdout, run_json, stderr_text, wire};

#[tokio::test]
async fn a_bracket_without_a_size_or_amount_suggests_a_command_that_runs() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let assert = env
        .command_with_server(&server)
        .args(words(
            "--dry-run --testnet orders bracket --coin ETH --side buy --take-profit +10% --stop-loss -5%",
        ))
        .assert()
        .code(2);
    let stderr = stderr_text(&assert);
    assert!(
        stderr.contains("--size") || stderr.contains("--amount"),
        "{stderr}"
    );

    let preview = run_json(
        env.command_with_server(&server)
            .args(suggested_invocation(&stderr)),
    );
    assert_eq!(preview["command"], "orders bracket");
    assert_eq!(preview["args"]["coin"], "ETH");
}

#[tokio::test]
async fn a_chase_without_a_timeout_in_agent_mode_suggests_a_command_that_runs() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let assert = env
        .live_json(&server)
        .env("HYPERLIQUID_AGENT", "1")
        .args(words(
            "--dry-run orders chase --coin ETH --side buy --size 0.5",
        ))
        .assert()
        .code(2);
    let error = json_stdout(&assert);
    let message = error_text(&error);
    assert!(message.contains("--timeout"), "{message}");

    let preview = run_json(
        env.command_with_server(&server)
            .args(suggested_invocation(message)),
    );
    assert_eq!(preview["command"], "orders chase");
    assert_eq!(preview["args"]["coin"], "ETH");
}

#[test]
fn a_bracket_refuses_a_zero_entry_timeout() {
    IsolatedHome::new()
        .command()
        .args(words(
            "--dry-run --testnet orders bracket --coin ETH --side buy --size 0.1 --take-profit +10% --stop-loss -5% --entry-timeout 0s",
        ))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("greater than zero"));
}

#[test]
fn a_chase_refuses_an_interval_or_timeout_that_is_not_a_whole_positive_duration() {
    for (flag, value) in [
        ("--interval", "100ms"),
        ("--interval", "0s"),
        ("--timeout", "0s"),
    ] {
        IsolatedHome::new()
            .command()
            .args(words(
                "--dry-run --testnet orders chase --coin ETH --side buy --size 0.5",
            ))
            .args([flag, value])
            .assert()
            .code(2);
    }
}

#[tokio::test]
async fn a_chase_truncates_its_quote_price_and_size() {
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
        .mount_order_statuses(vec![wire::filled_status("0.1234567", 1)])
        .await;

    let result = run_json(env.live_json(&server).args(words(
        "orders chase --coin BTC --side buy --size 0.1234567 --timeout 10s",
    )));

    assert_eq!(result["status"], "filled");
    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    // mid 1234.56789 less 5 bps is 1233.9506...: one price decimal at BTC's 5
    // size decimals, and the size keeps five decimals.
    assert_eq!(actions[0]["orders"][0]["p"], "1233.9");
    assert_eq!(actions[0]["orders"][0]["s"], "0.12345");
}

#[tokio::test]
async fn a_chase_refuses_a_spot_market_before_sending_anything() {
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
        .args(words(
            "orders chase --coin PURR/USDC --side buy --size 10 --timeout 10s",
        ))
        .assert()
        .code(13);

    let document = json_stdout(&assert);
    assert!(
        error_text(&document).contains("orders chase supports perpetual markets only"),
        "{document}"
    );
    assert_nothing_sent(&server).await;
}

/// Chase never sends a builder fee, so a builder default the other order
/// commands reject as half-configured does not stop it.
#[tokio::test]
async fn a_chase_does_not_read_the_default_builder_configuration() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::filled_status("0.5", 1)])
        .await;

    env.live_json(&server)
        .args(words(
            "orders chase --coin BTC --side buy --size 0.5 --timeout 10s",
        ))
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
async fn a_bracket_truncates_its_entry_and_the_protection_prices() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    // The entry fills at once, so the protection legs follow.
    server
        .mount_exchange_once(order_reply(vec![wire::filled_status("0.00012", 1)]))
        .await;
    server
        .mount_order_statuses(vec![wire::resting_status(2), wire::resting_status(3)])
        .await;
    // The protection prices come from the position's entry price.
    let mut state = wire::clearinghouse_state("900", 5);
    state["assetPositions"][0]["position"]["entryPx"] = json!("1234.5678");
    server
        .mount_info_containing(json!({"type": "clearinghouseState"}), state)
        .await;

    let result = run_json(env.live_json(&server).args(words(
        "orders bracket --coin BTC --side buy --entry limit --price 1234.5678 --size 0.000123 --take-profit +10% --stop-loss -5%",
    )));

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
    assert_eq!(result["status"], "armed");
}
