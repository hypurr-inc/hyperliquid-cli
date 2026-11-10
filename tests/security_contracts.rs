//! Unsafe local-file rejection, secret redaction and fail-closed confirmation:
//! what the CLI refuses before it renders, reads or sends anything.

mod support;

use std::path::Path;

use predicates::prelude::*;

use hyperliquid_cli::env;
use support::{
    FORMAT_ENV, IsolatedHome, TEST_ACCOUNT_PASSPHRASE, VALID_PRIVATE_KEY, run_json,
    run_json_failure,
};

const BTC_ORDER: [&str; 11] = [
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
    "--testnet",
];

/// A dry-run `orders create` carrying `payload`, as the `--payload-json` flag.
fn order_with_payload(home: &IsolatedHome, payload: &str) -> assert_cmd::assert::Assert {
    home.command()
        .args(["--dry-run", "--payload-json", payload])
        .args(BTC_ORDER)
        .assert()
}

/// A dry-run `orders batch-create --orders-file file` run from `cwd`.
fn batch_create(home: &IsolatedHome, cwd: &Path, file: &str) -> assert_cmd::assert::Assert {
    home.command()
        .current_dir(cwd)
        .args(["--dry-run", "orders", "batch-create", "--orders-file", file])
        .arg("--testnet")
        .assert()
}

#[test]
fn payload_file_rejects_absolute_path_before_rendering() {
    let home = IsolatedHome::new();
    let payload = home.tmp_path().join("payload.json");
    std::fs::write(&payload, r#"{"token":"should-not-render"}"#).unwrap();

    home.command()
        .args(["--dry-run", "--payload-file", payload.to_str().unwrap()])
        .args(BTC_ORDER)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "payload path must be relative to the current working directory",
        ))
        .stderr(predicate::str::contains("should-not-render").not());
}

#[test]
fn orders_file_rejects_stdin_invalid_utf8_and_symlinks() {
    let home = IsolatedHome::new();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("binary.json"), [0xff, b's', b'e', b'c']).unwrap();
    std::fs::write(cwd.path().join("orders.json"), r#"{"orders":[]}"#).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("orders.json", cwd.path().join("orders-link.json")).unwrap();

    batch_create(&home, cwd.path(), "-")
        .code(2)
        .stderr(predicate::str::contains(
            "orders file path cannot read from stdin",
        ));

    batch_create(&home, cwd.path(), "binary.json")
        .code(2)
        .stderr(predicate::str::contains(
            "orders file must be valid UTF-8 JSON",
        ))
        .stderr(predicate::str::contains("sec").not());

    #[cfg(unix)]
    batch_create(&home, cwd.path(), "orders-link.json")
        .code(2)
        .stderr(predicate::str::contains(
            "orders file path cannot be a symlink",
        ));
}

#[test]
fn payload_json_rejects_terminal_controls_and_oversized_strings() {
    let home = IsolatedHome::new();

    order_with_payload(&home, r#"{"note":"bad\u001b"}"#)
        .code(2)
        .stderr(predicate::str::contains(
            "payload JSON cannot contain terminal control characters",
        ));

    let oversized = format!(r#"{{"note":"{}"}}"#, "x".repeat(70_000));
    order_with_payload(&home, &oversized)
        .code(2)
        .stderr(predicate::str::contains(
            "payload JSON contains an oversized string",
        ));
}

#[test]
fn a_payload_parse_error_does_not_echo_the_payload() {
    let home = IsolatedHome::new();

    let error = run_json_failure(
        home.json_command()
            .args([
                "--dry-run",
                "--payload-json",
                r#"{"api_key":"should-not-render","#,
            ])
            .args(BTC_ORDER),
        2,
    )
    .to_string();

    assert!(error.contains("invalid payload JSON"), "{error}");
    assert!(!error.contains("should-not-render"), "{error}");
    assert!(!error.contains("api_key"), "{error}");
}

#[test]
fn raw_payload_is_rejected_before_a_command_that_would_ignore_it_runs() {
    let home = IsolatedHome::new();

    for args in [
        &["schema"][..],
        &["wallet", "list"],
        &["perps", "list"],
        &["update"],
    ] {
        let json = run_json_failure(
            home.json_command()
                .args(["--payload-json", "{}"])
                .args(args),
            13,
        );
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("payload input is not supported"),
            "{args:?}: {json}"
        );
    }
}

#[test]
fn prompt_required_commands_fail_closed_in_machine_context_without_confirmation_bypass() {
    let home = IsolatedHome::new();
    let send = [
        "transfer",
        "send",
        "--to",
        "0x0000000000000000000000000000000000000001",
        "--amount",
        "1",
    ];

    for args in [
        &["wallet", "delete", "qa-wallet"][..],
        &BTC_ORDER[..10],
        &send,
    ] {
        let json = run_json_failure(home.json_command().args(args), 13);
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("without confirmation in non-interactive mode"),
            "{args:?}: {json}"
        );
    }

    home.command()
        .env(env::AGENT, "1")
        .args(["--format", "pretty", "wallet", "delete", "qa-wallet"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "without confirmation in non-interactive mode",
        ));
}

#[test]
fn wallet_export_yes_satisfies_confirmation_in_machine_context() {
    let home = IsolatedHome::new();

    let blocked = run_json_failure(
        home.json_command().args(["wallet", "export", "qa-wallet"]),
        13,
    );
    let message = blocked["error"].as_str().unwrap();
    assert!(
        message.contains("without confirmation in non-interactive mode"),
        "{blocked}"
    );
    assert_eq!(blocked["reason"], "confirmation_required");
    let approve = blocked["next"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["when"] == "Only after a human approves")
        .expect("approval step in next");
    assert!(
        approve["command"]
            .as_str()
            .unwrap()
            .ends_with("wallet export qa-wallet --yes"),
        "{blocked}"
    );

    // With --yes the confirmation is answered; the run proceeds to the wallet
    // lookup and fails there because the wallet doesn't exist.
    let accepted = home
        .json_command()
        .args(["wallet", "export", "qa-wallet", "--yes"])
        .assert()
        .failure();
    let accepted = support::json_stdout(&accepted);
    let message = accepted["error"].as_str().unwrap();
    assert!(!message.contains("without confirmation"), "{accepted}");
    assert!(message.contains("qa-wallet"), "{accepted}");
}

#[test]
fn secret_prompt_commands_fail_closed_in_machine_context() {
    let home = IsolatedHome::new();
    let mnemonic = "test test test test test test test test test test test junk";

    for args in [
        vec!["account", "add"],
        vec![
            "account",
            "add",
            VALID_PRIVATE_KEY,
            "--alias",
            "argv-secret",
        ],
        vec![
            "--dry-run",
            "account",
            "add",
            VALID_PRIVATE_KEY,
            "--alias",
            "argv-secret",
        ],
        vec!["wallet", "import"],
        vec!["wallet", "import", VALID_PRIVATE_KEY],
        vec!["--dry-run", "wallet", "import", VALID_PRIVATE_KEY],
        vec!["wallet", "import-mnemonic"],
        vec!["wallet", "import-mnemonic", mnemonic],
        vec!["--dry-run", "wallet", "import-mnemonic", mnemonic],
    ] {
        let json = run_json_failure(home.json_command().args(&args), 13);
        let error = json["error"].as_str().unwrap();
        assert!(
            error.contains("without confirmation in non-interactive mode")
                || error.contains("argv secret input is not supported"),
            "{args:?}: {json}"
        );
    }
}

#[test]
fn prompt_required_dry_runs_remain_machine_readable_previews() {
    let home = IsolatedHome::new();

    let json = run_json(
        home.json_command()
            .args(["--dry-run", "wallet", "delete", "qa-wallet"]),
    );

    assert_eq!(json["command"], "wallet delete");
    assert_eq!(json["dry_run"], true);
}

/// A stored account's alias names a local signing account, not an address: a
/// field that takes a protocol address must refuse it instead of resolving it.
#[test]
fn address_fields_reject_stored_account_aliases() {
    let home = IsolatedHome::new();
    let account = || {
        let mut command = home.account_command(TEST_ACCOUNT_PASSPHRASE);
        command.env(FORMAT_ENV, "json");
        command
    };
    let run = |args: &[&str]| {
        let mut command = account();
        command.arg("--dry-run").args(args);
        command
    };
    home.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["account", "add", VALID_PRIVATE_KEY, "--alias", "alias"])
        .assert()
        .success();

    let accounts = run_json(account().args(["account", "ls"]));
    assert!(accounts.to_string().contains("alias"), "{accounts}");

    for args in [
        &["transfer", "send", "--to", "alias", "--amount", "1"][..],
        &[
            "transfer",
            "spot-send",
            "--to",
            "alias",
            "--token",
            "HYPE",
            "--amount",
            "1",
        ],
        &[
            "transfer",
            "send-asset",
            "--to",
            "alias",
            "--source",
            "spot",
            "--dest",
            "perp",
            "--token",
            "USDC",
            "--amount",
            "1",
        ],
        &["transfer", "withdraw", "--to", "alias", "--amount", "1"],
        &[
            "staking",
            "delegate",
            "--validator",
            "alias",
            "--amount",
            "1",
            "--testnet",
        ],
        &[
            "staking",
            "link",
            "initiate",
            "--user",
            "alias",
            "--testnet",
        ],
        &[
            "vault",
            "deposit",
            "--vault",
            "alias",
            "--amount",
            "1",
            "--testnet",
        ],
        &[
            "vault",
            "withdraw",
            "--vault",
            "alias",
            "--amount",
            "1",
            "--testnet",
        ],
    ] {
        let json = support::json_stdout(&run(args).assert().failure());
        let error = json["error"].as_str().unwrap();
        assert!(
            error.to_ascii_lowercase().contains("address"),
            "{args:?}: {json}"
        );
    }
}

#[test]
fn position_margin_precision_is_validated_before_auth_or_network() {
    let home = IsolatedHome::new();

    home.command()
        .args(["positions", "update-margin", "--coin", "BTC"])
        .args(["--amount", "0.0000001", "--testnet"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "amount supports at most 6 decimal places",
        ))
        .stderr(predicate::str::contains("Authentication required").not());
}
