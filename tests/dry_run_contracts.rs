//! The `--dry-run` envelope of the commands that are not orders, pinned whole:
//! what an agent reads to decide whether to run the command for real.

mod contract_support;
mod support;

use serde_json::{Value, json};

use contract_support::assert_json_fixture;
use support::exchange::ExchangeFixture;
use support::{
    API_OVERRIDE_ENV, IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY, expected_address,
    run_json,
};

/// The preview of `args`, signed by the test key when `signer` is set, against
/// `server` when the command resolves a market.
///
/// Every envelope names the network, and a signed action whose signer can't be
/// resolved carries a `signer_warning`; both are checked or dropped here, so a
/// test pins the rest of the envelope.
fn dry_run(server: Option<&ExchangeFixture>, signer: bool, args: &[&str]) -> Value {
    let home = IsolatedHome::new();
    let mut command = home.json_command();
    if let Some(server) = server {
        command.env(API_OVERRIDE_ENV, server.uri());
    }
    if signer {
        command.env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY);
    }
    let mut value = run_json(command.arg("--dry-run").args(args));
    let envelope = value
        .as_object_mut()
        .expect("a dry-run envelope is an object");
    let network = envelope.remove("network");
    assert!(
        matches!(
            network.as_ref().and_then(Value::as_str),
            Some("mainnet" | "testnet")
        ),
        "dry-run envelope is missing its network: {network:?}"
    );
    envelope.remove("signer_warning");
    value
}

fn unsigned(args: &[&str]) -> Value {
    dry_run(None, false, args)
}

fn signed(args: &[&str]) -> Value {
    dry_run(None, true, args)
}

/// The envelope of a preview that names the signer: the signer is also who the
/// command acts as, with no vault.
fn signed_envelope(command: &str, would_execute: &str, args: Value) -> Value {
    let signer = expected_address(VALID_PRIVATE_KEY);
    json!({
        "dry_run": true,
        "command": command,
        "would_execute": would_execute,
        "args": args,
        "signer": signer,
        "acting_as": signer,
        "vault_address": null
    })
}

/// The envelope of a preview run without a signer.
fn unsigned_envelope(command: &str, would_execute: &str, args: Value) -> Value {
    json!({
        "dry_run": true,
        "command": command,
        "would_execute": would_execute,
        "args": args,
        "signer": null,
        "acting_as": null,
        "vault_address": null
    })
}

#[tokio::test]
async fn orders_dry_runs_match_the_snapshot() {
    let server = ExchangeFixture::start().await;

    let outputs = json!({
        "orders_create_with_redacted_payload": dry_run(Some(&server), false, &[
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
        "orders_batch_create_file": dry_run(Some(&server), false, &[
            "orders",
            "batch-create",
            "--orders-file",
            "tests/fixtures/orders_batch_create.json",
            "--testnet",
        ]),
    });

    assert_json_fixture("dry_run_outputs.json", &outputs);
}

#[test]
fn subaccount_previews_are_unsigned_and_name_the_irreversibility() {
    let sub = "0x0000000000000000000000000000000000000001";

    assert_eq!(
        unsigned(&["subaccount", "create", "--name", "market-maker-1"]),
        unsigned_envelope(
            "subaccount create",
            "create_subaccount",
            json!({
                "name": "market-maker-1",
                "network": "Mainnet",
                "reversibility": "irreversible"
            })
        )
    );

    assert_eq!(
        unsigned(&[
            "subaccount",
            "transfer",
            "--subaccount",
            sub,
            "--amount",
            "10",
            "--direction",
            "deposit",
        ]),
        unsigned_envelope(
            "subaccount transfer",
            "subaccount_usdc_transfer",
            json!({
                "subaccount": sub,
                "amount": "10",
                "direction": "deposit",
                "is_deposit": true,
                "usd": 10_000_000_u64,
                "asset": "USDC",
                "network": "Mainnet",
                "reversibility": "partially_reversible"
            })
        )
    );

    assert_eq!(
        unsigned(&[
            "subaccount",
            "spot-transfer",
            "--subaccount",
            sub,
            "--token",
            "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2",
            "--amount",
            "1.2300",
            "--direction",
            "withdraw",
        ]),
        unsigned_envelope(
            "subaccount spot-transfer",
            "subaccount_spot_transfer",
            json!({
                "subaccount": sub,
                "token": "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2",
                "amount": "1.2300",
                "direction": "withdraw",
                "is_deposit": false,
                "asset": "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2",
                "network": "Mainnet",
                "reversibility": "partially_reversible"
            })
        )
    );
}

#[test]
fn builder_and_referral_previews_carry_the_signed_action() {
    let signer = expected_address(VALID_PRIVATE_KEY);
    let builder = "0x00000000000000000000000000000000000000bb";

    assert_eq!(
        signed(&[
            "builder",
            "approve",
            "--builder",
            builder,
            "--max-fee-rate",
            "0.001%",
            "--testnet",
        ]),
        signed_envelope(
            "builder approve",
            "approve_builder_fee",
            json!({
                "network": "Testnet",
                "signer": signer,
                "query_address": signer,
                "builder": builder,
                "max_fee_rate": "0.001%",
                "max_fee_tenths_bps": 1,
                "reversibility": "reversible",
                "action": {
                    "type": "approveBuilderFee",
                    "hyperliquidChain": "Testnet",
                    "signatureChainId": "0x66eee",
                    "builder": builder,
                    "maxFeeRate": "0.001%"
                }
            })
        )
    );

    assert_eq!(
        signed(&["referral", "set", "TESTNET", "--testnet"]),
        signed_envelope(
            "referral set",
            "set_referral_code",
            json!({
                "network": "Testnet",
                "signer": signer,
                "query_address": signer,
                "code": "TESTNET",
                "action": {"type": "setReferrer", "code": "TESTNET"},
                "reversibility": "irreversible"
            })
        )
    );

    assert_eq!(
        signed(&["referral", "register", "MYCODE", "--testnet"]),
        signed_envelope(
            "referral register",
            "register_referrer_code",
            json!({
                "network": "Testnet",
                "signer": signer,
                "query_address": signer,
                "code": "MYCODE",
                "action": {"type": "registerReferrer", "code": "MYCODE"},
                "reversibility": "irreversible"
            })
        )
    );
}

#[test]
fn transfer_previews_are_unsigned_without_a_key() {
    let to = "0x0000000000000000000000000000000000000001";

    assert_eq!(
        unsigned(&["transfer", "spot-to-perp", "--amount", "1.23"]),
        unsigned_envelope(
            "transfer spot-to-perp",
            "transfer_spot_to_perp",
            json!({"amount": "1.23"})
        )
    );
    assert_eq!(
        unsigned(&["transfer", "perp-to-spot", "--amount", "1.23"]),
        unsigned_envelope(
            "transfer perp-to-spot",
            "transfer_perp_to_spot",
            json!({"amount": "1.23"})
        )
    );
    assert_eq!(
        unsigned(&["transfer", "send", "--to", to, "--amount", "1.23"]),
        unsigned_envelope(
            "transfer send",
            "send_usdc",
            json!({"to": to, "amount": "1.23"})
        )
    );
    assert_eq!(
        unsigned(&["transfer", "withdraw", "--to", to, "--amount", "1.23"]),
        unsigned_envelope(
            "transfer withdraw",
            "withdraw_usdc",
            json!({"to": to, "amount": "1.23"})
        )
    );
}

#[tokio::test]
async fn position_previews_bind_the_validated_intent() {
    let server = ExchangeFixture::start().await;
    let preview = |args: &[&str]| dry_run(Some(&server), true, args);

    assert_eq!(
        preview(&[
            "positions",
            "update-leverage",
            "--coin",
            "BTC",
            "--leverage",
            "5",
            "--isolated",
            "--testnet",
        ]),
        signed_envelope(
            "positions update-leverage",
            "update_position_leverage",
            json!({
                "coin": "BTC",
                "resolved_coin": "BTC",
                "asset": 0,
                "network": "testnet",
                "leverage": 5,
                "margin_mode": "isolated",
                "is_cross": false,
                "on_behalf_of": null
            })
        )
    );

    assert_eq!(
        preview(&[
            "positions",
            "update-margin",
            "--coin",
            "BTC",
            "--amount",
            "1000",
            "--testnet",
        ]),
        signed_envelope(
            "positions update-margin",
            "update_isolated_margin",
            json!({
                "coin": "BTC",
                "resolved_coin": "BTC",
                "asset": 0,
                "network": "testnet",
                "amount": "1000",
                "ntli": 1_000_000_000_u64,
                "direction": "add",
                "margin_mode": "isolated",
                "on_behalf_of": null
            })
        )
    );
}

#[tokio::test]
async fn staking_vault_and_borrowlend_previews_carry_the_verified_shape() {
    let server = ExchangeFixture::start().await;
    let validator = "0x0000000000000000000000000000000000000002";
    let vault = "0x0000000000000000000000000000000000000003";

    assert_eq!(
        signed(&[
            "staking",
            "delegate",
            "--validator",
            validator,
            "--amount",
            "1.5",
            "--testnet",
        ]),
        signed_envelope(
            "staking delegate",
            "delegate_hype",
            json!({
                "validator": validator,
                "amount": "1.5",
                "asset": "HYPE",
                "network": "Testnet",
                "reversibility": "partially_reversible"
            })
        )
    );

    assert_eq!(
        signed(&["staking", "claim-rewards", "--testnet"]),
        signed_envelope(
            "staking claim-rewards",
            "claim_staking_rewards",
            json!({
                "asset": "HYPE",
                "network": "Testnet",
                "reversibility": "irreversible"
            })
        )
    );

    assert_eq!(
        signed(&[
            "vault",
            "deposit",
            "--vault",
            vault,
            "--amount",
            "10",
            "--testnet",
        ]),
        signed_envelope(
            "vault deposit",
            "deposit_usdc_to_vault",
            json!({
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
            })
        )
    );

    assert_eq!(
        dry_run(
            Some(&server),
            true,
            &["borrowlend", "withdraw", "USDC", "--max", "--testnet"]
        ),
        signed_envelope(
            "borrowlend withdraw",
            "withdraw_borrowlend",
            json!({
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
            })
        )
    );
}

/// Every signing command's preview names the signer, the account it acts as and
/// the vault, and a signer that can't be resolved is `null` with a warning,
/// wherever else the command's own `args` mention the signer.
#[test]
fn signed_previews_name_the_acting_account_and_warn_about_a_missing_signer() {
    let home = IsolatedHome::new();
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
        let with_key = run_json(
            home.json_command()
                .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
                .args(["--dry-run", "--testnet"])
                .args(command),
        );
        assert_eq!(with_key["signer"], signer, "{command:?}");
        assert_eq!(with_key["acting_as"], signer, "{command:?}");
        assert!(with_key["vault_address"].is_null(), "{command:?}");
        assert!(with_key.get("signer_warning").is_none(), "{command:?}");

        let without = run_json(
            home.json_command()
                .args(["--dry-run", "--testnet"])
                .args(command),
        );
        assert!(without["signer"].is_null(), "{command:?}");
        assert!(without["acting_as"].is_null(), "{command:?}");
        assert!(without["vault_address"].is_null(), "{command:?}");
        assert!(
            without["signer_warning"].as_str().is_some(),
            "a null signer carries its warning: {command:?}"
        );
    }
}

/// `--dry-run` previews the command that ran, not the command a shortcut expands to.
#[tokio::test]
async fn shortcut_orders_preview_under_their_own_command_name() {
    let server = ExchangeFixture::start().await;

    for command in ["buy", "sell"] {
        let output = dry_run(
            Some(&server),
            false,
            &[command, "--coin", "BTC", "--price", "50000", "--size", "0.001"],
        );
        assert_eq!(output["command"], command);
        assert_eq!(output["would_execute"], "submit_order");
        assert_eq!(output["args"]["side"], command);
    }
}
