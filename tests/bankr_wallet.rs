//! `wallet login` / `wallet logout` against a stubbed Bankr API, plus the
//! `--bankr` signer's missing-credential path.

mod support;

use alloy::signers::SignerSync;
use hyperliquid_cli::bankr::{BankrClient, BankrTypedData};
use predicates::prelude::*;
use serde_json::Value;
use support::IsolatedHome;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BANKR_KEY: &str = "bk_test_0123456789";
const BANKR_ADDRESS: &str = "0xB776A1972Ed36586B9bD0A19AD7d6031f31abd68";

fn wallet_me_body() -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "wallets": [
            {"chain": "evm", "address": BANKR_ADDRESS},
            {"chain": "solana", "address": "CPRoFRSFQqTbuJ9LQnTTEautZMvKYDahhSyS4riWea8j"}
        ],
        "socialAccounts": [{"platform": "twitter", "username": "tester"}],
        "refCode": "TEST-BNKR",
        "bankrClub": {"hasBankrClub": false, "active": false}
    })
}

fn config_json(env: &IsolatedHome) -> Value {
    let text = std::fs::read_to_string(env.config_file_path()).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn wallet_login_stores_the_key_and_address_and_logout_removes_them() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wallet/me"))
        .and(header("X-API-Key", BANKR_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(wallet_me_body()))
        .mount(&server)
        .await;

    env.command()
        .args([
            "wallet",
            "login",
            "--api-key",
            BANKR_KEY,
            "--api-url",
            server.uri().as_str(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(BANKR_ADDRESS));

    let config = config_json(&env);
    assert_eq!(config["bankr_api_key"], BANKR_KEY);
    assert_eq!(config["bankr_address"], BANKR_ADDRESS);
    assert_eq!(config["bankr_api_url"], server.uri());

    // `--bankr wallet show` resolves the stored address without another request.
    env.command()
        .args(["--bankr", "wallet", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains(BANKR_ADDRESS))
        .stdout(predicate::str::contains("Bankr hosted wallet"));

    env.command()
        .args(["wallet", "logout", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Logged out of Bankr"));

    let config = config_json(&env);
    assert!(config.get("bankr_api_key").is_none());
    assert!(config.get("bankr_address").is_none());
    assert!(config.get("bankr_api_url").is_none());
}

#[tokio::test]
async fn wallet_login_refuses_a_key_the_api_rejects() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wallet/me"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({"error": "Invalid API key"})),
        )
        .mount(&server)
        .await;

    env.command()
        .args([
            "wallet",
            "login",
            "--api-key",
            BANKR_KEY,
            "--api-url",
            server.uri().as_str(),
        ])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("wallet login"));

    assert!(
        env.config_file_candidates()
            .iter()
            .all(|path| !path.exists()),
        "a rejected key must not be written to the config"
    );
}

#[test]
fn wallet_login_dry_run_does_not_call_the_api() {
    let env = IsolatedHome::new();
    env.command()
        .args([
            "--dry-run",
            "wallet",
            "login",
            "--api-key",
            BANKR_KEY,
            "--api-url",
            "http://127.0.0.1:9",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would execute"))
        .stdout(predicate::str::contains("validate_and_store_bankr_api_key"));
}

#[test]
fn bankr_flag_without_credentials_names_wallet_login() {
    let env = IsolatedHome::new();
    env.command()
        .args(["--format", "json", "--bankr", "wallet", "show"])
        .env_remove("BANKR_API_KEY")
        .assert()
        .code(10);
}

#[test]
fn bankr_conflicts_with_the_other_signer_flags() {
    let env = IsolatedHome::new();
    env.command()
        .args(["--bankr", "--account", "main", "wallet", "show"])
        .assert()
        .code(2);
}

/// The signature Bankr must return, in the `0x<r><s><v>` shape its API uses.
fn signature_hex(private_key: &str, typed_data: &Value) -> String {
    let signer: hypersdk::hypercore::PrivateKeySigner = private_key.parse().unwrap();
    let typed: alloy::dyn_abi::TypedData = serde_json::from_value(typed_data.clone()).unwrap();
    let signature = signer.sign_dynamic_typed_data_sync(&typed).unwrap();
    format!("0x{}", hex::encode(signature.as_bytes()))
}

fn agent_document() -> Value {
    let value = hyperliquid_cli::bankr::agent_typed_data(
        hypersdk::hypercore::Chain::Mainnet,
        alloy::primitives::B256::repeat_byte(0x22),
    );
    value.document().clone()
}

#[tokio::test]
async fn a_signature_from_another_key_is_refused() {
    let server = MockServer::start().await;
    let expected: hypersdk::Address = "0x0000000000000000000000000000000000000009"
        .parse()
        .unwrap();
    let document = agent_document();
    let other_key = "0x0000000000000000000000000000000000000000000000000000000000000008";
    let signature = signature_hex(other_key, &document);

    Mock::given(method("POST"))
        .and(path("/wallet/sign"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "signature": signature,
            "signatureType": "eth_signTypedData_v4"
        })))
        .mount(&server)
        .await;

    let client = BankrClient::new(server.uri(), BANKR_KEY);
    let typed = BankrTypedData::parse(document).unwrap();
    let err = client.sign(&typed, expected).await.unwrap_err();
    assert_eq!(err.exit_code(), 10);
    assert!(err.to_string().contains("bankr_signer_mismatch"), "{err}");
}

#[tokio::test]
async fn the_right_key_signs_and_is_accepted() {
    let server = MockServer::start().await;
    let key = "0x0000000000000000000000000000000000000000000000000000000000000009";
    let signer: hypersdk::hypercore::PrivateKeySigner = key.parse().unwrap();
    let document = agent_document();

    Mock::given(method("POST"))
        .and(path("/wallet/sign"))
        .and(header("X-API-Key", BANKR_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "signature": signature_hex(key, &document),
            "signer": signer.address().to_string(),
            "signatureType": "eth_signTypedData_v4"
        })))
        .mount(&server)
        .await;

    let client = BankrClient::new(server.uri(), BANKR_KEY);
    let typed = BankrTypedData::parse(document).unwrap();
    let signature = client.sign(&typed, signer.address()).await.unwrap();
    let alloy_signature: alloy::primitives::Signature = signature.into();
    assert_eq!(
        alloy_signature
            .recover_address_from_prehash(&typed.signing_hash().unwrap())
            .unwrap(),
        signer.address()
    );
}

#[test]
fn wallet_login_refuses_plaintext_http_to_a_remote_host() {
    let env = IsolatedHome::new();
    env.command()
        .args([
            "wallet",
            "login",
            "--api-key",
            BANKR_KEY,
            "--api-url",
            "http://api.bankr.bot",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("loopback"))
        .stderr(predicate::str::contains("Bankr API URL"));
}

#[tokio::test]
async fn bankr_requests_do_not_follow_redirects() {
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(wallet_me_body()))
        .expect(0)
        .mount(&destination)
        .await;

    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wallet/me"))
        .and(header("X-API-Key", BANKR_KEY))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/wallet/me", destination.uri())),
        )
        .expect(1)
        .mount(&origin)
        .await;

    let client = BankrClient::new(origin.uri(), BANKR_KEY);
    assert!(client.wallet().await.is_err());
    destination.verify().await;
}

fn wallet_me_body_with_address(address: &str) -> Value {
    serde_json::json!({
        "success": true,
        "wallets": [{"chain": "evm", "address": address}]
    })
}

#[tokio::test]
async fn a_cached_address_survives_only_for_the_login_endpoint() {
    let env = IsolatedHome::new();
    let login_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wallet/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(wallet_me_body()))
        .expect(1)
        .mount(&login_server)
        .await;

    env.command()
        .args([
            "wallet",
            "login",
            "--api-key",
            BANKR_KEY,
            "--api-url",
            login_server.uri().as_str(),
        ])
        .assert()
        .success();

    // Same endpoint: the stored address is used without another request.
    env.command()
        .args(["--bankr", "wallet", "show"])
        .env("BANKR_API_URL", login_server.uri())
        .assert()
        .success()
        .stdout(predicate::str::contains(BANKR_ADDRESS));
    login_server.verify().await;

    // A different endpoint can hold a different wallet, so the address is
    // re-resolved instead of reusing the cached one.
    let other = "0x00000000000000000000000000000000000000aa";
    let other_display = "0x00000000000000000000000000000000000000AA";
    let other_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wallet/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(wallet_me_body_with_address(other)))
        .expect(1)
        .mount(&other_server)
        .await;

    env.command()
        .args(["--bankr", "wallet", "show"])
        .env("BANKR_API_URL", other_server.uri())
        .assert()
        .success()
        .stdout(predicate::str::contains(other_display));
    other_server.verify().await;
}

#[test]
fn wallet_logout_without_a_config_file_is_a_noop() {
    let env = IsolatedHome::new();
    env.command()
        .args(["wallet", "logout", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No Bankr login is stored"));
}
