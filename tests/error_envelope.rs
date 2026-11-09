mod support;

use predicates::prelude::*;
use serde_json::json;

use support::exchange::ExchangeFixture;
use support::{IsolatedHome, json_command, run_json_failure};

#[tokio::test]
async fn exchange_503_is_unknown_outcome_with_one_post() {
    let home = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server.mount_exchange_http_status(503).await;

    let json = run_json_failure(
        home.live_json(&server)
            .args(["staking", "deposit", "--amount", "3"]),
        12,
    );

    assert_eq!(json["category"], "unavailable");
    assert_eq!(json["exit_code"], 12);
    assert_eq!(json["reason"], "unknown_outcome");
    assert_eq!(json["retryable"], false);
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .starts_with("Can't confirm whether the exchange applied this action."),
        "{json}"
    );
    assert_eq!(json["next"][0]["command"], "hyperliquid orders open");
    assert_eq!(json["next"][1]["command"], "hyperliquid account fills");
    assert_eq!(
        server.exchange_bodies().await.len(),
        1,
        "mutations are never retried"
    );
}

#[tokio::test]
async fn exchange_503_human_output_has_recovery_block() {
    let home = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server.mount_exchange_http_status(503).await;

    home.live_command(&server)
        .args(["--format", "table", "--testnet"])
        .args(["staking", "deposit", "--amount", "3"])
        .assert()
        .code(12)
        .stderr(predicate::str::contains(
            "Error: Can't confirm whether the exchange applied this action.",
        ))
        .stderr(predicate::str::contains(
            "\n\nCheck open orders and recent fills:\n    hyperliquid orders open\n    hyperliquid account fills",
        ));
    assert_eq!(server.exchange_bodies().await.len(), 1);
}

#[tokio::test]
async fn exchange_rejection_with_network_words_is_not_unavailable() {
    let home = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({
            "status": "err",
            "response": "Connection pool timeout while matching dns http 503"
        }))
        .await;

    let json = run_json_failure(
        home.live_json(&server)
            .args(["staking", "deposit", "--amount", "3"]),
        1,
    );

    assert_eq!(json["reason"], "exchange_rejected");
    assert_ne!(json["category"], "unavailable");
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("[untrusted remote data]"),
        "{json}"
    );
}

#[tokio::test]
async fn an_unknown_asset_suggests_a_close_match_in_data_only_when_there_is_one() {
    let home = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let funding = |coin: &str| {
        run_json_failure(
            home.json_command()
                .env(support::API_OVERRIDE_ENV, server.uri())
                .args(["funding", coin]),
            13,
        )
    };

    let near = funding("BTCC");
    assert_eq!(near["category"], "unsupported");
    assert_eq!(near["exit_code"], 13);
    assert_eq!(near["reason"], "asset_not_found");
    assert_eq!(
        near["error"],
        "Couldn't find asset \"BTCC\". Did you mean BTC?"
    );
    assert_eq!(near["data"]["suggestion"], "BTC");
    assert!(
        near.get("next").is_none(),
        "a suggestion is never a command"
    );

    let far = funding("NOTACOIN");
    assert_eq!(far["reason"], "asset_not_found");
    assert_eq!(far["error"], "Couldn't find asset \"NOTACOIN\".");
    assert!(far.get("data").is_none());
}

#[test]
fn a_mistyped_command_suggests_the_fix_and_points_at_its_group_help() {
    let top = run_json_failure(json_command().arg("ordres"), 2);
    assert_eq!(top["reason"], "unknown_command");
    assert_eq!(top["data"]["suggestion"], "orders");

    let nested = run_json_failure(json_command().args(["orders", "cancl"]), 2);
    assert_eq!(nested["reason"], "unknown_command");
    assert_eq!(nested["data"]["suggestion"], "cancel");
    assert_eq!(nested["next"][0]["command"], "hyperliquid orders --help");
}

#[test]
fn auth_required_names_how_to_set_up_a_wallet_in_both_formats() {
    let home = IsolatedHome::new();
    let deposit = ["staking", "deposit", "--amount", "3", "--testnet"];

    home.command()
        .args(["--format", "table"])
        .args(deposit)
        .assert()
        .code(10)
        .stderr(predicate::str::starts_with("Error: Authentication required."))
        .stderr(predicate::str::contains(
            "\n\nSet up a wallet:\n    hyperliquid setup\n    hyperliquid wallet login\nOr set HYPERLIQUID_PRIVATE_KEY, or BANKR_API_KEY with --bankr, for this run.",
        ));

    let json = run_json_failure(home.json_command().args(deposit), 10);
    assert_eq!(json["reason"], "auth_required");
    assert_eq!(json["category"], "auth");
    assert_eq!(json["next"][0]["command"], "hyperliquid setup");
}
