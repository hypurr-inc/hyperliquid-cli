mod support;

use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use support::{
    IsolatedHome, TEST_ACCOUNT_PASSPHRASE, expected_address, mock_all_mids_server,
    mock_all_mids_server_with_prices,
};

const IMPORT_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000006";

fn assert_directory_does_not_contain(path: &std::path::Path, needle: &str) {
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                let bytes = fs::read(&path).unwrap();
                let contents = String::from_utf8_lossy(&bytes);
                assert!(
                    !contents.contains(needle),
                    "{} must not contain sensitive material",
                    path.display()
                );
            }
        }
    }
}

#[tokio::test]
async fn setup_create_wallet_saves_config_and_verifies_connection() {
    let env = IsolatedHome::new();
    let server = mock_all_mids_server().await;

    let output = env
        .account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .arg("setup")
        .write_stdin("1\nn\n\n\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("Welcome to Hyperliquid CLI setup"))
        .stderr(predicate::str::contains("Create new wallet"))
        .stderr(predicate::str::contains("Saved config"))
        .stdout(predicate::str::contains("Test query succeeded"))
        .stdout(predicate::str::contains("Setup complete"))
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    let address = stdout
        .split_whitespace()
        .find(|part| part.starts_with("0x"))
        .expect("setup should print generated wallet address")
        .to_string();

    let config_path = env.config_file();
    let config: Value = serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(config["network"], "mainnet");
    assert!(config.get("private_key").is_none());

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["wallet", "address"])
        .assert()
        .success()
        .stdout(predicate::str::contains(address));
}

#[tokio::test]
async fn setup_yes_creates_wallet_and_persists_default_builder_and_referral() {
    let env = IsolatedHome::new();
    let server = mock_all_mids_server().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .and(body_string_contains(r#""type":"approveBuilderFee""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": { "type": "default" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .env(
            "HYPERLIQUID_DEFAULT_BUILDER_ADDRESS",
            "0x00000000000000000000000000000000000000bb",
        )
        .env("HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE", "0.001%")
        .env("HYPERLIQUID_DEFAULT_REFERRAL_CODE", "SETUPYES")
        .args(["setup", "-y"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Setup complete"))
        .stdout(predicate::str::contains("Default builder"))
        .stdout(predicate::str::contains("Default referral"))
        .stderr(predicate::str::contains("Builder approval submitted"))
        .stderr(predicate::str::contains("Choose an option").not())
        .stderr(predicate::str::contains("Default builder address").not())
        .stderr(predicate::str::contains("Default referral code").not());

    let config: Value = serde_json::from_str(&fs::read_to_string(env.config_file()).unwrap())
        .expect("setup -y should write valid config JSON");
    assert_eq!(config["network"], "mainnet");
    assert!(config["default_wallet_id"].as_str().is_some());
    assert_eq!(
        config["default_builder_address"],
        "0x00000000000000000000000000000000000000bb"
    );
    assert_eq!(config["default_builder_fee_rate"], "0.001%");
    assert_eq!(config["default_referral_code"], "SETUPYES");
}

#[tokio::test]
async fn setup_yes_reports_builder_approval_failure_without_failing_setup() {
    let env = IsolatedHome::new();
    let server = mock_all_mids_server().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .and(body_string_contains(r#""type":"approveBuilderFee""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "err",
            "response": "Builder has insufficient balance to be approved."
        })))
        .expect(1)
        .mount(&server)
        .await;

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .env(
            "HYPERLIQUID_DEFAULT_BUILDER_ADDRESS",
            "0x00000000000000000000000000000000000000bb",
        )
        .env("HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE", "0.001%")
        .args(["setup", "-y"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Builder approval failed"))
        .stdout(predicate::str::contains("Setup complete"))
        .stdout(predicate::str::contains("approval failed"));

    let config: Value = serde_json::from_str(&fs::read_to_string(env.config_file()).unwrap())
        .expect("setup -y should keep valid config when approval fails");
    assert!(config["default_wallet_id"].as_str().is_some());
    assert_eq!(
        config["default_builder_address"],
        "0x00000000000000000000000000000000000000bb"
    );
    assert_eq!(config["default_builder_fee_rate"], "0.001%");
}

#[tokio::test]
async fn setup_import_wallet_uses_hidden_prompt_path_and_stores_key_encrypted() {
    let env = IsolatedHome::new();
    let server = mock_all_mids_server().await;
    let address = expected_address(IMPORT_KEY);

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .arg("setup")
        .write_stdin(format!("2\ny\n\n\n{IMPORT_KEY}\n"))
        .assert()
        .success()
        .stderr(predicate::str::contains("Private key:"))
        .stdout(predicate::str::contains("Test query succeeded"))
        .stdout(predicate::str::contains(&address))
        .stdout(predicate::str::contains(IMPORT_KEY).not())
        .stderr(predicate::str::contains(IMPORT_KEY).not());

    let config_path = env.config_file();
    let config: Value = serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(config["network"], "testnet");
    assert!(config.get("private_key").is_none());

    let vault_path = env.ows_vault();
    assert_directory_does_not_contain(&vault_path, IMPORT_KEY);
    assert_directory_does_not_contain(&vault_path, IMPORT_KEY.trim_start_matches("0x"));
    assert!(
        !env.legacy_accounts_key().exists(),
        "raw encryption key must not be stored next to accounts.db"
    );

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["wallet", "address"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(format!("^{address}\n$")).unwrap());
}

#[tokio::test]
async fn setup_connection_failure_has_clear_error_and_nonzero_exit() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .respond_with(ResponseTemplate::new(503).set_body_string("maintenance"))
        .mount(&server)
        .await;

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .arg("setup")
        .write_stdin("1\nn\n")
        .assert()
        .code(12)
        .stderr(predicate::str::contains(
            "The Hyperliquid API failed to process the request.",
        ))
        .stderr(predicate::str::contains("HTTP 503"))
        .stderr(predicate::str::contains("Check your network connection").not());

    assert!(
        !env.config_file().exists(),
        "failed verification must not leave config.json behind"
    );
    assert!(
        !env.ows_vault().exists(),
        "failed verification must not leave OWS vault behind"
    );
}

#[tokio::test]
async fn setup_verifies_exactly_once_against_the_selected_network_only() {
    for (network_answer, selected_is_testnet) in [("y", true), ("n", false)] {
        let env = IsolatedHome::new();
        let mainnet_server = mock_all_mids_server_with_prices("50000.0", "3000.0").await;
        let testnet_server = mock_all_mids_server_with_prices("51000.0", "3100.0").await;

        env.account_command_with_mainnet_and_testnet(
            TEST_ACCOUNT_PASSPHRASE,
            &mainnet_server,
            &testnet_server,
        )
        .arg("setup")
        .write_stdin(format!("1\n{network_answer}\n\n\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("Test query succeeded"));

        let mainnet_requests = mainnet_server.received_requests().await.unwrap().len();
        let testnet_requests = testnet_server.received_requests().await.unwrap().len();
        let (selected, other) = if selected_is_testnet {
            (testnet_requests, mainnet_requests)
        } else {
            (mainnet_requests, testnet_requests)
        };
        assert_eq!(selected, 1, "testnet selected: {selected_is_testnet}");
        assert_eq!(other, 0, "testnet selected: {selected_is_testnet}");
    }
}

#[tokio::test]
async fn setup_reports_exactly_the_config_keys_it_changed() {
    let env = IsolatedHome::new();
    let server = mock_all_mids_server().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": { "type": "default" }
        })))
        .mount(&server)
        .await;

    let output = env
        .account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .env(
            "HYPERLIQUID_DEFAULT_BUILDER_ADDRESS",
            "0x00000000000000000000000000000000000000bb",
        )
        .env("HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE", "0.001%")
        .env("HYPERLIQUID_DEFAULT_REFERRAL_CODE", "SETUPKEYS")
        .args(["--format", "json", "setup", "-y"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: Value = serde_json::from_slice(&output).unwrap();

    // Mainnet is the default network, so setup leaves `network` as it was.
    assert_eq!(
        summary["config_keys_written"],
        serde_json::json!([
            "default_builder_address",
            "default_builder_fee_rate",
            "default_referral_code",
            "default_wallet_id"
        ])
    );
}
