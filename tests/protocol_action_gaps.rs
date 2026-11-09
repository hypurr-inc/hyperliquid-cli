//! Exchange actions the CLI gained from the protocol backlog: class transfers
//! for a subaccount, `topUpIsolatedOnlyMargin`, manual `borrow`/`repay`,
//! `evmUserModify`, the priority-auction slots and the staking claim wording.

mod support;

use hypersdk::hypercore::Chain;
use hypersdk::hypercore::types::{Action, Signature};
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout};

const SUBACCOUNT: &str = "0x00000000000000000000000000000000000000cc";
const VAULT: &str = "0x0000000000000000000000000000000000000001";

fn ok() -> Value {
    json!({"status": "ok", "response": {"type": "default"}})
}

fn run(env: &IsolatedHome, server: &ExchangeFixture, args: &[&str]) -> assert_cmd::Command {
    let mut command = env.live_command(server);
    command.args(["--format", "json", "--testnet"]).args(args);
    command
}

/// The address that signed `body`, recovered from its signature over the
/// hypersdk `Action` it carries.
fn signer_of(body: &Value) -> String {
    let action: Action = serde_json::from_value(body["action"].clone()).unwrap();
    let signature: Signature = serde_json::from_value(body["signature"].clone()).unwrap();
    let vault = body["vaultAddress"]
        .as_str()
        .map(|vault| vault.parse().unwrap());
    action
        .recover(
            &signature,
            body["nonce"].as_u64().unwrap(),
            vault,
            None,
            Chain::Testnet,
        )
        .unwrap()
        .to_string()
        .to_lowercase()
}

fn test_signer() -> String {
    expected_address(VALID_PRIVATE_KEY).to_lowercase()
}

// ---- transfer spot-to-perp / perp-to-spot --subaccount ----

fn perp_state(withdrawable: &str) -> Value {
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
        "withdrawable": withdrawable,
        "assetPositions": [],
        "time": 1_700_000_000_000_u64
    })
}

/// A subaccount holding `spot` USDC in spot and `perp` withdrawable in perp,
/// in a standard account. Only reads about the subaccount are answered: a
/// preflight that asks about anyone else fails the command.
async fn subaccount_server(spot: &str, perp: &str) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "spotClearinghouseState", "user": SUBACCOUNT}),
            json!({"balances": [
                {"coin": "USDC", "token": 0, "hold": "0", "total": spot, "entryNtl": "0"}
            ]}),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "userAbstraction", "user": SUBACCOUNT}),
            json!("default"),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState", "user": SUBACCOUNT}),
            perp_state(perp),
        )
        .await;
    server.mount_exchange(ok()).await;
    server
}

#[tokio::test]
async fn class_transfers_for_a_subaccount_sign_the_amount_with_its_suffix() {
    for (direction, to_perp) in [("spot-to-perp", true), ("perp-to-spot", false)] {
        let env = IsolatedHome::new();
        let server = subaccount_server("10", "10").await;

        run(
            &env,
            &server,
            &[
                "transfer",
                direction,
                "--amount",
                "5",
                "--subaccount",
                SUBACCOUNT,
                "--yes",
            ],
        )
        .assert()
        .success();

        let bodies = server.exchange_bodies().await;
        assert_eq!(bodies.len(), 1, "{direction}");
        let nonce = bodies[0]["nonce"].as_u64().unwrap();
        assert_eq!(
            bodies[0]["action"],
            json!({
                "type": "usdClassTransfer",
                "hyperliquidChain": "Testnet",
                "signatureChainId": "0x66eee",
                "amount": format!("5 subaccount:{SUBACCOUNT}"),
                "toPerp": to_perp,
                "nonce": nonce,
            }),
            "{direction}"
        );
        // The master signs the amount with its suffix: the signature only
        // verifies against that document.
        assert_eq!(signer_of(&bodies[0]), test_signer(), "{direction}");
    }
}

#[tokio::test]
async fn a_subaccount_transfer_is_checked_against_the_subaccounts_balance() {
    let env = IsolatedHome::new();
    let server = subaccount_server("1", "1").await;

    for direction in ["spot-to-perp", "perp-to-spot"] {
        run(
            &env,
            &server,
            &[
                "transfer",
                direction,
                "--amount",
                "5",
                "--subaccount",
                SUBACCOUNT,
                "--yes",
            ],
        )
        .assert()
        .code(13)
        .stdout(predicate::str::contains("insufficient balance"));
    }
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn a_class_transfer_without_a_subaccount_keeps_the_plain_amount() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let signer = test_signer();
    server
        .mount_info_containing(
            json!({"type": "spotClearinghouseState", "user": signer}),
            json!({"balances": [
                {"coin": "USDC", "token": 0, "hold": "0", "total": "10", "entryNtl": "0"}
            ]}),
        )
        .await;
    server.mount_exchange(ok()).await;

    run(
        &env,
        &server,
        &["transfer", "spot-to-perp", "--amount", "5", "--yes"],
    )
    .assert()
    .success();

    assert_eq!(server.exchange_actions().await[0]["amount"], "5");
}

#[tokio::test]
async fn subaccount_class_transfer_dry_run_shows_the_wire_amount() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let assert = run(
        &env,
        &server,
        &[
            "--dry-run",
            "transfer",
            "spot-to-perp",
            "--amount",
            "5.50",
            "--subaccount",
            SUBACCOUNT,
        ],
    )
    .assert()
    .success();
    let preview = json_stdout(&assert);
    assert_eq!(preview["args"]["subaccount"], SUBACCOUNT);
    assert_eq!(
        preview["args"]["wire_amount"],
        format!("5.5 subaccount:{SUBACCOUNT}")
    );

    let assert = run(
        &env,
        &server,
        &["--dry-run", "transfer", "spot-to-perp", "--amount", "5"],
    )
    .assert()
    .success();
    assert_eq!(json_stdout(&assert)["args"], json!({"amount": "5"}));
    assert!(server.exchange_actions().await.is_empty());
}

// ---- positions top-up-margin ----

fn perp(name: &str, margin_mode: Value) -> Value {
    json!({
        "name": name,
        "szDecimals": 2,
        "maxLeverage": 10,
        "onlyIsolated": !margin_mode.is_null(),
        "marginMode": margin_mode,
        "growthMode": "disabled"
    })
}

fn position(coin: &str, leverage_type: &str) -> Value {
    json!({
        "type": "oneWay",
        "position": {
            "coin": coin,
            "szi": "1",
            "leverage": {"type": leverage_type, "value": 4},
            "entryPx": "100",
            "positionValue": "100",
            "unrealizedPnl": "0",
            "returnOnEquity": "0",
            "liquidationPx": null,
            "marginUsed": "25",
            "maxLeverage": 10,
            "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
        }
    })
}

/// BTC and ETH trade either way, PUMP is `strictIsolated`; `user`'s account
/// holds `positions`.
async fn margin_server(user: &str, positions: Vec<Value>) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "meta"}),
            support::fixture_perp_meta(vec![
                perp("BTC", Value::Null),
                perp("ETH", Value::Null),
                perp("PUMP", json!("strictIsolated")),
            ]),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState", "user": user}),
            json!({
                "marginSummary": {
                    "accountValue": "1000", "totalNtlPos": "100",
                    "totalRawUsd": "1000", "totalMarginUsed": "25"
                },
                "crossMarginSummary": {
                    "accountValue": "1000", "totalNtlPos": "100",
                    "totalRawUsd": "1000", "totalMarginUsed": "25"
                },
                "crossMaintenanceMarginUsed": "10",
                "withdrawable": "900",
                "assetPositions": positions,
                "time": 1_700_000_000_000_u64
            }),
        )
        .await;
    server.mount_exchange(ok()).await;
    server
}

#[tokio::test]
async fn top_up_margin_sends_the_target_leverage_as_a_float_string() {
    let env = IsolatedHome::new();
    let server = margin_server(&test_signer(), vec![position("BTC", "isolated")]).await;

    run(
        &env,
        &server,
        &[
            "positions",
            "top-up-margin",
            "--coin",
            "BTC",
            "--leverage",
            "12.50",
        ],
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("\"target_leverage\":\"12.5\""));

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0]["action"],
        json!({"type": "topUpIsolatedOnlyMargin", "asset": 0, "leverage": "12.5"})
    );
    assert!(bodies[0]["vaultAddress"].is_null());
    assert_eq!(signer_of(&bodies[0]), test_signer());
}

#[tokio::test]
async fn top_up_margin_for_a_vault_checks_and_signs_for_the_vault() {
    let env = IsolatedHome::new();
    let server = margin_server(VAULT, vec![position("ETH", "isolated")]).await;

    run(
        &env,
        &server,
        &[
            "positions",
            "top-up-margin",
            "--coin",
            "ETH",
            "--leverage",
            "5",
            "--on-behalf-of",
            VAULT,
        ],
    )
    .assert()
    .success();

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies[0]["vaultAddress"], VAULT);
    assert_eq!(
        bodies[0]["action"],
        json!({"type": "topUpIsolatedOnlyMargin", "asset": 1, "leverage": "5"})
    );
    // The vault is part of what the signature covers.
    assert_eq!(signer_of(&bodies[0]), test_signer());
}

#[tokio::test]
async fn top_up_margin_applies_to_a_strict_isolated_market_too() {
    let env = IsolatedHome::new();
    let server = margin_server(&test_signer(), vec![position("PUMP", "isolated")]).await;

    run(
        &env,
        &server,
        &[
            "positions",
            "top-up-margin",
            "--coin",
            "PUMP",
            "--leverage",
            "3",
        ],
    )
    .assert()
    .success();

    assert_eq!(server.exchange_actions().await[0]["asset"], 2);
}

#[tokio::test]
async fn top_up_margin_needs_an_open_isolated_position() {
    let env = IsolatedHome::new();

    let flat = margin_server(&test_signer(), vec![]).await;
    run(
        &env,
        &flat,
        &[
            "positions",
            "top-up-margin",
            "--coin",
            "BTC",
            "--leverage",
            "5",
        ],
    )
    .assert()
    .code(13)
    .stdout(predicate::str::contains("has no open BTC position"));
    assert!(flat.exchange_actions().await.is_empty());

    let cross = margin_server(&test_signer(), vec![position("BTC", "cross")]).await;
    run(
        &env,
        &cross,
        &[
            "positions",
            "top-up-margin",
            "--coin",
            "BTC",
            "--leverage",
            "5",
        ],
    )
    .assert()
    .code(13)
    .stdout(predicate::str::contains("uses cross margin"));
    assert!(cross.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn top_up_margin_refuses_a_leverage_that_is_not_positive() {
    let env = IsolatedHome::new();
    let server = margin_server(&test_signer(), vec![position("BTC", "isolated")]).await;

    for leverage in ["0", "-3"] {
        run(
            &env,
            &server,
            &[
                "positions",
                "top-up-margin",
                "--coin",
                "BTC",
                "--leverage",
                leverage,
            ],
        )
        .assert()
        .failure();
    }
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn top_up_margin_dry_run_previews_without_signing_or_reading_the_position() {
    let env = IsolatedHome::new();
    let server = margin_server(&test_signer(), vec![]).await;

    let assert = run(
        &env,
        &server,
        &[
            "--dry-run",
            "positions",
            "top-up-margin",
            "--coin",
            "BTC",
            "--leverage",
            "7",
            "--on-behalf-of",
            VAULT,
        ],
    )
    .assert()
    .success();
    let preview = json_stdout(&assert);

    assert_eq!(preview["would_execute"], "top_up_isolated_margin");
    assert_eq!(preview["args"]["asset"], 0);
    assert_eq!(preview["args"]["target_leverage"], "7");
    assert_eq!(preview["vault_address"], VAULT);
    assert!(server.exchange_actions().await.is_empty());
}

// ---- borrowlend repay / borrow ----

async fn borrowlend_server(raw_abstraction: &str) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(json!({"type": "userAbstraction"}), json!(raw_abstraction))
        .await;
    server.mount_exchange(ok()).await;
    server
}

#[tokio::test]
async fn repay_and_borrow_submit_their_operations() {
    let env = IsolatedHome::new();
    let server = borrowlend_server("unifiedAccount").await;

    run(
        &env,
        &server,
        &["borrowlend", "repay", "USDC", "--amount", "5"],
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("\"operation\":\"repay\""));
    run(&env, &server, &["borrowlend", "repay", "USDC", "--max"])
        .assert()
        .success();
    run(
        &env,
        &server,
        &["borrowlend", "borrow", "USDC", "--amount", "7.5"],
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("\"operation\":\"borrow\""));

    let actions = server.exchange_actions().await;
    assert_eq!(
        actions,
        vec![
            json!({"type": "borrowLend", "operation": "repay", "token": 0, "amount": "5"}),
            json!({"type": "borrowLend", "operation": "repay", "token": 0, "amount": null}),
            json!({"type": "borrowLend", "operation": "borrow", "token": 0, "amount": "7.5"}),
        ]
    );
}

#[tokio::test]
async fn borrow_has_no_max_and_is_refused_before_any_request() {
    let env = IsolatedHome::new();
    let server = borrowlend_server("unifiedAccount").await;

    run(&env, &server, &["borrowlend", "borrow", "USDC", "--max"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "borrow/lend borrow requires --amount; --max is only supported for withdraw and repay",
        ));
    // The wording of the existing refusal follows the new operation.
    run(&env, &server, &["borrowlend", "supply", "USDC", "--max"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "--max is only supported for withdraw and repay",
        ));
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn manual_borrow_is_refused_under_portfolio_margin_live_and_dry_run() {
    let env = IsolatedHome::new();
    let server = borrowlend_server("portfolioMargin").await;

    for dry_run in [true, false] {
        let mut args = vec!["borrowlend", "borrow", "USDC", "--amount", "5"];
        if dry_run {
            args.insert(0, "--dry-run");
        }
        run(&env, &server, &args)
            .assert()
            .code(13)
            .stdout(predicate::str::contains("portfolio-margin account"))
            .stdout(predicate::str::contains(
                "the manual borrow action is disabled",
            ));
    }
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn repay_is_not_gated_on_the_account_mode() {
    let env = IsolatedHome::new();
    let server = borrowlend_server("portfolioMargin").await;

    run(
        &env,
        &server,
        &["borrowlend", "repay", "USDC", "--amount", "1"],
    )
    .assert()
    .success();

    assert_eq!(server.exchange_actions().await.len(), 1);
    assert!(server.info_requests("userAbstraction").await.is_empty());
}

#[tokio::test]
async fn borrow_dry_run_does_not_invent_a_core_writer_encoding() {
    let env = IsolatedHome::new();
    let server = borrowlend_server("default").await;

    for operation in ["borrow", "repay"] {
        let assert = run(
            &env,
            &server,
            &[
                "--dry-run",
                "borrowlend",
                operation,
                "USDC",
                "--amount",
                "5",
            ],
        )
        .assert()
        .success();
        let preview = json_stdout(&assert);
        let args = &preview["args"];
        assert_eq!(args["operation"], operation);
        assert!(args.get("encoded_operation").is_none(), "{operation}");
        assert!(args.get("verified_shape").is_none(), "{operation}");
        assert_eq!(args["exchange_action"]["operation"], operation);
    }

    let assert = run(
        &env,
        &server,
        &["--dry-run", "borrowlend", "supply", "USDC", "--amount", "5"],
    )
    .assert()
    .success();
    let args = &json_stdout(&assert)["args"];
    assert_eq!(args["encoded_operation"], 0);
    assert_eq!(args["verified_shape"]["fields"]["encodedOperation"], 0);
}

// ---- account evm-big-blocks ----

#[tokio::test]
async fn evm_big_blocks_sends_the_flag_in_both_directions() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server.mount_exchange(ok()).await;

    run(&env, &server, &["account", "evm-big-blocks", "--enable"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"using_big_blocks\":true"));
    run(&env, &server, &["account", "evm-big-blocks", "--disable"])
        .assert()
        .success();

    let bodies = server.exchange_bodies().await;
    assert_eq!(
        bodies[0]["action"],
        json!({"type": "evmUserModify", "usingBigBlocks": true})
    );
    assert_eq!(
        bodies[1]["action"],
        json!({"type": "evmUserModify", "usingBigBlocks": false})
    );
    assert!(bodies[0]["vaultAddress"].is_null());
    assert_eq!(signer_of(&bodies[0]), test_signer());
}

#[tokio::test]
async fn evm_big_blocks_needs_exactly_one_direction() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    for args in [
        &["account", "evm-big-blocks"][..],
        &["account", "evm-big-blocks", "--enable", "--disable"],
    ] {
        run(&env, &server, args).assert().code(2);
    }
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn evm_big_blocks_dry_run_previews_the_action() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let assert = run(
        &env,
        &server,
        &["--dry-run", "account", "evm-big-blocks", "--disable"],
    )
    .assert()
    .success();
    let preview = json_stdout(&assert);

    assert_eq!(preview["would_execute"], "set_evm_big_blocks");
    assert_eq!(
        preview["args"]["action"],
        json!({"type": "evmUserModify", "usingBigBlocks": false})
    );
    assert!(server.exchange_actions().await.is_empty());
}

// ---- prio bid --slot ----

#[tokio::test]
async fn prio_bid_slots_are_zero_to_two() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    for dry_run in [true, false] {
        let mut args = vec![
            "prio",
            "bid",
            "--max",
            "1",
            "--ip",
            "203.0.113.10",
            "--slot",
            "3",
        ];
        if dry_run {
            args.insert(0, "--dry-run");
        }
        run(&env, &server, &args)
            .assert()
            .code(2)
            .stdout(predicate::str::contains("3 is not in 0..=2"));
    }

    let assert = run(
        &env,
        &server,
        &[
            "--dry-run",
            "prio",
            "bid",
            "--max",
            "1",
            "--ip",
            "203.0.113.10",
            "--slot",
            "2",
        ],
    )
    .assert()
    .success();
    assert_eq!(json_stdout(&assert)["args"]["slot"], 2);
    assert!(server.exchange_actions().await.is_empty());

    IsolatedHome::new()
        .command()
        .args(["prio", "bid", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Slot index (0-2)"));
}

// ---- staking claim-rewards ----

#[test]
fn claim_rewards_says_what_it_claims() {
    let env = IsolatedHome::new();

    let help = env
        .command()
        .args(["staking", "claim-rewards", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(help).unwrap();
    let schema = env
        .command()
        .args(["--format", "json", "schema", "staking", "claim-rewards"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let schema = String::from_utf8(schema).unwrap();

    for text in [&help, &schema] {
        assert!(text.contains("referral and builder rewards"), "{text}");
        assert!(!text.contains("Claim staking rewards"), "{text}");
    }
    assert!(schema.contains("redelegated"), "{schema}");
}
