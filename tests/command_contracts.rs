mod contract_support;
mod support;

use hyperliquid_cli::env;
use serde_json::Value;

use contract_support::{command_path, schema_all};
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, hermetic_command, stdout_json};

#[test]
fn every_registered_command_resolves_to_cli_help() {
    for schema in schema_all() {
        let path = command_path(&schema);
        let args = path
            .split_whitespace()
            .chain(["--help"])
            .collect::<Vec<_>>();
        hermetic_command().args(args).assert().success();
    }
}

fn orders_create(
    home: &IsolatedHome,
    server: &ExchangeFixture,
    network: Option<&str>,
    testnet_flag: bool,
) -> Value {
    let mut command = home.json_command();
    command
        .env(support::API_OVERRIDE_ENV, server.uri())
        .env(env::CI, "1");
    if let Some(network) = network {
        command.env(env::NETWORK, network);
    }
    if testnet_flag {
        command.arg("--testnet");
    }
    let output = command
        .args(["orders", "create", "--coin", "BTC", "--side", "buy"])
        .args(["--price", "50000", "--size", "0.1"])
        .output()
        .unwrap();
    stdout_json(&output.stdout)
}

#[tokio::test]
async fn testnet_runs_skip_the_confirmation_check_however_the_network_was_chosen() {
    let home = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    for (network, flag) in [(Some("testnet"), false), (None, true)] {
        let error = orders_create(&home, &server, network, flag);
        assert_eq!(
            error["reason"], "auth_required",
            "past the confirmation check on testnet ({network:?}, --testnet {flag}): {error}"
        );
    }
    let mainnet = orders_create(&home, &server, Some("mainnet"), false);
    assert_eq!(mainnet["reason"], "confirmation_required", "{mainnet}");
    let default = orders_create(&home, &server, None, false);
    assert_eq!(default["reason"], "confirmation_required", "{default}");
}

const SECRET_KEY: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

#[test]
fn suggested_commands_never_repeat_a_secret_argument() {
    let home = IsolatedHome::new();
    let output = home
        .command()
        .env(env::CI, "1")
        .args(["wallet", "import", SECRET_KEY])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(13));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Can't run `wallet import` without confirmation"),
        "{stderr}"
    );
    assert!(!stderr.contains(&SECRET_KEY[2..]), "{stderr}");

    let recorded = home.read_last_error().to_string();
    assert!(!recorded.contains(&SECRET_KEY[2..]), "{recorded}");

    let output = home
        .command()
        .env(env::CI, "1")
        .args([
            "--format",
            "pretty",
            "wallet",
            "login",
            "--api-key",
            "bk_SECRETVALUE",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("hyperliquid --dry-run --format pretty wallet login"),
        "{stderr}"
    );
    assert!(!stderr.contains("bk_SECRETVALUE"), "{stderr}");
}
