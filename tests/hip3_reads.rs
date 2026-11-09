//! HIP-3 dex scoping on account and market reads.

mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::reads::read_json;
use support::{IsolatedHome, request_bodies, run_json, wire};

const USER: &str = "0x0000000000000000000000000000000000000001";

async fn xyz_exchange() -> ExchangeFixture {
    ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await
}

/// The `/info` reads that named a dex.
async fn dex_scoped_reads(server: &ExchangeFixture) -> Vec<Value> {
    request_bodies(server, "/info")
        .await
        .into_iter()
        .filter(|body| body.get("dex").is_some())
        .collect()
}

fn xyz_state(account_value: &str) -> Value {
    wire::perp_state_with(account_value, "0", "0", account_value, vec![])
}

#[tokio::test]
async fn mids_scopes_all_mids_to_the_dex_in_its_canonical_spelling() {
    let server = xyz_exchange().await;

    let mids = read_json(&server, &["mids", "--dex", "XYZ"]);

    assert_eq!(mids["xyz:XYZ100"], "123.5");
    // The unscoped read is the override probe; the scoped one names the dex as
    // the exchange spells it, not as it was typed.
    assert_eq!(
        server.info_requests("allMids").await,
        [
            json!({"type": "allMids"}),
            json!({"type": "allMids", "dex": "xyz"})
        ]
    );
    assert_eq!(server.info_requests("perpDexs").await.len(), 1);
}

#[tokio::test]
async fn positions_list_scopes_clearinghouse_state_to_a_dex() {
    let server = xyz_exchange().await;
    server
        .mount_info(
            json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"}),
            xyz_state("10"),
        )
        .await;

    let positions = read_json(
        &server,
        &["--ows-signer", USER, "positions", "list", "--dex", "xyz"],
    );

    assert_eq!(positions, json!([]));
    assert_eq!(
        server.info_requests("clearinghouseState").await,
        [json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"})]
    );
}

#[tokio::test]
async fn orders_open_scopes_frontend_open_orders_to_a_dex() {
    let server = xyz_exchange().await;
    server
        .mount_info(
            json!({"type": "frontendOpenOrders", "user": USER, "dex": "xyz"}),
            json!([]),
        )
        .await;

    let orders = read_json(
        &server,
        &["--ows-signer", USER, "orders", "open", "--dex", "xyz"],
    );

    assert_eq!(orders, json!([]));
    assert_eq!(
        server.info_requests("frontendOpenOrders").await,
        [json!({"type": "frontendOpenOrders", "user": USER, "dex": "xyz"})]
    );
}

#[tokio::test]
async fn portfolio_dex_view_skips_account_wide_reads() {
    let server = xyz_exchange().await;
    server
        .mount_info(
            json!({"type": "userAbstraction", "user": USER}),
            json!("disabled"),
        )
        .await;
    server
        .mount_info(
            json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"}),
            xyz_state("42"),
        )
        .await;

    let portfolio = read_json(&server, &["account", "portfolio", USER, "--dex", "xyz"]);

    assert_eq!(portfolio["summary"]["account_value"], "42");
    assert_eq!(portfolio["spot_balances"], json!([]));
    // Spot balances and vault equities are account-wide; a dex view must not
    // report them, so these endpoints are never called.
    assert!(
        server
            .info_requests("spotClearinghouseState")
            .await
            .is_empty()
    );
    assert!(server.info_requests("userVaultEquities").await.is_empty());
}

#[tokio::test]
async fn blank_dex_is_the_default_dex() {
    let server = xyz_exchange().await;

    let mids = read_json(&server, &["mids", "--dex", ""]);

    assert_eq!(mids["BTC"], "50000");
    // The override probe and the command both send the unscoped read: a blank
    // `--dex` must not add a dex field.
    assert_eq!(
        server.info_requests("allMids").await,
        [json!({"type": "allMids"}), json!({"type": "allMids"})]
    );
}

#[tokio::test]
async fn blank_dex_keeps_account_wide_portfolio_rows() {
    let server = xyz_exchange().await;
    server
        .mount_info(
            json!({"type": "userAbstraction", "user": USER}),
            json!("disabled"),
        )
        .await;
    server
        .mount_info(
            json!({"type": "clearinghouseState", "user": USER}),
            xyz_state("11"),
        )
        .await;
    server
        .mount_info(
            json!({"type": "spotClearinghouseState", "user": USER}),
            json!({"balances": [{
                "coin": "USDC", "token": 0, "hold": "0", "total": "7", "entryNtl": "0"
            }]}),
        )
        .await;
    server
        .mount_info(
            json!({"type": "userVaultEquities", "user": USER}),
            json!([]),
        )
        .await;

    let portfolio = read_json(&server, &["account", "portfolio", USER, "--dex", ""]);

    assert_eq!(portfolio["summary"]["account_value"], "11");
    assert_eq!(portfolio["spot_balances"].as_array().unwrap().len(), 1);
    assert_eq!(
        server.info_requests("spotClearinghouseState").await.len(),
        1
    );
    assert_eq!(server.info_requests("userVaultEquities").await.len(), 1);
}

#[tokio::test]
async fn unknown_dex_fails_before_reading() {
    let server = xyz_exchange().await;

    IsolatedHome::new()
        .json_command(&server)
        .args(["mids", "--dex", "nope"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("Unknown DEX"));

    // Refused after the dex list is read and before anything is read from a dex.
    assert_eq!(server.info_requests("perpDexs").await.len(), 1);
    assert_eq!(dex_scoped_reads(&server).await, Vec::<Value>::new());
}

#[tokio::test]
async fn unknown_dex_fails_before_a_watch_starts() {
    let server = xyz_exchange().await;

    for command in ["positions", "orders"] {
        let list = if command == "positions" {
            "list"
        } else {
            "open"
        };
        IsolatedHome::new()
            .json_command(&server)
            .args(["--non-interactive", "--ows-signer", USER, command, list])
            .args(["--watch", "--max-ticks", "1", "--dex", "nope"])
            .assert()
            .code(13)
            .stdout(predicate::str::contains("Unknown DEX"));
    }

    assert_eq!(dex_scoped_reads(&server).await, Vec::<Value>::new());
}

#[tokio::test]
async fn positions_watch_sends_the_canonical_dex() {
    let server = xyz_exchange().await;
    server
        .mount_info(
            json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"}),
            xyz_state("10"),
        )
        .await;

    // `--max-ticks` bounds the watch to its first snapshot.
    let output = run_json(IsolatedHome::new().json_command(&server).args([
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
    ]));

    assert_eq!(output, json!([]));
    assert_eq!(
        server.info_requests("clearinghouseState").await,
        [json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"})]
    );
}
