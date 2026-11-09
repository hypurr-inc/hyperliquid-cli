mod contract_support;
mod support;

use support::exchange::ExchangeFixture;

use serde_json::{Value, json};

use contract_support::assert_json_fixture;
use support::{
    API_OVERRIDE_ENV, IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY, expected_address,
};

#[tokio::test]
async fn dry_run_outputs_match_characterization_fixture() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();

    let outputs = json!({
        "orders_create_with_redacted_payload": run_json(
            env.command()
                .env(API_OVERRIDE_ENV, server.uri())
                .args([
                    "--format",
                    "json",
                    "--dry-run",
                    "--payload-json",
                    r#"{"api_key":"secret","nested":{"authorization":"Bearer token"}}"#,
                    "orders",
                    "create",
                    "--coin",
                    "BTC",
                    "--side",
                    "buy",
                    "--price",
                    "50000",
                    "--size",
                    "0.001",
                ]),
        ),
        "orders_batch_create_file": run_json(
            env.command()
                .env(API_OVERRIDE_ENV, server.uri())
                .args([
                    "--format",
                    "json",
                    "--dry-run",
                    "orders",
                    "batch-create",
                    "--orders-file",
                    "tests/fixtures/orders_batch_create.json",
                    "--testnet",
                ]),
        ),
    });

    assert_json_fixture(
        "dry_run_outputs.json",
        &json!({
            "characterization": true,
            "review_required_to_update": true,
            "outputs": outputs,
        }),
    );
}

#[test]
fn subaccount_create_action_plan_preserves_public_dry_run_envelope() {
    let env = IsolatedHome::new();
    let output = run_json(env.command().args([
        "--format",
        "json",
        "--dry-run",
        "subaccount",
        "create",
        "--name",
        "market-maker-1",
    ]));

    assert_eq!(
        output,
        json!({
            "dry_run": true,
            "command": "subaccount create",
            "would_execute": "create_subaccount",
            "args": {
                "name": "market-maker-1",
                "network": "Mainnet",
                "reversibility": "irreversible"
            },
            "signer": null,
            "acting_as": null,
            "vault_address": null
        })
    );
}

#[test]
fn subaccount_transfer_action_plans_preserve_public_dry_run_envelopes() {
    let env = IsolatedHome::new();

    assert_eq!(
        run_json(env.command().args([
            "--format",
            "json",
            "--dry-run",
            "subaccount",
            "transfer",
            "--subaccount",
            "0x0000000000000000000000000000000000000001",
            "--amount",
            "10",
            "--direction",
            "deposit",
        ])),
        json!({
            "dry_run": true,
            "command": "subaccount transfer",
            "would_execute": "subaccount_usdc_transfer",
            "args": {
                "subaccount": "0x0000000000000000000000000000000000000001",
                "amount": "10",
                "direction": "deposit",
                "is_deposit": true,
                "usd": 10_000_000_u64,
                "asset": "USDC",
                "network": "Mainnet",
                "reversibility": "partially_reversible"
            },
            "signer": null,
            "acting_as": null,
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().args([
            "--format",
            "json",
            "--dry-run",
            "subaccount",
            "spot-transfer",
            "--subaccount",
            "0x0000000000000000000000000000000000000001",
            "--token",
            "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2",
            "--amount",
            "1.2300",
            "--direction",
            "withdraw",
        ])),
        json!({
            "dry_run": true,
            "command": "subaccount spot-transfer",
            "would_execute": "subaccount_spot_transfer",
            "args": {
                "subaccount": "0x0000000000000000000000000000000000000001",
                "token": "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2",
                "amount": "1.2300",
                "direction": "withdraw",
                "is_deposit": false,
                "asset": "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2",
                "network": "Mainnet",
                "reversibility": "partially_reversible"
            },
            "signer": null,
            "acting_as": null,
            "vault_address": null
        })
    );
}

#[test]
fn builder_and_referral_action_plans_preserve_public_dry_run_envelopes() {
    let env = IsolatedHome::new();
    let signer = expected_address(VALID_PRIVATE_KEY);

    assert_eq!(
        run_json(env.command().env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY).args([
            "--format",
            "json",
            "--dry-run",
            "builder",
            "approve",
            "--builder",
            "0x00000000000000000000000000000000000000bb",
            "--max-fee-rate",
            "0.001%",
            "--testnet",
        ])),
        json!({
            "dry_run": true,
            "command": "builder approve",
            "would_execute": "approve_builder_fee",
            "args": {
                "network": "Testnet",
                "signer": signer.clone(),
                "query_address": signer.clone(),
                "builder": "0x00000000000000000000000000000000000000bb",
                "max_fee_rate": "0.001%",
                "max_fee_tenths_bps": 1,
                "reversibility": "reversible",
                "action": {
                    "type": "approveBuilderFee",
                    "hyperliquidChain": "Testnet",
                    "signatureChainId": "0x66eee",
                    "builder": "0x00000000000000000000000000000000000000bb",
                    "maxFeeRate": "0.001%"
                }
            },
            "signer": signer.clone(),
            "acting_as": signer.clone(),
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY).args([
            "--format",
            "json",
            "--dry-run",
            "referral",
            "set",
            "TESTNET",
            "--testnet",
        ])),
        json!({
            "dry_run": true,
            "command": "referral set",
            "would_execute": "set_referral_code",
            "args": {
                "network": "Testnet",
                "signer": signer.clone(),
                "query_address": signer.clone(),
                "code": "TESTNET",
                "action": {
                    "type": "setReferrer",
                    "code": "TESTNET"
                },
                "reversibility": "irreversible"
            },
            "signer": signer.clone(),
            "acting_as": signer.clone(),
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY).args([
            "--format",
            "json",
            "--dry-run",
            "referral",
            "register",
            "MYCODE",
            "--testnet",
        ])),
        json!({
            "dry_run": true,
            "command": "referral register",
            "would_execute": "register_referrer_code",
            "args": {
                "network": "Testnet",
                "signer": signer.clone(),
                "query_address": signer.clone(),
                "code": "MYCODE",
                "action": {
                    "type": "registerReferrer",
                    "code": "MYCODE"
                },
                "reversibility": "irreversible"
            },
            "signer": signer.clone(),
            "acting_as": signer,
            "vault_address": null
        })
    );
}

#[test]
fn transfer_action_plans_preserve_public_dry_run_envelopes() {
    let env = IsolatedHome::new();

    assert_eq!(
        run_json(env.command().args([
            "--format",
            "json",
            "--dry-run",
            "transfer",
            "spot-to-perp",
            "--amount",
            "1.23",
        ])),
        json!({
            "dry_run": true,
            "command": "transfer spot-to-perp",
            "would_execute": "transfer_spot_to_perp",
            "args": {"amount": "1.23"},
            "signer": null,
            "acting_as": null,
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().args([
            "--format",
            "json",
            "--dry-run",
            "transfer",
            "perp-to-spot",
            "--amount",
            "1.23",
        ])),
        json!({
            "dry_run": true,
            "command": "transfer perp-to-spot",
            "would_execute": "transfer_perp_to_spot",
            "args": {"amount": "1.23"},
            "signer": null,
            "acting_as": null,
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().args([
            "--format",
            "json",
            "--dry-run",
            "transfer",
            "send",
            "--to",
            "0x0000000000000000000000000000000000000001",
            "--amount",
            "1.23",
        ])),
        json!({
            "dry_run": true,
            "command": "transfer send",
            "would_execute": "send_usdc",
            "args": {
                "to": "0x0000000000000000000000000000000000000001",
                "amount": "1.23"
            },
            "signer": null,
            "acting_as": null,
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().args([
            "--format",
            "json",
            "--dry-run",
            "transfer",
            "withdraw",
            "--to",
            "0x0000000000000000000000000000000000000001",
            "--amount",
            "1.23",
        ])),
        json!({
            "dry_run": true,
            "command": "transfer withdraw",
            "would_execute": "withdraw_usdc",
            "args": {
                "to": "0x0000000000000000000000000000000000000001",
                "amount": "1.23"
            },
            "signer": null,
            "acting_as": null,
            "vault_address": null
        })
    );
}

#[tokio::test]
async fn position_action_previews_bind_validated_intent() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let signer = expected_address(VALID_PRIVATE_KEY);

    assert_eq!(
        run_json(
            env.command()
                .env(API_OVERRIDE_ENV, server.uri())
                .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
                .args([
                    "--format",
                    "json",
                    "--dry-run",
                    "positions",
                    "update-leverage",
                    "--coin",
                    "BTC",
                    "--leverage",
                    "5",
                    "--isolated",
                    "--testnet",
                ])
        ),
        json!({
            "dry_run": true,
            "command": "positions update-leverage",
            "would_execute": "update_position_leverage",
            "args": {
                "coin": "BTC",
                "resolved_coin": "BTC",
                "asset": 0,
                "network": "testnet",
                "leverage": 5,
                "margin_mode": "isolated",
                "is_cross": false,
                "on_behalf_of": null
            },
            "signer": signer.clone(),
            "acting_as": signer.clone(),
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(
            env.command()
                .env(API_OVERRIDE_ENV, server.uri())
                .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
                .args([
                    "--format",
                    "json",
                    "--dry-run",
                    "positions",
                    "update-margin",
                    "--coin",
                    "BTC",
                    "--amount",
                    "1000",
                    "--testnet",
                ])
        ),
        json!({
            "dry_run": true,
            "command": "positions update-margin",
            "would_execute": "update_isolated_margin",
            "args": {
                "coin": "BTC",
                "resolved_coin": "BTC",
                "asset": 0,
                "network": "testnet",
                "amount": "1000",
                "ntli": 1_000_000_000_u64,
                "direction": "add",
                "margin_mode": "isolated",
                "on_behalf_of": null
            },
            "signer": signer,
            "acting_as": signer,
            "vault_address": null
        })
    );
}

#[tokio::test]
async fn staking_vault_and_borrowlend_action_plans_preserve_public_dry_run_envelopes() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let signer = expected_address(VALID_PRIVATE_KEY);
    let validator = "0x0000000000000000000000000000000000000002";
    let vault = "0x0000000000000000000000000000000000000003";

    assert_eq!(
        run_json(env.command().env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY).args([
            "--format",
            "json",
            "--dry-run",
            "staking",
            "delegate",
            "--validator",
            validator,
            "--amount",
            "1.5",
            "--testnet",
        ])),
        json!({
            "dry_run": true,
            "command": "staking delegate",
            "would_execute": "delegate_hype",
            "args": {
                "validator": validator,
                "amount": "1.5",
                "asset": "HYPE",
                "network": "Testnet",
                "reversibility": "partially_reversible"
            },
            "signer": signer.clone(),
            "acting_as": signer.clone(),
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY).args([
            "--format",
            "json",
            "--dry-run",
            "staking",
            "claim-rewards",
            "--testnet",
        ])),
        json!({
            "dry_run": true,
            "command": "staking claim-rewards",
            "would_execute": "claim_staking_rewards",
            "args": {
                "asset": "HYPE",
                "network": "Testnet",
                "reversibility": "irreversible"
            },
            "signer": signer.clone(),
            "acting_as": signer.clone(),
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(env.command().env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY).args([
            "--format",
            "json",
            "--dry-run",
            "vault",
            "deposit",
            "--vault",
            vault,
            "--amount",
            "10",
            "--testnet",
        ])),
        json!({
            "dry_run": true,
            "command": "vault deposit",
            "would_execute": "deposit_usdc_to_vault",
            "args": {
                "action": "deposit",
                "network": "Testnet",
                "vault": vault,
                "amount": "10",
                "asset": "USDC",
                "is_deposit": true,
                "usd": 10_000_000_u64,
                "reversibility": "partially_reversible",
                "verified_shape": {
                    "type": "vaultTransfer",
                    "vaultAddress": vault,
                    "isDeposit": true,
                    "usd": 10_000_000_u64
                }
            },
            "signer": signer.clone(),
            "acting_as": signer.clone(),
            "vault_address": null
        })
    );

    assert_eq!(
        run_json(
            env.command()
                .env(API_OVERRIDE_ENV, server.uri())
                .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
                .args([
                    "--format",
                    "json",
                    "--dry-run",
                    "borrowlend",
                    "withdraw",
                    "USDC",
                    "--max",
                    "--testnet",
                ])
        ),
        json!({
            "dry_run": true,
            "command": "borrowlend withdraw",
            "would_execute": "withdraw_borrowlend",
            "args": {
                "operation": "withdraw",
                "encoded_operation": 1,
                "token": "USDC",
                "token_index": 0,
                "amount": null,
                "max": true,
                "network": "Testnet",
                "wei": 0,
                "reversibility": "partially_reversible",
                "verified_shape": {
                    "transport": "HyperEVM CoreWriter.sendRawAction",
                    "action_id": 15,
                    "encoding_version": 1,
                    "solidity_types": ["uint8", "uint64", "uint64"],
                    "fields": {
                        "encodedOperation": 1,
                        "token": 0,
                        "wei": 0
                    }
                },
                "exchange_action": {
                    "type": "borrowLend",
                    "operation": "withdraw",
                    "token": 0,
                    "amount": null
                },
                "live_submission": "enabled: wallet-signed /exchange borrowLend action shape verified against @nktkas/hyperliquid v0.31.0"
            },
            "signer": signer.clone(),
            "acting_as": signer,
            "vault_address": null
        })
    );
}

/// Every signing command's preview names the signer, the account it acts as and
/// the vault, and a signer that can't be resolved is `null` with a warning,
/// wherever else the command's own `args` mention the signer.
#[test]
fn signed_previews_name_the_acting_account_and_warn_about_a_missing_signer() {
    let env = IsolatedHome::new();
    let signer = expected_address(VALID_PRIVATE_KEY);
    let to = "0x0000000000000000000000000000000000000001";
    let builder = "0x00000000000000000000000000000000000000bb";
    let commands: [&[&str]; 5] = [
        &["transfer", "spot-to-perp", "--amount", "1"],
        &["transfer", "send", "--to", to, "--amount", "1"],
        &["prio", "bid", "--max", "1", "--ip", "1.2.3.4"],
        &["account", "abstraction", "set", "--mode", "disabled"],
        &[
            "builder",
            "approve",
            "--builder",
            builder,
            "--max-fee-rate",
            "0.001%",
        ],
    ];

    for command in commands {
        let run = |key: Option<&str>| {
            let mut cmd = env.command();
            if let Some(key) = key {
                cmd.env(PRIVATE_KEY_ENV, key);
            }
            let output = cmd
                .args(["--format", "json", "--dry-run", "--testnet"])
                .args(command)
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            serde_json::from_slice::<Value>(&output).unwrap()
        };

        let signed = run(Some(VALID_PRIVATE_KEY));
        assert_eq!(signed["signer"], signer, "{command:?}");
        assert_eq!(signed["acting_as"], signer, "{command:?}");
        assert!(signed["vault_address"].is_null(), "{command:?}");
        assert!(signed.get("signer_warning").is_none(), "{command:?}");

        let unsigned = run(None);
        assert!(unsigned["signer"].is_null(), "{command:?}");
        assert!(unsigned["acting_as"].is_null(), "{command:?}");
        assert!(unsigned["vault_address"].is_null(), "{command:?}");
        assert!(
            unsigned["signer_warning"].as_str().is_some(),
            "a null signer carries its warning: {command:?}"
        );
    }
}

/// `--dry-run` previews the command that ran, not the command a shortcut expands to.
#[tokio::test]
async fn shortcut_orders_preview_under_their_own_command_name() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();

    for (command, side) in [("buy", "buy"), ("sell", "sell")] {
        let output = run_json(env.command().env(API_OVERRIDE_ENV, server.uri()).args([
            "--format",
            "json",
            "--dry-run",
            command,
            "--coin",
            "BTC",
            "--price",
            "50000",
            "--size",
            "0.001",
        ]));
        assert_eq!(output["command"], command);
        assert_eq!(output["would_execute"], "submit_order");
        assert_eq!(output["args"]["side"], side);
    }
}

/// Run a dry-run command and return its envelope without the run context.
///
/// Every dry-run envelope names the network, and a signed action whose signer
/// can't be resolved without prompting carries a `signer_warning`. Those two
/// keys are checked here; the contract fixtures pin the rest of the envelope.
fn run_json(command: &mut assert_cmd::Command) -> Value {
    let output = command.assert().success().get_output().stdout.clone();
    let mut value: Value = serde_json::from_slice(&output).unwrap();
    let object = value
        .as_object_mut()
        .expect("dry-run envelope is an object");
    let network = object.remove("network");
    assert!(
        matches!(
            network.as_ref().and_then(Value::as_str),
            Some("mainnet" | "testnet")
        ),
        "dry-run envelope is missing its network: {network:?}"
    );
    object.remove("signer_warning");
    value
}
