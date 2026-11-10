mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::signing::user_signed::APPROVE_AGENT;
use support::signing::{recover_user_signer, test_signer};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, stdout_json, unix_millis};

const AGENT: &str = "0x00000000000000000000000000000000000000ab";
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

async fn exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// The expiry in an approval's agent name, `<name> valid_until <ms>`: the
/// protocol carries an agent's expiry in its name.
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

/// The one `approveAgent` request `server` received, checked to be signed by
/// the test key as the user-signed typed data the exchange requires, and to
/// carry the nonce it signed in both places.
async fn signed_approval(server: &ExchangeFixture, chain: &str) -> Value {
    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    let body = bodies.into_iter().next().unwrap();
    assert_eq!(body["action"]["type"], "approveAgent");
    assert_eq!(body["action"]["hyperliquidChain"], chain);
    assert_eq!(body["action"]["nonce"], body["nonce"]);
    assert!(body["vaultAddress"].is_null());
    assert_eq!(recover_user_signer(&body, APPROVE_AGENT), test_signer());
    body
}

#[test]
fn api_wallet_create_dry_run_redacts_generated_private_key() {
    let env = IsolatedHome::new();
    let before = unix_millis();

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
    let after = unix_millis();
    let json = stdout_json(&output);

    assert_eq!(json["status"], "dry_run");
    assert_eq!(json["approval_action"]["type"], "approveAgent");
    assert_eq!(json["name"], "bot-qa");
    let expiry = valid_until(
        json["action_agent_name"].as_str().unwrap(),
        "bot-qa valid_until ",
    );
    assert!((before + 30 * DAY_MS..=after + 30 * DAY_MS).contains(&expiry));
    assert_eq!(json["expires_at"].as_u64(), Some(expiry));
    let agent_address = json["approval_action"]["agentAddress"].as_str().unwrap();
    assert_eq!(agent_address.len(), 42, "{agent_address}");
    assert!(agent_address.starts_with("0x"));
    assert!(!String::from_utf8_lossy(&output).contains("private_key"));
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
        let json = support::run_json(&mut dry_run(accepted));
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
fn api_wallet_approve_needs_an_agent_address_or_generate() {
    let env = IsolatedHome::new();

    env.command()
        .args(["--format", "json", "--dry-run", "api-wallet", "approve"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "api-wallet approve requires --agent-address or --generate",
        ));
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
    let server = exchange().await;
    let master = expected_address(VALID_PRIVATE_KEY);
    let before = unix_millis();

    let output = env
        .live_command(&server)
        .args([
            "--format",
            "json",
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
    let after = unix_millis();
    let stdout = String::from_utf8_lossy(&output);
    let json = stdout_json(&output);
    let private_key = json["private_key"].as_str().unwrap();

    assert_eq!(json["status"], "submitted");
    assert_eq!(json["master_address"], master);
    assert_eq!(stdout.matches(private_key).count(), 1);

    let body = signed_approval(&server, "Mainnet").await;
    let expiry = valid_until(
        body["action"]["agentName"].as_str().unwrap(),
        "bot-live valid_until ",
    );
    assert!((before + DAY_MS..=after + DAY_MS).contains(&expiry));
    assert_eq!(json["expires_at"].as_u64(), Some(expiry));
    assert_eq!(
        body["action"]["agentAddress"]
            .as_str()
            .unwrap()
            .to_lowercase(),
        expected_address(private_key).to_lowercase(),
        "the approved agent is the one whose key was printed"
    );
    assert!(!body.to_string().contains(private_key));
}

#[tokio::test]
async fn api_wallet_approve_submits_the_given_agent_address_as_signed_typed_data() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let before = unix_millis();

    let json = support::run_json(env.live_json(&server).args([
        "api-wallet",
        "approve",
        "--agent-address",
        AGENT,
        "--name",
        "desk-bot",
        "--expires-in",
        "7d",
    ]));
    let after = unix_millis();

    let body = signed_approval(&server, "Testnet").await;
    assert_eq!(
        body["action"]["agentAddress"]
            .as_str()
            .unwrap()
            .to_lowercase(),
        AGENT
    );
    let expiry = valid_until(
        body["action"]["agentName"].as_str().unwrap(),
        "desk-bot valid_until ",
    );
    assert!((before + 7 * DAY_MS..=after + 7 * DAY_MS).contains(&expiry));
    assert_eq!(
        body["action"].as_object().unwrap().len(),
        6,
        "type, chain, signature chain, agent, name and nonce only: {}",
        body["action"]
    );

    assert_eq!(json["status"], "submitted");
    assert_eq!(json["action"], "api_wallet_approve");
    assert_eq!(json["master_address"], expected_address(VALID_PRIVATE_KEY));
    assert_eq!(
        json["agent_address"].as_str().unwrap().to_lowercase(),
        AGENT
    );
    assert_eq!(json["generated"], false);
    assert_eq!(json["expires_at"].as_u64(), Some(expiry));
    assert!(json.get("private_key").is_none_or(Value::is_null), "{json}");
}

#[tokio::test]
async fn api_wallet_approve_generate_approves_a_new_key_and_prints_it_once() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let output = env
        .live_json(&server)
        .args(["api-wallet", "approve", "--generate"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = stdout_json(&output);
    let private_key = json["private_key"].as_str().expect("the generated key");

    let body = signed_approval(&server, "Testnet").await;
    assert_eq!(
        body["action"]["agentAddress"]
            .as_str()
            .unwrap()
            .to_lowercase(),
        expected_address(private_key).to_lowercase(),
        "the approved agent is the one whose key was printed"
    );
    assert_ne!(
        expected_address(private_key).to_lowercase(),
        test_signer(),
        "a generated agent is not the master"
    );
    assert!(
        body["action"]["agentName"]
            .as_str()
            .is_none_or(str::is_empty),
        "{}",
        body["action"]
    );
    assert_eq!(json["generated"], true);
    assert_eq!(
        String::from_utf8_lossy(&output)
            .matches(private_key)
            .count(),
        1
    );
    assert!(!body.to_string().contains(private_key));
}

#[tokio::test]
async fn api_wallet_generated_key_projection_flags_fail_before_approval() {
    for projection_args in [
        vec!["--select", "status"],
        vec!["--results-only"],
        vec!["--max-results", "1"],
    ] {
        let env = IsolatedHome::new();
        let server = exchange().await;

        let output = env
            .live_json(&server)
            .args(projection_args)
            .args(["api-wallet", "create", "--name", "bot-live"])
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone();
        assert!(
            stdout_json(&output)["error"]
                .as_str()
                .unwrap()
                .contains("private key exactly once")
        );
        assert!(
            server.exchange_actions().await.is_empty(),
            "projection guard must fail before approveAgent is submitted"
        );
    }
}

#[tokio::test]
async fn api_wallet_list_asks_for_the_agents_of_the_signing_master() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "extraAgents"}),
            json!([{
                "name": "stored-bot",
                "address": "0x0000000000000000000000000000000000000001",
                "validUntil": 1800000000000_u64
            }]),
        )
        .await;
    let master = expected_address(VALID_PRIVATE_KEY);

    let json = support::run_json(env.live_json(&server).args(["api-wallet", "list"]));

    assert_eq!(json["master_address"], master);
    assert_eq!(json["api_wallets"][0]["name"], "stored-bot");
    let requests = server.info_requests("extraAgents").await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["user"].as_str().unwrap().to_lowercase(),
        master.to_lowercase()
    );
}

#[test]
fn api_wallet_revoke_requires_name_or_unnamed_in_cli_and_schema() {
    let env = IsolatedHome::new();

    env.command()
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

    let json = support::run_json(env.command().args([
        "--format",
        "json",
        "schema",
        "api-wallet",
        "revoke",
    ]));
    assert_eq!(
        json["json_schema"]["oneOf"],
        json!([{"required": ["name"]}, {"required": ["unnamed"]}])
    );
}

#[tokio::test]
async fn api_wallet_revoke_unnamed_replaces_the_unnamed_agent_with_a_throwaway() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    env.live_json(&server)
        .args(["api-wallet", "revoke", "--unnamed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("private_key").not())
        .stdout(predicate::str::contains("Unnamed API wallet replaced"));

    let body = signed_approval(&server, "Testnet").await;
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
        replacement.to_lowercase(),
        test_signer(),
        "the replacement agent must be a throwaway key, not the master"
    );
}

#[tokio::test]
async fn api_wallet_revoke_submits_short_lived_replacement_payload() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let before = unix_millis();

    env.live_json(&server)
        .args(["api-wallet", "revoke", "--name", "bot-qa"])
        .assert()
        .success()
        .stdout(predicate::str::contains("private_key").not());
    let after = unix_millis();

    let body = signed_approval(&server, "Testnet").await;
    let expiry = valid_until(
        body["action"]["agentName"].as_str().unwrap(),
        "bot-qa valid_until ",
    );
    assert!((before + 60_000..=after + 60_000).contains(&expiry));
    let replacement = body["action"]["agentAddress"].as_str().unwrap();
    assert_ne!(
        replacement.to_lowercase(),
        test_signer(),
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
