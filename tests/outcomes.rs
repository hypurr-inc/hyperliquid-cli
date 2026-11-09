//! Trading an outcome side by its `#<encoding>` notation. Listing and
//! looking up outcomes is in `outcome_reads.rs`.

mod support;

use predicates::prelude::*;
use serde_json::json;
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, run_json, wire};

/// An exchange listing outcome 1, whose sides are encoded `#10` and `#11`.
async fn outcome_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), wire::outcome_meta(&[1]))
        .await;
    server
}

const LIMIT_BUY: [&str; 12] = [
    "orders", "create", "--coin", "#10", "--side", "buy", "--price", "0.5", "--size", "1", "--tif",
    "alo",
];

#[test]
fn outcomes_get_rejects_malformed_notation_before_api_call() {
    IsolatedHome::new()
        .command()
        .args(["outcomes", "get", "#abc"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "expected #<encoding> or +<encoding>",
        ));
}

#[tokio::test]
async fn orders_live_outcome_trading_submits_encoded_asset_id() {
    let server = outcome_exchange().await;
    server
        .mount_order_statuses(vec![json!({"resting": {"oid": 123}})])
        .await;

    let receipt = run_json(IsolatedHome::new().live_json(&server).args(LIMIT_BUY));

    assert_eq!(receipt[0]["order_id"], 123);
    assert_eq!(receipt[0]["coin"], "#10");
    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["orders"][0]["a"], 100_000_010);
}

#[tokio::test]
async fn orders_outcome_notation_is_trimmed_and_refuses_a_dex_with_a_specific_message() {
    let server = outcome_exchange().await;
    let home = IsolatedHome::new();

    let mut padded = LIMIT_BUY;
    padded[3] = " #10";
    let preview = run_json(home.json_command_for(&server).arg("--dry-run").args(padded));
    assert_eq!(preview["args"]["resolved_asset"], "#10");

    home.command_with_server(&server)
        .args(["--dry-run", "orders", "create", "--dex", "foo"])
        .args(&LIMIT_BUY[2..])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("cannot be used with --dex"));
}

#[tokio::test]
async fn orders_outcome_market_dry_run_is_rejected_until_decimals_are_verified() {
    let server = outcome_exchange().await;

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["--dry-run", "orders", "create", "--coin", "#10"])
        .args(["--side", "buy", "--type", "market", "--amount", "1"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "outcome size decimals are not verified",
        ));
}

#[tokio::test]
async fn orders_outcome_dry_run_exposes_verified_encoding() {
    let server = outcome_exchange().await;

    let preview = run_json(
        IsolatedHome::new()
            .json_command_for(&server)
            .arg("--dry-run")
            .args(LIMIT_BUY),
    );

    assert_eq!(preview["args"]["outcome"]["encoding"], 10);
    assert_eq!(preview["args"]["outcome"]["asset_id"], 100_000_010);
    assert!(
        preview["args"]["outcome"]["live_submission"]
            .as_str()
            .unwrap()
            .contains("enabled")
    );
}
