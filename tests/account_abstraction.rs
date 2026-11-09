mod support;

use support::stdout_json;

use serde_json::Value;
use support::IsolatedHome;
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

#[tokio::test]
async fn account_abstraction_read_normalizes_every_protocol_mode() {
    let cases = [
        ("unifiedAccount", "unified-account"),
        ("portfolioMargin", "portfolio-margin"),
        ("dexAbstraction", "dex-abstraction"),
        ("disabled", "disabled"),
        ("default", "default"),
        ("someFutureMode", "unknown"),
    ];
    for (raw_mode, normalized_mode) in cases {
        let env = IsolatedHome::new();
        let server = mock_info_server().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .and(body_partial_json(serde_json::json!({
                "type": "userAbstraction",
                "user": USER
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(Value::String(raw_mode.into())))
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

        let json = stdout_json(&out);
        assert_eq!(
            json["user"].as_str().unwrap().to_ascii_lowercase(),
            USER.to_ascii_lowercase()
        );
        assert_eq!(json["raw_mode"], raw_mode);
        assert_eq!(json["normalized_mode"], normalized_mode, "{raw_mode}");
    }
}

#[test]
fn account_abstraction_set_dry_run_maps_cli_modes_to_protocol_modes() {
    for (cli_mode, protocol_mode) in [
        ("disabled", "disabled"),
        ("unified-account", "unifiedAccount"),
        ("portfolio-margin", "portfolioMargin"),
    ] {
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
                cli_mode,
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
        assert_eq!(json["args"]["mode"], cli_mode);
        assert_eq!(json["args"]["protocol_mode"], protocol_mode);
        assert_eq!(json["args"]["action"]["type"], "userSetAbstraction");
        assert_eq!(json["args"]["action"]["abstraction"], protocol_mode);
    }
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
        stdout_json(&out)
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
    let env =
        IsolatedHome::new().with_stored_account("bot", support::VALID_PRIVATE_KEY, Some(USER));

    // `account abstraction set` is not here: an API wallet signs it as
    // `agentSetAbstraction` (tests/agent_signed_actions.rs).
    let master_only: [&[&str]; 2] = [
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

#[test]
fn account_abstraction_set_help_and_schema_carry_the_protocol_cautions() {
    let env = IsolatedHome::new();
    let text = |args: &[&str]| {
        let output = env
            .command()
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(output).unwrap()
    };
    let help = text(&["account", "abstraction", "set", "--help"]);
    let schema = text(&[
        "--format",
        "json",
        "schema",
        "account",
        "abstraction",
        "set",
    ]);

    for caution in [
        "`disabled` is Standard",
        "builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode",
        "limited to 50k actions per day",
    ] {
        assert!(help.contains(caution), "{caution}: {help}");
        assert!(schema.contains(caution), "{caution}: {schema}");
    }
}

async fn portfolio_server(raw_mode: &str) -> MockServer {
    let server = mock_info_server().await;
    for (request, response) in [
        (
            serde_json::json!({"type": "userAbstraction"}),
            serde_json::json!(raw_mode),
        ),
        (
            serde_json::json!({"type": "clearinghouseState"}),
            support::wire::perp_state_with("626469", "0", "0", "479227", vec![]),
        ),
        (
            serde_json::json!({"type": "spotClearinghouseState"}),
            serde_json::json!({"balances": [{
                "coin": "USDC", "token": 0, "hold": "147242",
                "total": "1186505", "entryNtl": "0"
            }]}),
        ),
        (
            serde_json::json!({"type": "userVaultEquities"}),
            serde_json::json!([]),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path("/info"))
            .and(body_partial_json(request))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
    }
    server
}

fn portfolio_stdout(env: &IsolatedHome, server: &MockServer, format: &str) -> String {
    let output = env
        .command_with_server(server)
        .args(["--format", format, "account", "portfolio", USER])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap()
}

#[tokio::test]
async fn account_portfolio_names_the_account_mode_and_flags_a_partial_perp_state() {
    for (raw_mode, normalized_mode, pooled) in [
        ("unifiedAccount", "unified-account", true),
        ("portfolioMargin", "portfolio-margin", true),
        ("disabled", "disabled", false),
        ("default", "default", false),
        ("dexAbstraction", "dex-abstraction", false),
    ] {
        let env = IsolatedHome::new();
        let server = portfolio_server(raw_mode).await;

        let json: Value = serde_json::from_str(&portfolio_stdout(&env, &server, "json")).unwrap();
        let summary = &json["summary"];

        assert_eq!(summary["account_mode"], normalized_mode, "{raw_mode}");
        if pooled {
            assert_eq!(summary["perp_state_is_partial"], true, "{raw_mode}");
        } else {
            assert!(summary.get("perp_state_is_partial").is_none(), "{raw_mode}");
        }
        // The perp figures are reported as the API gave them in every mode.
        assert_eq!(summary["account_value"], "626469");
        assert_eq!(summary["withdrawable"], "479227");

        let note = "Balances live in the spot balance under this mode";
        let pretty = portfolio_stdout(&env, &server, "pretty");
        assert_eq!(
            pretty.matches(note).count(),
            usize::from(pooled),
            "{raw_mode}: {pretty}"
        );
        assert!(!portfolio_stdout(&env, &server, "table").contains(note));
        assert!(
            pretty.contains("1186505"),
            "spot balances stay listed: {pretty}"
        );
    }
}
