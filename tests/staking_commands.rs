mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::signing::user_signed::{C_DEPOSIT, C_WITHDRAW, LINK_STAKING_USER, TOKEN_DELEGATE};
use support::signing::{
    assert_signed_by_test_key, recover_raw_l1_signer, recover_user_signer, test_signer,
};
use support::{IsolatedHome, run_json};

const VALID_ADDRESS: &str = "0x0000000000000000000000000000000000000001";
const VALIDATOR_ADDRESS: &str = "0x0000000000000000000000000000000000000002";

#[derive(Serialize)]
struct ClaimRewards {
    r#type: &'static str,
}

async fn staking_public_server() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "delegatorSummary"}),
            json!({
                "delegated": "123.45",
                "undelegated": "10",
                "totalPendingWithdrawal": "5.5",
                "nPendingWithdrawals": 2
            }),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "delegations"}),
            json!([{
                "validator": VALIDATOR_ADDRESS,
                "amount": "100.0",
                "lockedUntilTimestamp": 1700000000000_u64
            }]),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "delegatorRewards"}),
            json!([{
                "time": 1700000000000_u64,
                "source": "staking",
                "totalAmount": "1.25"
            }]),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "validatorSummaries"}),
            json!([{
                "validator": VALIDATOR_ADDRESS,
                "signer": "0x0000000000000000000000000000000000000003",
                "name": "Validator One",
                "description": "test validator",
                "nRecentBlocks": 1,
                "stake": 123456789_u64,
                "isJailed": false,
                "unjailableAfter": null,
                "isActive": true,
                "commission": "0.04",
                "stats": [
                    ["day", {"uptimeFraction": "1.0", "predictedApr": "0.025", "nSamples": 1440}]
                ]
            }]),
        )
        .await;
    server
}

async fn staking_action_server() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// The request envelope every signed action travels in, with no vault and no
/// expiry. Returns the nonce.
fn assert_signed_exchange_envelope(body: &Value) -> u64 {
    let mut keys: Vec<_> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "action",
            "expiresAfter",
            "nonce",
            "signature",
            "vaultAddress"
        ]
    );
    assert!(body["vaultAddress"].is_null());
    assert!(body["expiresAfter"].is_null());
    let nonce = body["nonce"].as_u64().expect("nonce is a u64");
    assert!(nonce > 0);
    nonce
}

#[tokio::test]
async fn staking_summary_json_includes_balances_delegations_and_rewards() {
    let env = IsolatedHome::new();
    let server = staking_public_server().await;

    let json = run_json(env.command_with_server(&server).args([
        "--format",
        "json",
        "staking",
        "summary",
        VALID_ADDRESS,
    ]));

    assert_eq!(json["address"], VALID_ADDRESS);
    assert_eq!(json["delegated"], "123.45");
    assert_eq!(json["undelegated"], "10");
    assert_eq!(json["total_pending_withdrawal"], "5.5");
    assert_eq!(json["pending_rewards"], "1.25");
    assert_eq!(json["delegations"][0]["validator"], VALIDATOR_ADDRESS);
}

#[tokio::test]
async fn staking_validators_lists_validator_names_and_addresses() {
    let env = IsolatedHome::new();
    let server = staking_public_server().await;

    env.command_with_server(&server)
        .args(["staking", "validators"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Validator One"))
        .stdout(predicate::str::contains(VALIDATOR_ADDRESS))
        .stderr(predicate::str::contains("Completed in").not());
}

#[tokio::test]
async fn staking_rewards_json_shows_reward_amounts() {
    let env = IsolatedHome::new();
    let server = staking_public_server().await;

    let json = run_json(env.command_with_server(&server).args([
        "--format",
        "json",
        "staking",
        "rewards",
        VALID_ADDRESS,
    ]));

    assert_eq!(json[0]["source"], "staking");
    assert_eq!(json[0]["total_amount"], "1.25");
}

#[tokio::test]
async fn staking_actions_report_action_and_reversibility_in_json_output() {
    let cases: [(&[&str], &str, &str); 5] = [
        (
            &[
                "delegate",
                "--validator",
                VALIDATOR_ADDRESS,
                "--amount",
                "1.5",
            ],
            "delegate",
            "partially_reversible",
        ),
        (
            &[
                "undelegate",
                "--validator",
                VALIDATOR_ADDRESS,
                "--amount",
                "4",
            ],
            "undelegate",
            "partially_reversible",
        ),
        (
            &["deposit", "--amount", "3"],
            "deposit",
            "partially_reversible",
        ),
        (
            &["withdraw", "--amount", "2"],
            "withdraw",
            "partially_reversible",
        ),
        (&["claim-rewards"], "claim-rewards", "irreversible"),
    ];
    let server = staking_action_server().await;

    for (args, action, reversibility) in cases {
        let env = IsolatedHome::new();
        let json = run_json(env.live_json(&server).arg("staking").args(args));

        assert_eq!(json["action"], action);
        assert_eq!(json["status"], "submitted", "{action}");
        assert_eq!(json["reversibility"], reversibility, "{action}");
        assert_eq!(json["network"], "Testnet", "{action}");
    }
}

#[tokio::test]
async fn staking_withdraw_notes_seven_day_queue() {
    let env = IsolatedHome::new();
    let server = staking_action_server().await;

    let json = run_json(
        env.live_json(&server)
            .args(["staking", "withdraw", "--amount", "2"]),
    );

    assert_eq!(json["action"], "withdraw");
    assert_eq!(json["status"], "submitted");
    assert_eq!(json["network"], "Testnet");
    assert_eq!(json["amount"], "2");
    assert_eq!(json["asset"], "HYPE");
    assert_eq!(json["reversibility"], "partially_reversible");
    assert!(
        json["note"]
            .as_str()
            .unwrap()
            .contains("7-day withdrawal queue")
    );
    assert_signed_by_test_key(&json);

    let body = &server.exchange_bodies().await[0];
    assert_eq!(body["action"]["type"], "cWithdraw");
    assert_eq!(body["action"]["wei"], 200000000_u64);
}

#[tokio::test]
async fn staking_action_payloads_and_signatures_match_hyperliquid_shapes() {
    let env = IsolatedHome::new();
    let server = staking_action_server().await;

    for args in [
        &[
            "delegate",
            "--validator",
            VALIDATOR_ADDRESS,
            "--amount",
            "1.5",
        ][..],
        &[
            "undelegate",
            "--validator",
            VALIDATOR_ADDRESS,
            "--amount",
            "4",
        ],
        &["deposit", "--amount", "3"],
        &["withdraw", "--amount", "2"],
        &["claim-rewards"],
    ] {
        env.live_json(&server)
            .arg("staking")
            .args(args)
            .assert()
            .success();
    }

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 5);
    let nonces: Vec<u64> = bodies.iter().map(assert_signed_exchange_envelope).collect();

    let user_signed = |type_string, action: Value| (type_string, action);
    let expected = [
        user_signed(
            TOKEN_DELEGATE,
            json!({
                "type": "tokenDelegate",
                "signatureChainId": "0x66eee",
                "hyperliquidChain": "Testnet",
                "validator": VALIDATOR_ADDRESS,
                "wei": 150000000_u64,
                "isUndelegate": false,
                "nonce": nonces[0],
            }),
        ),
        user_signed(
            TOKEN_DELEGATE,
            json!({
                "type": "tokenDelegate",
                "signatureChainId": "0x66eee",
                "hyperliquidChain": "Testnet",
                "validator": VALIDATOR_ADDRESS,
                "wei": 400000000_u64,
                "isUndelegate": true,
                "nonce": nonces[1],
            }),
        ),
        user_signed(
            C_DEPOSIT,
            json!({
                "type": "cDeposit",
                "signatureChainId": "0x66eee",
                "hyperliquidChain": "Testnet",
                "wei": 300000000_u64,
                "nonce": nonces[2],
            }),
        ),
        user_signed(
            C_WITHDRAW,
            json!({
                "type": "cWithdraw",
                "signatureChainId": "0x66eee",
                "hyperliquidChain": "Testnet",
                "wei": 200000000_u64,
                "nonce": nonces[3],
            }),
        ),
    ];
    for (body, (type_string, action)) in bodies.iter().zip(expected) {
        assert_eq!(body["action"], action);
        assert_eq!(recover_user_signer(body, type_string), test_signer());
    }

    let claim = &bodies[4];
    assert_eq!(claim["action"], json!({"type": "claimRewards"}));
    assert_eq!(
        recover_raw_l1_signer(
            claim,
            &ClaimRewards {
                r#type: "claimRewards"
            },
            Chain::Testnet
        ),
        test_signer()
    );
}

#[test]
fn staking_rejects_invalid_validators_and_amounts_before_auth() {
    let zero = "0x0000000000000000000000000000000000000000";
    let malformed = "0x02";
    let cases: [(&str, &str, i32, &str); 4] = [
        (
            zero,
            "1",
            2,
            "validator address must not be the zero address",
        ),
        (malformed, "1", 13, "Invalid address"),
        (
            VALIDATOR_ADDRESS,
            "0",
            2,
            "amount must be greater than zero",
        ),
        (VALIDATOR_ADDRESS, "0.000000001", 2, "amount"),
    ];

    for (validator, amount, code, message) in cases {
        let env = IsolatedHome::new();
        env.command()
            .args([
                "staking",
                "delegate",
                "--validator",
                validator,
                "--amount",
                amount,
                "--testnet",
            ])
            .assert()
            .code(code)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(message));
    }
}

#[tokio::test]
async fn staking_link_dry_run_previews_verified_shape_and_warning() {
    let env = IsolatedHome::new();
    let server = staking_action_server().await;
    let preview = |phase: &str| {
        run_json(env.live_json(&server).args([
            "--dry-run",
            "staking",
            "link",
            phase,
            "--user",
            VALID_ADDRESS,
        ]))
    };

    let json = preview("initiate");
    assert_eq!(json["command"], "staking link initiate");
    assert_eq!(json["would_execute"], "link_staking_user");
    assert_eq!(json["args"]["phase"], "initiate");
    assert_eq!(json["args"]["user"], VALID_ADDRESS);
    assert_eq!(json["args"]["is_finalize"], false);
    assert_eq!(json["args"]["network"], "Testnet");
    assert_eq!(json["args"]["reversibility"], "irreversible");
    assert_eq!(json["args"]["action"]["type"], "linkStakingUser");
    assert_eq!(json["args"]["action"]["signatureChainId"], "0x66eee");
    assert_eq!(
        json["args"]["verified_shape"]["eip712_fields"],
        json!(["hyperliquidChain", "user", "isFinalize", "nonce"])
    );
    assert!(
        json["args"]["warning"]
            .as_str()
            .unwrap()
            .contains("permanent")
    );
    assert_signed_by_test_key(&json);
    assert!(json["vault_address"].is_null());

    let finalize = preview("finalize");
    assert_eq!(finalize["command"], "staking link finalize");
    assert_eq!(finalize["args"]["phase"], "finalize");
    assert_eq!(finalize["args"]["action"]["isFinalize"], true);
    assert!(server.exchange_bodies().await.is_empty());
}

#[tokio::test]
async fn staking_link_initiate_and_finalize_submit_verified_actions() {
    let env = IsolatedHome::new();
    let server = staking_action_server().await;

    let initiate_json = run_json(env.live_json(&server).args([
        "staking",
        "link",
        "initiate",
        "--user",
        VALID_ADDRESS,
        "--yes",
    ]));
    assert_eq!(initiate_json["status"], "submitted");
    assert_eq!(initiate_json["action"], "staking-link");
    assert_eq!(initiate_json["phase"], "initiate");
    assert_eq!(initiate_json["user"], VALID_ADDRESS);
    assert_eq!(initiate_json["is_finalize"], false);
    assert_eq!(initiate_json["network"], "Testnet");
    assert_eq!(initiate_json["reversibility"], "irreversible");
    assert!(
        initiate_json["warning"]
            .as_str()
            .unwrap()
            .contains("permanent")
    );
    assert_signed_by_test_key(&initiate_json);

    env.live_command(&server)
        .args([
            "staking",
            "link",
            "finalize",
            "--user",
            VALID_ADDRESS,
            "--yes",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("finalize"))
        .stdout(predicate::str::contains("staking-link"));

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 2);
    for (body, is_finalize) in bodies.iter().zip([false, true]) {
        let nonce = assert_signed_exchange_envelope(body);
        assert_eq!(
            body["action"],
            json!({
                "type": "linkStakingUser",
                "signatureChainId": "0x66eee",
                "hyperliquidChain": "Testnet",
                "user": VALID_ADDRESS,
                "isFinalize": is_finalize,
                "nonce": nonce,
            })
        );
        assert_eq!(recover_user_signer(body, LINK_STAKING_USER), test_signer());
    }
}

#[tokio::test]
async fn staking_link_requires_confirmation_without_yes() {
    let env = IsolatedHome::new();
    let server = staking_action_server().await;

    env.live_command(&server)
        .args([
            "staking",
            "link",
            "initiate",
            "--user",
            VALID_ADDRESS,
            "--testnet",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Can't continue without --yes"));

    assert!(server.exchange_bodies().await.is_empty());
}

#[test]
fn staking_link_validation_fails_before_auth() {
    let env = IsolatedHome::new();
    env.command()
        .args([
            "staking",
            "link",
            "finalize",
            "--user",
            "0x0000000000000000000000000000000000000000",
            "--yes",
            "--testnet",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "staking-link user address must not be the zero address",
        ));
}

#[test]
fn staking_actions_without_wallet_exit_10() {
    for args in [
        vec![
            "staking",
            "delegate",
            "--validator",
            VALIDATOR_ADDRESS,
            "--amount",
            "1",
            "--testnet",
        ],
        vec![
            "staking",
            "undelegate",
            "--validator",
            VALIDATOR_ADDRESS,
            "--amount",
            "1",
            "--testnet",
        ],
        vec!["staking", "deposit", "--amount", "1", "--testnet"],
        vec!["staking", "withdraw", "--amount", "1", "--testnet"],
        vec!["staking", "claim-rewards", "--testnet"],
        vec![
            "staking",
            "link",
            "initiate",
            "--user",
            VALID_ADDRESS,
            "--yes",
            "--testnet",
        ],
        vec![
            "staking",
            "link",
            "finalize",
            "--user",
            VALID_ADDRESS,
            "--yes",
            "--testnet",
        ],
    ] {
        let env = IsolatedHome::new();
        env.command()
            .args(args)
            .assert()
            .code(10)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Authentication required"));
    }
}
