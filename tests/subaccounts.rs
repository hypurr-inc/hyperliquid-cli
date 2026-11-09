mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::signing::{recover_signer, test_signer};
use support::{IsolatedHome, TEST_ACCOUNT_PASSPHRASE, expected_address, wire};

const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
const SUBACCOUNT_ADDRESS: &str = "0x0000000000000000000000000000000000000001";
const SUBACCOUNT_KEY: &str = "0x000000000000000000000000000000000000000000000000000000000000000a";

async fn exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// The one request `server` received, signed by the test key as an L1 action.
async fn signed_l1_body(server: &ExchangeFixture) -> Value {
    let mut bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    let body = bodies.remove(0);
    assert_eq!(recover_signer(&body, Chain::Testnet), test_signer());
    body
}

#[tokio::test]
async fn subaccount_create_submits_sdk_matching_action_shape() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    env.live_json(&server)
        .args(["subaccount", "create", "--name", "market-maker-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("market-maker-1"));

    let body = signed_l1_body(&server).await;
    assert_eq!(
        body["action"],
        json!({"type": "createSubAccount", "name": "market-maker-1"})
    );
    assert!(body["vaultAddress"].is_null());
}

#[tokio::test]
async fn subaccount_transfer_resolves_alias_and_uses_usdc_integer_units() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let subaccount_address = expected_address(SUBACCOUNT_KEY);

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args([
            "account",
            "add",
            SUBACCOUNT_KEY,
            "--alias",
            "market-maker-1",
            "--type",
            "subaccount",
        ])
        .assert()
        .success();

    let mut command = env.live_json(&server);
    env.apply_account_env(&mut command, Some(TEST_ACCOUNT_PASSPHRASE));
    let json = support::run_json(command.args([
        "subaccount",
        "transfer",
        "--subaccount",
        "market-maker-1",
        "--amount",
        "10.25",
        "--direction",
        "deposit",
        "-y",
    ]));

    let row = &json[0];
    assert_eq!(row["status"], "confirmed");
    support::signing::assert_signed_by_test_key(row);
    assert_eq!(row["network"], "Testnet");
    assert_eq!(row["reversibility"], "partially_reversible");
    assert_eq!(row["direction"], "deposit");
    assert_eq!(row["amount"], "10.25");
    assert_eq!(row["usd"], 10_250_000);
    assert_eq!(row["token"], "USDC");
    assert_eq!(
        row["subaccount"].as_str().unwrap().to_ascii_lowercase(),
        subaccount_address.to_ascii_lowercase()
    );

    let body = signed_l1_body(&server).await;
    assert_eq!(body["action"]["type"], "subAccountTransfer");
    assert_eq!(
        body["action"]["subAccountUser"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
        subaccount_address.to_ascii_lowercase()
    );
    assert_eq!(body["action"]["isDeposit"], true);
    assert_eq!(body["action"]["usd"], 10_250_000);
    assert!(body["vaultAddress"].is_null());
}

#[tokio::test]
async fn subaccount_spot_transfer_submits_expected_exchange_payload() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let token = "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2";

    let json = support::run_json(env.live_json(&server).args([
        "subaccount",
        "spot-transfer",
        "--subaccount",
        SUBACCOUNT_ADDRESS,
        "--token",
        token,
        "--amount",
        "1.2300",
        "--direction",
        "deposit",
        "-y",
    ]));

    let row = &json[0];
    assert_eq!(row["status"], "confirmed");
    support::signing::assert_signed_by_test_key(row);
    assert_eq!(row["network"], "Testnet");
    assert_eq!(row["reversibility"], "partially_reversible");
    assert_eq!(row["direction"], "deposit");
    assert_eq!(row["amount"], "1.23");
    assert_eq!(row["token"], token);
    assert_eq!(row["subaccount"], SUBACCOUNT_ADDRESS);

    let body = signed_l1_body(&server).await;
    assert_eq!(
        body["action"],
        json!({
            "type": "subAccountSpotTransfer",
            "subAccountUser": SUBACCOUNT_ADDRESS,
            "isDeposit": true,
            "token": token,
            "amount": "1.23",
        })
    );
    assert!(body["vaultAddress"].is_null());
}

#[test]
fn subaccount_transfer_rejects_more_than_six_usdc_decimals() {
    let env = IsolatedHome::new();
    env.command()
        .args([
            "--dry-run",
            "subaccount",
            "transfer",
            "--subaccount",
            SUBACCOUNT_ADDRESS,
            "--amount",
            "0.0000001",
            "--direction",
            "deposit",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("at most 6 decimal places"));
}

#[test]
fn subaccount_transfer_rejects_short_hex_address_with_precise_wording() {
    let env = IsolatedHome::new();
    env.command()
        .args([
            "--dry-run",
            "subaccount",
            "transfer",
            "--subaccount",
            "0x123",
            "--amount",
            "0.000001",
            "--direction",
            "deposit",
        ])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("40-hex-character"))
        .stderr(predicate::str::contains("20-byte"));
}

#[tokio::test]
async fn orders_create_dry_run_on_behalf_includes_vault_context() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let json = support::run_json(env.command_with_server(&server).args([
        "--format",
        "json",
        "--dry-run",
        "orders",
        "create",
        "--on-behalf-of",
        ZERO_ADDRESS,
        "--coin",
        "BTC",
        "--side",
        "buy",
        "--type",
        "limit",
        "--price",
        "50000",
        "--size",
        "0.001",
    ]));

    assert_eq!(json["dry_run"], true);
    assert_eq!(json["command"], "orders create");
    assert_eq!(json["args"]["on_behalf_of"], ZERO_ADDRESS);
    assert!(json["signer"].is_null());
    assert_eq!(json["acting_as"], ZERO_ADDRESS);
    assert_eq!(json["vault_address"], ZERO_ADDRESS);
}

#[tokio::test]
async fn orders_create_on_behalf_sets_vault_address_on_exchange_request() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(wire::order_success_response(12345))
        .await;

    env.live_command(&server)
        .args([
            "orders",
            "create",
            "--on-behalf-of",
            SUBACCOUNT_ADDRESS,
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--type",
            "limit",
            "--price",
            "50000",
            "--size",
            "0.001",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("12345"));

    let body = signed_l1_body(&server).await;
    assert_eq!(body["action"]["type"], "order");
    assert_eq!(body["vaultAddress"], SUBACCOUNT_ADDRESS);
}
