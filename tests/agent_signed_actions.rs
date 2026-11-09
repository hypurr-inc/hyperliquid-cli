//! What a stored API wallet may sign. The exchange takes user-signed actions
//! (`usdSend`, `withdraw3`, `tokenDelegate`, ...) from the account that signs
//! them, so a wallet that only holds an agent key is refused up front. Three
//! actions have an agent-signed form, an L1 action the master's agent may send:
//! `agentSendAsset`, `cDeposit2` and `agentSetAbstraction`.

mod support;

use alloy::primitives::Address;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::signing::agent_signing_hash;
use hypersdk::hypercore::types::Signature;
use predicates::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::signing::{raw_l1_hash, recover_signer, test_signer};
use support::{API_OVERRIDE_ENV, IsolatedHome, VALID_PRIVATE_KEY, expected_address};

const MASTER: &str = "0x00000000000000000000000000000000000000aa";
const OTHER: &str = "0x00000000000000000000000000000000000000bb";
const SUBACCOUNT: &str = "0x00000000000000000000000000000000000000cc";

/// A home whose default signer is an API wallet of [`MASTER`].
fn home_with_agent() -> IsolatedHome {
    IsolatedHome::new().with_stored_account("bot", VALID_PRIVATE_KEY, Some(MASTER))
}

async fn server() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// A command signed by the stored API wallet.
fn as_agent(env: &IsolatedHome, server: &ExchangeFixture) -> assert_cmd::Command {
    let mut command = env.account_command_without_passphrase();
    command
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["--format", "json", "--testnet"]);
    command
}

fn lower(address: &str) -> String {
    address.to_lowercase()
}

#[tokio::test]
async fn user_signed_actions_refuse_a_stored_api_wallet_before_any_request() {
    let env = home_with_agent();
    let server = server().await;

    // (arguments, the exchange action the refusal names, takes --yes)
    let commands: [(&[&str], &str, bool); 11] = [
        (
            &["transfer", "spot-to-perp", "--amount", "1"],
            "usdClassTransfer",
            true,
        ),
        (
            &["transfer", "perp-to-spot", "--amount", "1"],
            "usdClassTransfer",
            true,
        ),
        (
            &["transfer", "send", "--to", OTHER, "--amount", "1"],
            "usdSend",
            true,
        ),
        (
            &[
                "transfer",
                "spot-send",
                "--to",
                OTHER,
                "--token",
                "USDC",
                "--amount",
                "1",
            ],
            "spotSend",
            true,
        ),
        (
            &["transfer", "withdraw", "--to", OTHER, "--amount", "1"],
            "withdraw3",
            true,
        ),
        (
            &["staking", "delegate", "--validator", OTHER, "--amount", "1"],
            "tokenDelegate",
            false,
        ),
        (
            &[
                "staking",
                "undelegate",
                "--validator",
                OTHER,
                "--amount",
                "1",
            ],
            "tokenDelegate",
            false,
        ),
        (
            &["staking", "withdraw", "--amount", "1"],
            "cWithdraw",
            false,
        ),
        (
            &["staking", "link", "initiate", "--user", OTHER],
            "linkStakingUser",
            true,
        ),
        (
            &["staking", "link", "finalize", "--user", OTHER],
            "linkStakingUser",
            true,
        ),
        (
            &[
                "builder",
                "approve",
                "--builder",
                OTHER,
                "--max-fee-rate",
                "0.001%",
            ],
            "approveBuilderFee",
            true,
        ),
    ];

    for (command, action, takes_yes) in commands {
        for dry_run in [true, false] {
            let mut args = Vec::from(command);
            if dry_run {
                args.insert(0, "--dry-run");
            } else if takes_yes {
                args.push("--yes");
            }
            as_agent(&env, &server)
                .args(&args)
                .assert()
                .code(13)
                .stdout(predicate::str::contains(format!(
                    "{action} must be signed by the master account"
                )));
        }
    }
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn an_api_wallet_signs_agent_send_asset_to_the_account_it_signs_for() {
    let env = home_with_agent();
    let server = server().await;

    as_agent(&env, &server)
        .args([
            "transfer",
            "send-asset",
            "--to",
            MASTER,
            "--source",
            "spot",
            "--dest",
            "perp",
            "--token",
            "USDC",
            "--amount",
            "1.5",
            "--yes",
        ])
        .assert()
        .success();

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1);
    let nonce = bodies[0]["nonce"].as_u64().unwrap();
    let action = &bodies[0]["action"];
    assert_eq!(
        action["destination"].as_str().map(lower),
        Some(lower(MASTER))
    );
    let mut without_destination = action.clone();
    without_destination
        .as_object_mut()
        .unwrap()
        .remove("destination");
    assert_eq!(
        without_destination,
        json!({
            "type": "agentSendAsset",
            "sourceDex": "spot",
            "destinationDex": "",
            "token": "USDC:0x00000000000000000000000000000000",
            "amount": "1.5",
            "fromSubAccount": "",
            "nonce": nonce,
        })
    );
    // An L1 action signed by the API wallet, not the master's typed data.
    assert_eq!(recover_signer(&bodies[0], Chain::Testnet), test_signer());
}

#[tokio::test]
async fn an_api_wallet_may_send_asset_out_of_a_subaccount_to_that_subaccount() {
    let env = home_with_agent();
    let server = server().await;

    as_agent(&env, &server)
        .args([
            "transfer",
            "send-asset",
            "--to",
            SUBACCOUNT,
            "--from-subaccount",
            SUBACCOUNT,
            "--source",
            "perp",
            "--dest",
            "spot",
            "--token",
            "USDC",
            "--amount",
            "2",
            "--yes",
        ])
        .assert()
        .success();

    let action = &server.exchange_actions().await[0];
    assert_eq!(action["type"], "agentSendAsset");
    assert_eq!(action["fromSubAccount"], SUBACCOUNT);
}

#[tokio::test]
async fn an_api_wallet_cannot_send_asset_to_another_address_live_or_dry_run() {
    let env = home_with_agent();
    let server = server().await;

    for dry_run in [true, false] {
        let mut command = as_agent(&env, &server);
        if dry_run {
            command.arg("--dry-run");
        }
        command
            .args([
                "transfer",
                "send-asset",
                "--to",
                OTHER,
                "--source",
                "spot",
                "--dest",
                "perp",
                "--token",
                "USDC",
                "--amount",
                "1",
                "--yes",
            ])
            .assert()
            .code(13)
            .stdout(predicate::str::contains(
                "an API wallet can only send an asset to the account it leaves",
            ));
    }
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn send_asset_dry_run_names_the_agent_action_only_for_an_api_wallet() {
    let env = home_with_agent();
    let server = server().await;
    let args = [
        "--dry-run",
        "transfer",
        "send-asset",
        "--to",
        MASTER,
        "--source",
        "spot",
        "--dest",
        "perp",
        "--token",
        "USDC",
        "--amount",
        "1",
    ];

    let output = as_agent(&env, &server).args(args).output().unwrap();
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(preview["args"]["action_type"], "agentSendAsset");

    let output = env.live_json(&server).args(args).output().unwrap();
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(preview["args"].get("action_type").is_none(), "{preview}");
}

#[tokio::test]
async fn a_master_key_still_signs_send_asset_as_user_signed_typed_data() {
    let env = IsolatedHome::new();
    let server = server().await;

    env.live_json(&server)
        .args([
            "transfer",
            "send-asset",
            "--to",
            OTHER,
            "--source",
            "spot",
            "--dest",
            "perp",
            "--token",
            "USDC",
            "--amount",
            "1",
            "--yes",
        ])
        .assert()
        .success();

    let action = &server.exchange_actions().await[0];
    assert_eq!(action["type"], "sendAsset");
    assert_eq!(action["hyperliquidChain"], "Testnet");
}

#[derive(Serialize)]
struct CDeposit2 {
    r#type: &'static str,
    wei: u64,
}

/// The signer of an L1 action hypersdk does not model, from its msgpack hash.
fn raw_l1_signer(action: &impl Serialize, nonce: u64, signature: &Value) -> Address {
    let prehash = agent_signing_hash(Chain::Testnet, raw_l1_hash(action, nonce));
    let signature: Signature = serde_json::from_value(signature.clone()).unwrap();
    alloy::signers::Signature::new(signature.r, signature.s, signature.v == 28)
        .recover_address_from_prehash(&prehash)
        .unwrap()
}

#[tokio::test]
async fn an_api_wallet_deposits_to_staking_with_the_agent_action() {
    let env = home_with_agent();
    let server = server().await;

    as_agent(&env, &server)
        .args(["staking", "deposit", "--amount", "1.5"])
        .assert()
        .success();

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0]["action"],
        json!({"type": "cDeposit2", "wei": 150_000_000_u64})
    );
    let nonce = bodies[0]["nonce"].as_u64().unwrap();
    let recovered = raw_l1_signer(
        &CDeposit2 {
            r#type: "cDeposit2",
            wei: 150_000_000,
        },
        nonce,
        &bodies[0]["signature"],
    );
    assert_eq!(recovered.to_string().to_lowercase(), test_signer());
}

#[tokio::test]
async fn a_master_key_deposits_to_staking_with_the_user_signed_action() {
    let env = IsolatedHome::new();
    let server = server().await;

    env.live_json(&server)
        .args(["staking", "deposit", "--amount", "1.5"])
        .assert()
        .success();

    let action = &server.exchange_actions().await[0];
    assert_eq!(action["type"], "cDeposit");
    assert_eq!(action["wei"], 150_000_000_u64);
    assert_eq!(action["hyperliquidChain"], "Testnet");
}

#[tokio::test]
async fn staking_deposit_dry_run_names_the_agent_action_only_for_an_api_wallet() {
    let env = home_with_agent();
    let server = server().await;

    let output = as_agent(&env, &server)
        .args(["--dry-run", "staking", "deposit", "--amount", "1"])
        .output()
        .unwrap();
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(preview["args"]["action_type"], "cDeposit2");
}

#[tokio::test]
async fn an_api_wallet_sets_its_masters_abstraction_with_the_agent_action() {
    for (mode, code) in [
        ("disabled", "i"),
        ("unified-account", "u"),
        ("portfolio-margin", "p"),
    ] {
        let env = home_with_agent();
        let server = server().await;

        as_agent(&env, &server)
            .args(["account", "abstraction", "set", "--mode", mode, "--yes"])
            .assert()
            .success();

        let bodies = server.exchange_bodies().await;
        assert_eq!(
            bodies[0]["action"],
            json!({"type": "agentSetAbstraction", "abstraction": code}),
            "{mode}"
        );
        assert_eq!(recover_signer(&bodies[0], Chain::Testnet), test_signer());
    }
}

#[tokio::test]
async fn an_api_wallet_may_name_its_master_but_no_other_account() {
    let env = home_with_agent();
    let server = server().await;

    as_agent(&env, &server)
        .args([
            "account",
            "abstraction",
            "set",
            "--mode",
            "unified-account",
            "--user",
            MASTER,
            "--yes",
        ])
        .assert()
        .success();
    assert_eq!(server.exchange_actions().await.len(), 1);

    for dry_run in [true, false] {
        let mut command = as_agent(&env, &server);
        if dry_run {
            command.arg("--dry-run");
        }
        command
            .args([
                "account",
                "abstraction",
                "set",
                "--mode",
                "unified-account",
                "--user",
                SUBACCOUNT,
                "--yes",
            ])
            .assert()
            .code(13)
            .stdout(predicate::str::contains(
                "an API wallet can only set the abstraction of its own master account",
            ));
    }
    assert_eq!(
        server.exchange_actions().await.len(),
        1,
        "nothing more sent"
    );
}

#[tokio::test]
async fn abstraction_dry_run_previews_the_agent_action_for_an_api_wallet() {
    let env = home_with_agent();
    let server = server().await;

    let output = as_agent(&env, &server)
        .args([
            "--dry-run",
            "account",
            "abstraction",
            "set",
            "--mode",
            "portfolio-margin",
        ])
        .output()
        .unwrap();
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(preview["args"]["agent_signed"], true);
    assert_eq!(
        preview["args"]["user"].as_str().map(lower),
        Some(lower(MASTER))
    );
    assert_eq!(
        preview["args"]["action"],
        json!({"type": "agentSetAbstraction", "abstraction": "p"})
    );
}

#[tokio::test]
async fn a_master_sets_the_abstraction_of_its_own_subaccount() {
    let env = IsolatedHome::new();
    let server = server().await;
    let signer = expected_address(VALID_PRIVATE_KEY);
    server
        .mount_info_containing(
            json!({"type": "userRole", "user": SUBACCOUNT}),
            json!({"role": "subAccount", "data": {"master": signer}}),
        )
        .await;

    env.live_json(&server)
        .args([
            "account",
            "abstraction",
            "set",
            "--mode",
            "unified-account",
            "--user",
            SUBACCOUNT,
            "--yes",
        ])
        .assert()
        .success();

    let action = &server.exchange_actions().await[0];
    assert_eq!(action["type"], "userSetAbstraction");
    assert_eq!(action["abstraction"], "unifiedAccount");
    assert_eq!(action["user"].as_str().map(lower), Some(lower(SUBACCOUNT)));
}

#[tokio::test]
async fn a_master_cannot_set_the_abstraction_of_an_account_it_does_not_own() {
    let env = IsolatedHome::new();
    let server = server().await;
    let signer = expected_address(VALID_PRIVATE_KEY);
    server
        .mount_info_containing(
            json!({"type": "userRole", "user": SUBACCOUNT}),
            json!({"role": "subAccount", "data": {"master": OTHER}}),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "userRole", "user": OTHER}),
            json!({"role": "user"}),
        )
        .await;

    for user in [SUBACCOUNT, OTHER] {
        env.live_json(&server)
            .args([
                "account",
                "abstraction",
                "set",
                "--mode",
                "disabled",
                "--user",
                user,
                "--yes",
            ])
            .assert()
            .code(13)
            .stdout(predicate::str::contains(format!(
                "is not a subaccount of the signer {signer}"
            )));
    }
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn a_master_without_user_sets_its_own_abstraction_without_a_role_lookup() {
    let env = IsolatedHome::new();
    let server = server().await;

    env.live_json(&server)
        .args([
            "account",
            "abstraction",
            "set",
            "--mode",
            "disabled",
            "--yes",
        ])
        .assert()
        .success();

    let action = &server.exchange_actions().await[0];
    assert_eq!(action["type"], "userSetAbstraction");
    assert_eq!(action["user"].as_str().map(lower), Some(test_signer()));
    assert!(server.info_requests("userRole").await.is_empty());
}
