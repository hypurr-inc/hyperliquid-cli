mod support;

use predicates::prelude::*;
use support::IsolatedHome;

#[test]
fn bracket_requires_size_or_amount() {
    IsolatedHome::new()
        .command()
        .args([
            "--dry-run",
            "--testnet",
            "orders",
            "bracket",
            "--coin",
            "ETH",
            "--side",
            "buy",
            "--take-profit",
            "+10%",
            "--stop-loss",
            "-5%",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--size").or(predicate::str::contains("--amount")))
        .stderr(predicate::str::contains(
            "hyperliquid --format json --dry-run orders bracket --coin ETH --side buy --size 0.1 --take-profit +10% --stop-loss -5%",
        ));
}

#[test]
fn buy_help_is_registered() {
    IsolatedHome::new()
        .command()
        .args(["buy", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--coin"))
        .stdout(predicate::str::contains("--size"))
        .stdout(predicate::str::contains("Examples:"))
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn orders_bracket_help_includes_examples() {
    IsolatedHome::new()
        .command()
        .args(["orders", "bracket", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Examples:"))
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn bracket_refuses_a_zero_entry_timeout() {
    IsolatedHome::new()
        .command()
        .args([
            "--dry-run",
            "--testnet",
            "orders",
            "bracket",
            "--coin",
            "ETH",
            "--side",
            "buy",
            "--size",
            "0.1",
            "--take-profit",
            "+10%",
            "--stop-loss",
            "-5%",
            "--entry-timeout",
            "0s",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("greater than zero"));
}
