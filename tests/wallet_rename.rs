//! `wallet rename` against a real vault in an isolated home.

mod support;

use serde_json::Value;
use support::{IsolatedHome, TEST_ACCOUNT_PASSPHRASE, expected_address, json_stdout, stdout_json};

const KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000007";
const OTHER_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000008";

/// Import `key` and return the name the vault gave it.
fn import(env: &IsolatedHome, key: &str) -> String {
    let output = env
        .account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["wallet", "import", key])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Alias"))
        .map(|alias| alias.trim().to_string())
        .expect("the import receipt names the wallet")
}

/// `account ls` as `(alias, address, is_default)` rows.
fn wallets(env: &IsolatedHome) -> Vec<(String, String, bool)> {
    let output = env
        .account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["--format", "json", "account", "ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let Value::Array(rows) = stdout_json(&output) else {
        panic!("account ls prints an array");
    };
    let mut wallets: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row["alias"].as_str().unwrap().to_string(),
                row["address"].as_str().unwrap().to_string(),
                row["is_default"].as_bool().unwrap(),
            )
        })
        .collect();
    wallets.sort();
    wallets
}

#[test]
fn rename_changes_the_name_of_a_wallet_and_nothing_else() {
    let env = IsolatedHome::new();
    let address = expected_address(KEY);
    let name = import(&env, KEY);
    assert_eq!(wallets(&env), [(name.clone(), address.clone(), true)]);

    let assert = env
        .account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["--format", "json", "wallet", "rename", &name, "trading"])
        .assert()
        .success();

    let receipt = json_stdout(&assert);
    assert_eq!(receipt["message"], "Wallet renamed");
    assert_eq!(receipt["alias"], "trading");
    assert_eq!(receipt["address"], address);
    assert_eq!(
        wallets(&env),
        [("trading".to_string(), address.clone(), true)],
        "the wallet keeps its address and stays the default under its new name"
    );
    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["wallet", "address"])
        .assert()
        .success()
        .stdout(predicates::str::contains(address));
}

#[test]
fn rename_leaves_the_other_wallets_and_the_default_alone() {
    let env = IsolatedHome::new();
    let first = import(&env, KEY);
    let second = import(&env, OTHER_KEY);

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["wallet", "rename", &first, "cold"])
        .assert()
        .success();

    let mut expected = vec![
        ("cold".to_string(), expected_address(KEY), false),
        (second, expected_address(OTHER_KEY), true),
    ];
    expected.sort();
    assert_eq!(wallets(&env), expected);
}

#[test]
fn rename_of_an_unknown_wallet_fails_and_changes_nothing() {
    let env = IsolatedHome::new();
    let name = import(&env, KEY);
    let before = wallets(&env);

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["--format", "json", "wallet", "rename", "missing", "trading"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("'missing' was not found"));

    assert_eq!(wallets(&env), before);
    assert_eq!(before[0].0, name);
}

#[test]
fn rename_to_a_name_another_wallet_has_fails_and_keeps_both() {
    let env = IsolatedHome::new();
    let first = import(&env, KEY);
    let second = import(&env, OTHER_KEY);
    let before = wallets(&env);

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["wallet", "rename", &first, &second])
        .assert()
        .failure();

    assert_eq!(wallets(&env), before);
}

#[test]
fn rename_refuses_dry_run_without_touching_the_vault() {
    let env = IsolatedHome::new();
    let name = import(&env, KEY);
    let before = wallets(&env);

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["--dry-run", "wallet", "rename", &name, "trading"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--dry-run is not supported for wallet rename",
        ));

    assert_eq!(wallets(&env), before);
}
