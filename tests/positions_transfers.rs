mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::signing::user_signed::{
    SEND_ASSET, SPOT_SEND, USD_CLASS_TRANSFER, USD_SEND, WITHDRAW,
};
use support::signing::{recover_signer, recover_user_signer, test_signer};
use support::{IsolatedHome, TEST_ACCOUNT_PASSPHRASE, VALID_PRIVATE_KEY, expected_address, wire};

const VALID_ADDRESS: &str = "0x0000000000000000000000000000000000000001";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// An account with a winning BTC cross position and a losing ETH isolated one.
fn positions_state(withdrawable: &str) -> Value {
    let mut btc = wire::position_with("BTC", json!({"type": "cross", "value": 5}), "1000", 50);
    btc["position"]["unrealizedPnl"] = json!("100");
    let mut eth = wire::position_with(
        "ETH",
        json!({"type": "isolated", "value": 3, "rawUsd": "1000"}),
        "1000",
        50,
    );
    eth["position"]["unrealizedPnl"] = json!("-200");
    wire::perp_state(withdrawable, vec![btc, eth])
}

/// An exchange where the account holds `withdrawable` on the perp side and
/// `spot_usdc_total` USDC on the spot side, and accepts every action.
async fn exchange(withdrawable: &str, spot_usdc_total: &str) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState"}),
            positions_state(withdrawable),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "spotClearinghouseState"}),
            json!({"balances": [
                {"coin": "USDC", "token": 0, "hold": "0", "total": spot_usdc_total, "entryNtl": "0"}
            ]}),
        )
        .await;
    server
        .mount_info_containing(json!({"type": "userAbstraction"}), json!("disabled"))
        .await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// An exchange that lists the token HYPE (index 150) and accepts every action.
async fn spot_token_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({
                "universe": [],
                "tokens": [
                    {
                        "name": "USDC", "index": 0,
                        "tokenId": "0x00000000000000000000000000000000",
                        "szDecimals": 6, "weiDecimals": 6, "evmContract": null
                    },
                    {
                        "name": "HYPE", "index": 150,
                        "tokenId": "0x11111111111111111111111111111111",
                        "szDecimals": 2, "weiDecimals": 8, "evmContract": null
                    }
                ]
            }),
        )
        .await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// The one `/exchange` request `server` received.
async fn only_exchange_body(server: &ExchangeFixture) -> Value {
    let mut bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    bodies.remove(0)
}

/// The one request, checked to be a user-signed action of `type_string` that
/// the test key signed on the testnet, with the nonce the action carries.
async fn user_signed_action(server: &ExchangeFixture, type_string: &str) -> Value {
    let body = only_exchange_body(server).await;
    assert_eq!(recover_user_signer(&body, type_string), test_signer());
    assert_eq!(body["action"]["hyperliquidChain"], "Testnet");
    assert_eq!(body["action"]["signatureChainId"], "0x66eee");
    assert!(body["vaultAddress"].is_null());
    body
}

#[tokio::test]
async fn positions_list_shows_positions_with_pnl_coloring() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;

    env.live_command(&server)
        .env("FORCE_COLOR", "1")
        .args(["positions", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stdout(predicate::str::contains("Size"))
        .stdout(predicate::str::contains("Entry"))
        .stdout(predicate::str::contains("Mark"))
        .stdout(predicate::str::contains("Unrealized PnL"))
        .stdout(predicate::str::contains("\u{1b}[32m100\u{1b}[0m"))
        .stdout(predicate::str::contains("\u{1b}[31m-200\u{1b}[0m"));
}

#[tokio::test]
async fn positions_list_table_and_json_keep_pnl_uncolored() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;

    env.live_command(&server)
        .env("FORCE_COLOR", "1")
        .args(["--format", "table", "positions", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("100"))
        .stdout(predicate::str::contains("-200"))
        .stdout(predicate::str::contains("\u{1b}[").not());

    env.live_command(&server)
        .env("FORCE_COLOR", "1")
        .args(["--format", "json", "positions", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"unrealized_pnl\""))
        .stdout(predicate::str::contains("\"100\""))
        .stdout(predicate::str::contains("\"-200\""))
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[tokio::test]
async fn transfer_confirmation_prompts_are_yellow_only_in_pretty() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;

    env.live_command(&server)
        .env("FORCE_COLOR", "1")
        .write_stdin("n\n")
        .args(["transfer", "spot-to-perp", "--amount", "100", "--testnet"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "\u{1b}[33mTransfer 100 USDC from spot to perp?\u{1b}[0m [y/N]: ",
        ));

    env.live_json(&server)
        .env("FORCE_COLOR", "1")
        .write_stdin("n\n")
        .args(["transfer", "spot-to-perp", "--amount", "100"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "without confirmation in non-interactive mode",
        ))
        .stdout(predicate::str::contains("\u{1b}[").not());
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn positions_update_leverage_validates_and_outputs_confirmation() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;

    env.live_command(&server)
        .args([
            "positions",
            "update-leverage",
            "--coin",
            "BTC",
            "--leverage",
            "5",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stdout(predicate::str::contains("5"))
        .stdout(predicate::str::contains("updated"));

    let body = only_exchange_body(&server).await;
    assert_eq!(
        body["action"],
        json!({"type": "updateLeverage", "asset": 0, "isCross": true, "leverage": 5})
    );
    assert_eq!(recover_signer(&body, Chain::Testnet), test_signer());
}

#[test]
fn positions_update_leverage_rejects_zero() {
    let env = IsolatedHome::new();
    env.command()
        .args([
            "positions",
            "update-leverage",
            "--coin",
            "BTC",
            "--leverage",
            "0",
            "--testnet",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid value"));
}

#[tokio::test]
async fn positions_update_margin_outputs_confirmation() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;

    env.live_command(&server)
        .args([
            "positions",
            "update-margin",
            "--coin",
            "ETH",
            "--amount",
            "1000",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ETH"))
        .stdout(predicate::str::contains("1000"))
        .stdout(predicate::str::contains("updated"));

    let body = only_exchange_body(&server).await;
    assert_eq!(
        body["action"],
        json!({"type": "updateIsolatedMargin", "asset": 1, "isBuy": true, "ntli": 1_000_000_000_u64})
    );
    assert_eq!(recover_signer(&body, Chain::Testnet), test_signer());
}

#[tokio::test]
async fn positions_update_leverage_reports_an_unknown_hip3_market_as_not_found() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;

    env.live_json(&server)
        .args([
            "positions",
            "update-leverage",
            "--coin",
            "xyz:NOPE",
            "--leverage",
            "3",
        ])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("xyz:NOPE"));
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn transfer_spot_to_perp_prompts_and_confirms() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;

    env.live_command(&server)
        .write_stdin("y\n")
        .args(["transfer", "spot-to-perp", "--amount", "100", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("spot-to-perp"))
        .stdout(predicate::str::contains("100"))
        .stderr(predicate::str::contains(
            "Transfer 100 USDC from spot to perp?",
        ));

    let body = user_signed_action(&server, USD_CLASS_TRANSFER).await;
    assert_eq!(
        body["action"],
        json!({
            "type": "usdClassTransfer",
            "signatureChainId": "0x66eee",
            "hyperliquidChain": "Testnet",
            "amount": "100",
            "toPerp": true,
            "nonce": body["nonce"],
        })
    );
}

#[tokio::test]
async fn transfer_perp_to_spot_prompts_and_confirms() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;

    env.live_command(&server)
        .write_stdin("y\n")
        .args(["transfer", "perp-to-spot", "--amount", "100", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("perp-to-spot"))
        .stderr(predicate::str::contains(
            "Transfer 100 USDC from perp to spot?",
        ));

    let body = user_signed_action(&server, USD_CLASS_TRANSFER).await;
    assert_eq!(
        body["action"],
        json!({
            "type": "usdClassTransfer",
            "signatureChainId": "0x66eee",
            "hyperliquidChain": "Testnet",
            "amount": "100",
            "toPerp": false,
            "nonce": body["nonce"],
        })
    );
}

#[tokio::test]
async fn transfer_send_prompts_and_confirms() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;
    let signer = expected_address(VALID_PRIVATE_KEY);

    env.live_command(&server)
        .write_stdin("yes\n")
        .args([
            "transfer",
            "send",
            "--to",
            VALID_ADDRESS,
            "--amount",
            "100",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("send"))
        .stdout(predicate::str::contains(VALID_ADDRESS))
        .stderr(predicate::str::contains("Network: Testnet"))
        .stderr(predicate::str::contains(format!("Signer: {signer}")))
        .stderr(predicate::str::contains(format!(
            "Acting context: {signer}"
        )))
        .stderr(predicate::str::contains(format!(
            "Recipient: {VALID_ADDRESS}"
        )))
        .stderr(predicate::str::contains(
            "Destination class: hyperliquid_user_address",
        ))
        .stderr(predicate::str::contains("Asset: USDC"))
        .stderr(predicate::str::contains("Amount: 100"))
        .stderr(predicate::str::contains(
            "Fee/cap: not_estimated_exchange_default",
        ))
        .stderr(predicate::str::contains("Reversibility: irreversible"))
        .stderr(predicate::str::contains(format!(
            "Send 100 USDC to {VALID_ADDRESS}?"
        )));

    let body = user_signed_action(&server, USD_SEND).await;
    assert_eq!(
        body["action"],
        json!({
            "type": "usdSend",
            "signatureChainId": "0x66eee",
            "hyperliquidChain": "Testnet",
            "destination": VALID_ADDRESS,
            "amount": "100",
            "time": body["nonce"],
        })
    );
}

#[tokio::test]
async fn transfer_spot_send_dry_run_includes_token_and_destination() {
    let env = IsolatedHome::new();
    let server = spot_token_exchange().await;

    let json = support::run_json(env.command_with_server(&server).args([
        "--format",
        "json",
        "--dry-run",
        "transfer",
        "spot-send",
        "--to",
        VALID_ADDRESS,
        "--token",
        "HYPE",
        "--amount",
        "1.23",
        "--testnet",
    ]));

    assert_eq!(json["command"], "transfer spot-send");
    assert_eq!(json["would_execute"], "spot_send");
    assert_eq!(json["args"]["to"], VALID_ADDRESS);
    assert_eq!(json["args"]["token"], "HYPE");
    assert_eq!(json["args"]["token_index"], 150);
    assert_eq!(json["args"]["amount"], "1.23");
}

#[tokio::test]
async fn transfer_send_asset_dry_run_normalizes_asset_targets() {
    let env = IsolatedHome::new();
    let server = spot_token_exchange().await;

    let json = support::run_json(env.command_with_server(&server).args([
        "--format",
        "json",
        "--dry-run",
        "transfer",
        "send-asset",
        "--to",
        VALID_ADDRESS,
        "--source",
        "spot",
        "--dest",
        "dex:test",
        "--token",
        "USDC",
        "--amount",
        "4.5",
        "--testnet",
    ]));

    assert_eq!(json["command"], "transfer send-asset");
    assert_eq!(json["would_execute"], "send_asset");
    assert_eq!(json["args"]["source"], "spot");
    assert_eq!(json["args"]["source_wire"], "spot");
    assert_eq!(json["args"]["dest"], "dex:test");
    assert_eq!(json["args"]["dest_wire"], "test");
    assert_eq!(json["args"]["token"], "USDC");
    assert_eq!(json["args"]["amount"], "4.5");
}

#[test]
fn transfer_send_asset_invalid_target_exits_before_auth() {
    let env = IsolatedHome::new();

    env.command()
        .args([
            "transfer",
            "send-asset",
            "--to",
            VALID_ADDRESS,
            "--source",
            "alias:foo",
            "--dest",
            "spot",
            "--token",
            "USDC",
            "--amount",
            "1",
            "--testnet",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "--source must be perp, spot, or dex:<NAME>",
        ))
        .stderr(predicate::str::contains("Authentication required").not());
}

#[tokio::test]
async fn transfer_spot_send_submits_expected_exchange_payload() {
    let env = IsolatedHome::new();
    let server = spot_token_exchange().await;
    let signer = expected_address(VALID_PRIVATE_KEY);

    env.live_command(&server)
        .write_stdin("yes\n")
        .args([
            "transfer",
            "spot-send",
            "--to",
            VALID_ADDRESS,
            "--token",
            "HYPE",
            "--amount",
            "1.23",
            "--testnet",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("Network: Testnet"))
        .stderr(predicate::str::contains(format!("Signer: {signer}")))
        .stderr(predicate::str::contains(format!(
            "Acting context: {signer}"
        )))
        .stderr(predicate::str::contains(format!(
            "Recipient: {VALID_ADDRESS}"
        )))
        .stderr(predicate::str::contains(
            "Destination class: hyperliquid_spot_user_address",
        ))
        .stderr(predicate::str::contains("Asset: HYPE"))
        .stderr(predicate::str::contains("Amount: 1.23"))
        .stderr(predicate::str::contains(
            "Fee/cap: not_estimated_exchange_default",
        ))
        .stderr(predicate::str::contains("Reversibility: irreversible"))
        .stderr(predicate::str::contains(format!(
            "Send 1.23 HYPE spot token to {VALID_ADDRESS}?"
        )));

    let body = user_signed_action(&server, SPOT_SEND).await;
    assert_eq!(
        body["action"],
        json!({
            "type": "spotSend",
            "signatureChainId": "0x66eee",
            "hyperliquidChain": "Testnet",
            "destination": VALID_ADDRESS,
            "token": "HYPE:0x11111111111111111111111111111111",
            "amount": "1.23",
            "time": body["nonce"],
        })
    );
}

#[tokio::test]
async fn transfer_send_asset_submits_expected_exchange_payload() {
    let env = IsolatedHome::new();
    let server = spot_token_exchange().await;
    let signer = expected_address(VALID_PRIVATE_KEY);

    env.live_command(&server)
        .write_stdin("yes\n")
        .args([
            "transfer",
            "send-asset",
            "--to",
            VALID_ADDRESS,
            "--source",
            "spot",
            "--dest",
            "dex:test",
            "--token",
            "USDC",
            "--amount",
            "4.5",
            "--testnet",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("Network: Testnet"))
        .stderr(predicate::str::contains(format!("Signer: {signer}")))
        .stderr(predicate::str::contains(format!(
            "Acting context: {signer}"
        )))
        .stderr(predicate::str::contains(format!(
            "Recipient: {VALID_ADDRESS}"
        )))
        .stderr(predicate::str::contains(
            "Destination class: hyperliquid_asset_context",
        ))
        .stderr(predicate::str::contains("Asset: USDC spot->dex:test"))
        .stderr(predicate::str::contains("Amount: 4.5"))
        .stderr(predicate::str::contains(
            "Fee/cap: not_estimated_exchange_default",
        ))
        .stderr(predicate::str::contains("Reversibility: irreversible"))
        .stderr(predicate::str::contains(format!(
            "Send 4.5 USDC from spot to dex:test for {VALID_ADDRESS}?"
        )));

    let body = user_signed_action(&server, SEND_ASSET).await;
    assert_eq!(
        body["action"],
        json!({
            "type": "sendAsset",
            "signatureChainId": "0x66eee",
            "hyperliquidChain": "Testnet",
            "destination": VALID_ADDRESS,
            "sourceDex": "spot",
            "destinationDex": "test",
            "token": "USDC:0x00000000000000000000000000000000",
            "amount": "4.5",
            "fromSubAccount": "",
            "nonce": body["nonce"],
        })
    );
}

#[tokio::test]
async fn transfer_withdraw_prompts_and_confirms() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "1000").await;
    let signer = expected_address(VALID_PRIVATE_KEY);

    env.live_command(&server)
        .write_stdin("y\n")
        .args([
            "transfer",
            "withdraw",
            "--to",
            VALID_ADDRESS,
            "--amount",
            "100",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("withdraw"))
        .stdout(predicate::str::contains(VALID_ADDRESS))
        .stderr(predicate::str::contains("Network: Testnet"))
        .stderr(predicate::str::contains(format!("Signer: {signer}")))
        .stderr(predicate::str::contains(format!(
            "Acting context: {signer}"
        )))
        .stderr(predicate::str::contains(format!(
            "Recipient: {VALID_ADDRESS}"
        )))
        .stderr(predicate::str::contains(
            "Destination class: arbitrum_address",
        ))
        .stderr(predicate::str::contains("Asset: USDC"))
        .stderr(predicate::str::contains("Amount: 100"))
        .stderr(predicate::str::contains(
            "Fee/cap: not_estimated_bridge_or_exchange_fee",
        ))
        .stderr(predicate::str::contains("Reversibility: irreversible"))
        .stderr(predicate::str::contains(format!(
            "Withdraw 100 USDC to Arbitrum address {VALID_ADDRESS}?"
        )));

    let body = user_signed_action(&server, WITHDRAW).await;
    assert_eq!(
        body["action"],
        json!({
            "type": "withdraw3",
            "signatureChainId": "0x66eee",
            "hyperliquidChain": "Testnet",
            "destination": VALID_ADDRESS,
            "amount": "100",
            "time": body["nonce"],
        })
    );
}

#[tokio::test]
async fn transfer_insufficient_balance_exits_13_with_context() {
    let env = IsolatedHome::new();
    let server = exchange("1000", "5").await;

    env.live_command(&server)
        .args(["transfer", "spot-to-perp", "--amount", "100", "--testnet"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("insufficient balance"))
        .stderr(predicate::str::contains("available spot USDC"));
    assert!(server.exchange_actions().await.is_empty());
}

#[test]
fn transfer_invalid_address_exits_2() {
    let env = IsolatedHome::new();

    env.command()
        .args([
            "transfer",
            "send",
            "--to",
            "INVALID",
            "--amount",
            "100",
            "--testnet",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "address must be a 0x-prefixed 40-byte hex string",
        ));
}

#[test]
fn transfer_send_zero_address_exits_2_before_prompt_or_auth() {
    let env = IsolatedHome::new();

    env.command()
        .args([
            "transfer",
            "send",
            "--to",
            ZERO_ADDRESS,
            "--amount",
            "100",
            "--testnet",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "address must not be the zero address",
        ))
        .stderr(predicate::str::contains("Send 100 USDC").not())
        .stderr(predicate::str::contains("Authentication required").not());
}

#[test]
fn transfer_send_recipient_does_not_resolve_stored_account_alias() {
    let env = IsolatedHome::new();
    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args([
            "account",
            "add",
            VALID_PRIVATE_KEY,
            "--alias",
            "recipient-alias",
        ])
        .assert()
        .success();

    let output = env
        .account_command(TEST_ACCOUNT_PASSPHRASE)
        .args([
            "--format",
            "json",
            "--dry-run",
            "transfer",
            "send",
            "--to",
            "recipient-alias",
            "--amount",
            "1",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    assert!(
        support::stdout_json(&output)["error"]
            .as_str()
            .unwrap()
            .contains("0x-prefixed")
    );
}

#[test]
fn transfer_withdraw_zero_address_exits_2_before_prompt_or_auth() {
    let env = IsolatedHome::new();

    env.command()
        .args([
            "transfer",
            "withdraw",
            "--to",
            ZERO_ADDRESS,
            "--amount",
            "100",
            "--testnet",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "address must not be the zero address",
        ))
        .stderr(predicate::str::contains("Withdraw 100 USDC").not())
        .stderr(predicate::str::contains("Authentication required").not());
}
