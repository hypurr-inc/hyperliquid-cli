mod support;

use serde_json::Value;
use support::{API_OVERRIDE_ENV, IsolatedHome};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USER: &str = "0x00000000000000000000000000000000000000aa";

async fn mock_info_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "BTC": "50000"
        })))
        .mount(&server)
        .await;
    server
}

fn json_stdout(output: Vec<u8>) -> Value {
    serde_json::from_slice(&output).expect("stdout should be valid JSON")
}

#[tokio::test]
async fn account_abstraction_read_returns_normalized_mode() {
    let env = IsolatedHome::new();
    let server = mock_info_server().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({
            "type": "userAbstraction",
            "user": USER
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::Value::String("unifiedAccount".to_string())),
        )
        .mount(&server)
        .await;

    let out = env
        .command_with_server(&server)
        .args(["--format", "json", "account", "abstraction", USER])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = json_stdout(out);
    assert_eq!(
        json["user"].as_str().unwrap().to_ascii_lowercase(),
        USER.to_ascii_lowercase()
    );
    assert_eq!(json["raw_mode"], "unifiedAccount");
    assert_eq!(json["normalized_mode"], "unified-account");
}

#[test]
fn account_abstraction_set_dry_run_includes_user_set_action() {
    let env = IsolatedHome::new();
    let out = env
        .command()
        .args([
            "--format",
            "json",
            "--dry-run",
            "account",
            "abstraction",
            "set",
            "--mode",
            "disabled",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["command"], "account abstraction set");
    assert_eq!(json["would_execute"], "set_abstraction");
    assert_eq!(json["args"]["mode"], "disabled");
    assert_eq!(json["args"]["protocol_mode"], "disabled");
    assert_eq!(json["args"]["action"]["type"], "userSetAbstraction");
    assert_eq!(json["args"]["action"]["abstraction"], "disabled");
}

#[test]
fn account_abstraction_set_dry_run_never_invents_the_user_it_signs_for() {
    let env = IsolatedHome::new();
    let args = [
        "--format",
        "json",
        "--dry-run",
        "account",
        "abstraction",
        "set",
        "--mode",
        "disabled",
    ];
    let run = |key: Option<&str>| {
        let mut command = env.command();
        if let Some(key) = key {
            command.env(support::PRIVATE_KEY_ENV, key);
        }
        let out = command
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        json_stdout(out)
    };

    let unsigned = run(None);
    assert!(unsigned["args"]["user"].is_null());
    assert!(unsigned["args"]["action"]["user"].is_null());
    assert!(unsigned["signer"].is_null());
    assert!(unsigned["signer_warning"].is_string());

    let signer = support::expected_address(support::VALID_PRIVATE_KEY);
    let signed = run(Some(support::VALID_PRIVATE_KEY));
    assert_eq!(signed["args"]["user"], signer);
    assert_eq!(signed["args"]["action"]["user"], signer);
    assert_eq!(signed["signer"], signer);
    assert_eq!(signed["acting_as"], signer);
}

#[test]
fn master_only_actions_refuse_a_stored_api_wallet_even_under_dry_run() {
    let env = IsolatedHome::new();
    let agent = support::expected_address(support::VALID_PRIVATE_KEY);
    for db in env.accounts_db_candidates() {
        let key_file = db.parent().unwrap().join("accounts.key");
        support::legacy_accounts_fixture::create_accounts_db_with_key_file(
            &db,
            &key_file,
            &[support::legacy_accounts_fixture::Row::new(
                "bot",
                &agent,
                support::VALID_PRIVATE_KEY,
            )
            .agent_of(USER)
            .default_account()],
        );
    }

    let master_only: [&[&str]; 3] = [
        &["account", "abstraction", "set", "--mode", "disabled"],
        &[
            "builder",
            "approve",
            "--builder",
            "0x00000000000000000000000000000000000000bb",
            "--max-fee-rate",
            "0.001%",
        ],
        &["api-wallet", "revoke", "--name", "bot"],
    ];
    for command in master_only {
        for dry_run in [true, false] {
            let mut args = vec!["--format", "json"];
            if dry_run {
                args.push("--dry-run");
            }
            args.extend_from_slice(command);
            if !dry_run && command[0] != "api-wallet" {
                args.push("--yes");
            }
            let output = env
                .account_command_without_passphrase()
                .args(&args)
                .assert()
                .failure()
                .code(13)
                .get_output()
                .stdout
                .clone();
            let text = String::from_utf8(output).unwrap();
            assert!(
                text.contains("must be signed by the master account"),
                "{command:?} dry_run={dry_run}: {text}"
            );
        }
    }
}

#[tokio::test]
async fn account_abstraction_read_uses_api_override() {
    let env = IsolatedHome::new();
    let server = mock_info_server().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({
            "type": "userAbstraction",
            "user": USER
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::Value::String("portfolioMargin".to_string())),
        )
        .mount(&server)
        .await;

    let mut cmd = env.command();
    cmd.env(API_OVERRIDE_ENV, server.uri());
    let out = cmd
        .args(["--format", "json", "account", "abstraction", USER])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = json_stdout(out);
    assert_eq!(json["normalized_mode"], "portfolio-margin");
    assert_eq!(json["raw_mode"], "portfolioMargin");
}
