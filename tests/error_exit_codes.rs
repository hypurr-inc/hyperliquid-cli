mod support;

use hyperliquid_cli::env;
use predicates::prelude::*;
use support::exchange::ExchangeFixture;
use support::{
    API_OVERRIDE_ENV, IsolatedHome, VALID_PRIVATE_KEY, hermetic_command, json_command,
    run_json_failure,
};

const OWS_ADDRESS: &str = "0x0000000000000000000000000000000000000001";
const OWS_FAIL_CLOSED: &str = "does not have a resolved wallet for live signing";

#[test]
fn no_arguments_and_help_print_usage_and_exit_zero() {
    for args in [&[][..], &["--help"]] {
        hermetic_command()
            .args(args)
            .assert()
            .code(0)
            .stdout(predicate::str::contains("Usage: hyperliquid"));
    }
}

#[test]
fn an_unknown_format_value_is_a_usage_error_with_a_json_envelope() {
    let json = run_json_failure(hermetic_command().args(["--format", "yaml"]), 2);

    assert_eq!(json["category"], "usage");
    assert!(json["error"].as_str().unwrap().contains("invalid"));
}

#[test]
fn a_usage_error_is_json_unless_pretty_output_was_asked_for() {
    let bogus = ["wallet", "show", "--bogus"];

    let piped = run_json_failure(hermetic_command().args(bogus), 2);
    assert_eq!(piped["category"], "usage");
    assert_eq!(piped["exit_code"], 2);
    assert!(piped["error"].as_str().unwrap().contains("--bogus"));

    let agent = run_json_failure(hermetic_command().env(env::AGENT, "1").args(bogus), 2);
    assert_eq!(agent["category"], "usage");
    assert_eq!(agent["exit_code"], 2);

    hermetic_command()
        .args(["--format", "pretty"])
        .args(bogus)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--bogus"));
}

/// `HYPERLIQUID_AGENT` and `HYPERLIQUID_NO_UPDATE_CHECK` were always checked;
/// the rest of the strict flags used to read a typo as off.
#[test]
fn an_invalid_value_of_any_strict_flag_variable_is_a_configuration_error() {
    for name in env::strict_flags() {
        let json = run_json_failure(
            json_command()
                .env(name, "banana")
                .args(["schema", "orders", "create"]),
            2,
        );
        assert_eq!(json["reason"], "invalid_config", "{name}");
        let message = json["error"].as_str().unwrap();
        assert!(
            message.contains(name) && message.contains("must be one of"),
            "{name}: {message}"
        );
    }
}

#[tokio::test]
async fn an_ows_signer_without_a_resolved_wallet_fails_closed_before_any_live_action() {
    let home = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let cases: [&[&str]; 3] = [
        &["orders", "cancel", "123"],
        &[
            "account",
            "abstraction",
            "set",
            "--mode",
            "disabled",
            "--yes",
        ],
        &[
            "api-wallet",
            "approve",
            "--agent-address",
            "0x0000000000000000000000000000000000000002",
            "--name",
            "ows-smoke",
        ],
    ];
    for command in cases {
        let json = run_json_failure(
            home.json_command()
                .env(API_OVERRIDE_ENV, server.uri())
                .args(["--ows-signer", OWS_ADDRESS])
                .args(command),
            13,
        );
        assert_eq!(json["category"], "unsupported", "{command:?}");
        assert_eq!(json["exit_code"], 13, "{command:?}");
        assert!(
            json["error"].as_str().unwrap().contains(OWS_FAIL_CLOSED),
            "{command:?}: {json}"
        );
    }
    assert!(
        server.exchange_bodies().await.is_empty(),
        "nothing was sent to the exchange"
    );
}

#[test]
fn an_ows_signer_dry_run_does_not_require_signing() {
    IsolatedHome::new()
        .json_command()
        .args(["--dry-run", "--ows-signer", OWS_ADDRESS])
        .args(["orders", "cancel", "123"])
        .assert()
        .success();
}

#[test]
fn a_private_key_flag_with_a_keystore_is_a_usage_error() {
    hermetic_command()
        .args(["--private-key", VALID_PRIVATE_KEY])
        .args(["--keystore", "/keystore.json", "status"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("cannot be used with"));
}
