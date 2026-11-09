mod support;

use predicates::prelude::*;
use serde_json::Value;
use support::{IsolatedHome, hermetic_command};

const OWS_ADDRESS: &str = "0x0000000000000000000000000000000000000001";
const OWS_FAIL_CLOSED: &str = "does not have a resolved wallet for live signing";

fn stdout_json(output: &[u8]) -> Value {
    serde_json::from_slice(output).expect("stdout should be one JSON document")
}

#[test]
fn no_arguments_prints_usage_and_exits_zero() {
    hermetic_command()
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("Usage: hyperliquid"));
}

#[test]
fn version_flag_prints_the_crate_version() {
    hermetic_command()
        .arg("--version")
        .assert()
        .code(0)
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_flag_exits_zero_with_usage() {
    hermetic_command()
        .arg("--help")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("Usage: hyperliquid"));
}

#[test]
fn an_unknown_format_value_is_a_usage_error_with_a_json_envelope() {
    let output = hermetic_command()
        .args(["--format", "yaml"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let json = stdout_json(&output);
    assert_eq!(json["category"], "usage");
    assert!(json["error"].as_str().unwrap().contains("invalid"));
}

#[test]
fn a_missing_required_argument_exits_with_usage_code() {
    hermetic_command().args(["perps", "get"]).assert().code(2);
}

#[test]
fn non_tty_usage_errors_are_structured_json_by_default() {
    let output = hermetic_command()
        .args(["wallet", "show", "--bogus"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let json = stdout_json(&output);
    assert_eq!(json["category"], "usage");
    assert_eq!(json["exit_code"], 2);
    assert!(json["error"].as_str().unwrap().contains("--bogus"));
}

#[test]
fn explicit_pretty_usage_errors_stay_human_readable() {
    hermetic_command()
        .args(["--format", "pretty", "wallet", "show", "--bogus"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--bogus"));
}

#[test]
fn agent_mode_usage_errors_are_structured_json() {
    let output = hermetic_command()
        .env("HYPERLIQUID_AGENT", "1")
        .args(["wallet", "show", "--bogus"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let json = stdout_json(&output);
    assert_eq!(json["category"], "usage");
    assert_eq!(json["exit_code"], 2);
}

/// `HYPERLIQUID_AGENT` and `HYPERLIQUID_NO_UPDATE_CHECK` were always checked;
/// the rest of the strict flags used to read a typo as off.
#[test]
fn an_invalid_value_of_any_strict_flag_variable_is_a_configuration_error() {
    for name in hyperliquid_cli::env::strict_flags() {
        let output = hermetic_command()
            .env(name, "banana")
            .args(["--format", "json", "schema", "orders", "create"])
            .assert()
            .code(2)
            .get_output()
            .stdout
            .clone();
        let json = stdout_json(&output);
        assert_eq!(json["reason"], "invalid_config", "{name}");
        assert_eq!(
            json["error"],
            format!(
                "Configuration error: {name} must be one of 1, true, yes, on, 0, false, no, off"
            ),
            "{name}"
        );
    }
}

#[test]
fn an_ows_signer_without_a_resolved_wallet_fails_closed_before_any_live_action() {
    let env = IsolatedHome::new();
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
        let output = env
            .command()
            .args(["--format", "json", "--ows-signer", OWS_ADDRESS])
            .args(command)
            .assert()
            .code(13)
            .get_output()
            .stdout
            .clone();
        let json = stdout_json(&output);
        assert_eq!(json["category"], "unsupported", "{command:?}");
        assert_eq!(json["exit_code"], 13, "{command:?}");
        assert!(
            json["error"].as_str().unwrap().contains(OWS_FAIL_CLOSED),
            "{command:?}: {json}"
        );
    }
}

#[test]
fn an_ows_signer_dry_run_does_not_require_signing() {
    let env = IsolatedHome::new();
    env.command()
        .args([
            "--format",
            "json",
            "--dry-run",
            "--ows-signer",
            OWS_ADDRESS,
            "orders",
            "cancel",
            "123",
        ])
        .assert()
        .success();
}

#[test]
fn a_private_key_flag_with_a_keystore_is_a_usage_error() {
    hermetic_command()
        .args([
            "--private-key",
            "0x0000000000000000000000000000000000000000000000000000000000000009",
            "--keystore",
            "/keystore.json",
            "status",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("cannot be used with"));
}
