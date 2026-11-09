mod support;

use predicates::prelude::*;
use serde_json::Value;
use support::{
    IsolatedHome, TEST_ACCOUNT_PASSPHRASE, VALID_PRIVATE_KEY, expected_address, mount_all_mids,
};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mock_api_wallet_server() -> MockServer {
    let server = MockServer::start().await;
    mount_all_mids(&server, "50000", "3000").await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": {"type": "default"}
        })))
        .mount(&server)
        .await;
    server
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn valid_until(agent_name: &str, expected_prefix: &str) -> u64 {
    agent_name
        .strip_prefix(expected_prefix)
        .unwrap_or_else(|| {
            panic!("agent name {agent_name:?} should start with {expected_prefix:?}")
        })
        .parse()
        .unwrap_or_else(|_| {
            panic!("agent name {agent_name:?} should end in a millisecond timestamp")
        })
}

fn assert_expires_in(expiry_ms: u64, expected_ms: u64) {
    let tolerance_ms = 120_000;
    let now = now_ms();
    assert!(
        expiry_ms + tolerance_ms >= now + expected_ms && expiry_ms <= now + expected_ms,
        "expiry {expiry_ms} is not about {expected_ms}ms after now ({now})"
    );
}

#[test]
fn api_wallet_create_dry_run_redacts_generated_private_key() {
    let env = IsolatedHome::new();

    let output = env
        .command()
        .args([
            "--format",
            "json",
            "--dry-run",
            "api-wallet",
            "create",
            "--name",
            "bot-qa",
            "--expires-in",
            "30d",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["status"], "dry_run");
    assert_eq!(json["approval_action"]["type"], "approveAgent");
    assert_eq!(json["name"], "bot-qa");
    let agent_name = json["action_agent_name"].as_str().unwrap();
    let expiry = valid_until(agent_name, "bot-qa valid_until ");
    assert_expires_in(expiry, 30 * 24 * 60 * 60 * 1000);
    assert_eq!(json["expires_at"].as_u64(), Some(expiry));
    let agent_address = json["approval_action"]["agentAddress"].as_str().unwrap();
    assert_eq!(agent_address.len(), 42, "{agent_address}");
    assert!(agent_address.starts_with("0x"));
    assert!(!stdout.contains("private_key"));
}

#[test]
fn api_wallet_agent_address_takes_bare_hex_and_the_zero_address_and_refuses_the_rest_as_unsupported()
 {
    let env = IsolatedHome::new();
    let dry_run = |agent: &str| {
        let mut command = env.command();
        command.args([
            "--format",
            "json",
            "--dry-run",
            "api-wallet",
            "approve",
            "--agent-address",
            agent,
        ]);
        command
    };

    for accepted in [
        "0x00000000000000000000000000000000000000ab",
        "00000000000000000000000000000000000000ab",
        "0X00000000000000000000000000000000000000ab",
        "0x0000000000000000000000000000000000000000",
    ] {
        let output = dry_run(accepted)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let json: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            json["approval_action"]["agentAddress"]
                .as_str()
                .unwrap()
                .to_lowercase(),
            format!("0x{}", &accepted[accepted.len() - 40..]).to_lowercase(),
            "{accepted}"
        );
    }
    dry_run("not-an-address")
        .assert()
        .code(13)
        .stdout(predicate::str::contains("Invalid address: not-an-address"));
}

#[test]
fn api_wallet_create_rejects_names_longer_than_protocol_limit_locally() {
    let env = IsolatedHome::new();

    env.command()
        .args(["api-wallet", "create", "--name", "name-longer-than-16"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("between 1 and 16 characters"));
}

#[test]
fn api_wallet_create_counts_unicode_name_length_by_character() {
    let env = IsolatedHome::new();

    env.command()
        .args([
            "--dry-run",
            "api-wallet",
            "create",
            "--name",
            "éééééééééééééééé",
        ])
        .assert()
        .success();

    env.command()
        .args([
            "--dry-run",
            "api-wallet",
            "create",
            "--name",
            "ééééééééééééééééé",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("between 1 and 16 characters"));
}

#[tokio::test]
async fn api_wallet_create_submits_approve_agent_and_prints_generated_key_once() {
    let env = IsolatedHome::new();
    let server = mock_api_wallet_server().await;
    let master = expected_address(VALID_PRIVATE_KEY);

    let output = env
        .command_with_server(&server)
        .args([
            "--format",
            "json",
            "--private-key",
            VALID_PRIVATE_KEY,
            "api-wallet",
            "create",
            "--name",
            "bot-live",
            "--expires-in",
            "1d",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);
    let json: Value = serde_json::from_slice(&output).unwrap();
    let private_key = json["private_key"].as_str().unwrap();

    assert_eq!(json["status"], "submitted");
    assert_eq!(json["master_address"], master);
    assert!(private_key.starts_with("0x"));
    assert_eq!(stdout.matches(private_key).count(), 1);

    let requests = server.received_requests().await.unwrap();
    let exchange = requests
        .iter()
        .find(|request| request.url.path() == "/exchange")
        .expect("expected approveAgent exchange request");
    let body: Value = serde_json::from_slice(&exchange.body).unwrap();
    assert_eq!(body["action"]["type"], "approveAgent");
    assert_eq!(body["action"]["hyperliquidChain"], "Mainnet");
    let expiry = valid_until(
        body["action"]["agentName"].as_str().unwrap(),
        "bot-live valid_until ",
    );
    assert_expires_in(expiry, 24 * 60 * 60 * 1000);
    assert_eq!(json["expires_at"].as_u64(), Some(expiry));
    assert_eq!(body["action"]["nonce"], body["nonce"]);
    assert!(body["signature"]["r"].as_str().unwrap().starts_with("0x"));
    assert!(!String::from_utf8_lossy(&exchange.body).contains(private_key));
}

#[tokio::test]
async fn api_wallet_generated_key_projection_flags_fail_before_approval() {
    for projection_args in [
        vec!["--select", "status"],
        vec!["--results-only"],
        vec!["--max-results", "1"],
    ] {
        let env = IsolatedHome::new();
        let server = mock_api_wallet_server().await;
        let mut args = vec!["--format", "json"];
        args.extend(projection_args);
        args.extend([
            "--private-key",
            VALID_PRIVATE_KEY,
            "api-wallet",
            "create",
            "--name",
            "bot-live",
        ]);

        let output = env
            .command_with_server(&server)
            .args(args)
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone();
        let json: Value = serde_json::from_slice(&output).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("private key exactly once")
        );

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.url.path() != "/exchange"),
            "projection guard must fail before approveAgent is submitted"
        );
    }
}

#[tokio::test]
async fn api_wallet_list_uses_selected_master_address_for_agent_lookup() {
    let env = IsolatedHome::new();
    let server = mock_api_wallet_server().await;
    let master = expected_address(VALID_PRIVATE_KEY);

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args([
            "--private-key",
            VALID_PRIVATE_KEY,
            "api-wallet",
            "create",
            "--name",
            "stored-bot",
        ])
        .assert()
        .success();

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "extraAgents"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "stored-bot",
                "address": "0x0000000000000000000000000000000000000001",
                "validUntil": 1800000000000_u64
            }
        ])))
        .mount(&server)
        .await;

    let output = env
        .account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args([
            "--format",
            "json",
            "--private-key",
            VALID_PRIVATE_KEY,
            "api-wallet",
            "list",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["master_address"], master);
    assert_eq!(json["api_wallets"][0]["name"], "stored-bot");

    let requests = server.received_requests().await.unwrap();
    let extra_agents = requests
        .iter()
        .find_map(|request| {
            if request.url.path() != "/info" {
                return None;
            }
            let body: Value = serde_json::from_slice(&request.body).ok()?;
            (body["type"] == "extraAgents").then_some(body)
        })
        .expect("expected extraAgents request");
    assert_eq!(
        extra_agents["user"].as_str().unwrap().to_ascii_lowercase(),
        master.to_ascii_lowercase()
    );
}

#[test]
fn api_wallet_revoke_requires_name_or_unnamed_in_cli_and_schema() {
    let env = IsolatedHome::new();

    env.command()
        .env("HYPERLIQUID_FORMAT", "pretty")
        .args(["api-wallet", "revoke"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--name"));

    env.command()
        .args(["api-wallet", "revoke", "--name", "bot", "--unnamed"])
        .assert()
        .failure()
        .code(2);

    let output = env
        .command()
        .args(["--format", "json", "schema", "api-wallet", "revoke"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        json["json_schema"]["oneOf"],
        serde_json::json!([{"required": ["name"]}, {"required": ["unnamed"]}])
    );
}

#[tokio::test]
async fn api_wallet_revoke_unnamed_replaces_the_unnamed_agent_with_a_throwaway() {
    let env = IsolatedHome::new();
    let server = mock_api_wallet_server().await;

    env.command_with_server(&server)
        .args([
            "--format",
            "json",
            "--private-key",
            VALID_PRIVATE_KEY,
            "api-wallet",
            "revoke",
            "--unnamed",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("private_key").not())
        .stdout(predicate::str::contains("Unnamed API wallet replaced"));

    let requests = server.received_requests().await.unwrap();
    let exchange = requests
        .iter()
        .find(|request| request.url.path() == "/exchange")
        .expect("expected revoke replacement exchange request");
    let body: Value = serde_json::from_slice(&exchange.body).unwrap();
    assert_eq!(body["action"]["type"], "approveAgent");
    // An unnamed approval is what deregisters the previous unnamed agent; a
    // name would register a named one instead.
    assert!(
        body["action"]["agentName"]
            .as_str()
            .is_none_or(str::is_empty),
        "{}",
        body["action"]
    );
    let replacement = body["action"]["agentAddress"].as_str().unwrap();
    assert_ne!(
        replacement.to_ascii_lowercase(),
        expected_address(VALID_PRIVATE_KEY).to_ascii_lowercase(),
        "the replacement agent must be a throwaway key, not the master"
    );
}

#[tokio::test]
async fn api_wallet_revoke_submits_short_lived_replacement_payload() {
    let env = IsolatedHome::new();
    let server = mock_api_wallet_server().await;

    env.command_with_server(&server)
        .args([
            "--format",
            "json",
            "--private-key",
            VALID_PRIVATE_KEY,
            "api-wallet",
            "revoke",
            "--name",
            "bot-qa",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("private_key").not());

    let requests = server.received_requests().await.unwrap();
    let exchange = requests
        .iter()
        .find(|request| request.url.path() == "/exchange")
        .expect("expected revoke replacement exchange request");
    let body: Value = serde_json::from_slice(&exchange.body).unwrap();
    assert_eq!(body["action"]["type"], "approveAgent");
    let expiry = valid_until(
        body["action"]["agentName"].as_str().unwrap(),
        "bot-qa valid_until ",
    );
    assert_expires_in(expiry, 60_000);
    let replacement = body["action"]["agentAddress"].as_str().unwrap();
    assert_eq!(replacement.len(), 42, "{replacement}");
    assert_ne!(
        replacement.to_ascii_lowercase(),
        expected_address(VALID_PRIVATE_KEY).to_ascii_lowercase(),
        "the replacement agent must be a throwaway key, not the master"
    );
}

#[test]
fn api_wallet_revoke_rejects_names_longer_than_protocol_limit_locally() {
    let env = IsolatedHome::new();

    env.command()
        .args(["api-wallet", "revoke", "--name", "name-longer-than-16"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("between 1 and 16 characters"));
}
