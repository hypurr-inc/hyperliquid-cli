//! What every signed request carries: a nonce that never repeats across the
//! processes of one machine, an `expiresAfter` that the signature covers, and
//! the two actions built on them, `account invalidate-nonce` (noop) and
//! `account reserve-weight`.

mod support;

use assert_cmd::Command;
use hyperliquid_cli::env;
use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::{Value, json};
use std::process::Stdio;
use support::exchange::ExchangeFixture;
use support::signing::{recover_signer, test_signer};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout, unix_millis};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const OK: fn() -> Value = || json!({"status": "ok", "response": {"type": "default"}});
const DESTINATION: &str = "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01";

async fn exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server.mount_exchange(OK()).await;
    server
}

fn signed(env: &IsolatedHome, server: &ExchangeFixture, args: &[&str]) -> Command {
    let mut command = env.live_json(server);
    command.args(args);
    command
}

fn nonces(bodies: &[Value]) -> Vec<u64> {
    bodies
        .iter()
        .map(|body| body["nonce"].as_u64().expect("nonce"))
        .collect()
}

fn mark_file(env: &IsolatedHome) -> std::path::PathBuf {
    env.data_dir().join("nonce-high-water")
}

fn write_mark(env: &IsolatedHome, mark: u64) {
    std::fs::create_dir_all(env.data_dir()).unwrap();
    std::fs::write(mark_file(env), mark.to_string()).unwrap();
}

fn read_mark(env: &IsolatedHome) -> u64 {
    std::fs::read_to_string(mark_file(env))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

const RESERVE_ONE: [&str; 5] = ["account", "reserve-weight", "--weight", "1", "--yes"];

// ---- nonces -------------------------------------------------------------

#[tokio::test]
async fn concurrent_processes_never_sign_with_the_same_nonce() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let children: Vec<_> = (0..24)
        .map(|_| {
            let command = signed(&env, &server, &RESERVE_ONE);
            let mut child = support::std_command(&command);
            child.stdout(Stdio::null()).stderr(Stdio::null());
            child.spawn().unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    let seen = nonces(&server.exchange_bodies().await);
    assert_eq!(seen.len(), 24);
    let distinct: std::collections::HashSet<_> = seen.iter().collect();
    assert_eq!(distinct.len(), seen.len(), "repeated nonce in {seen:?}");
    assert_eq!(read_mark(&env), *seen.iter().max().unwrap());
}

#[tokio::test]
async fn a_recorded_mark_ahead_of_the_clock_is_respected() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let mark = unix_millis() + 60 * 60 * 1000;
    write_mark(&env, mark);

    signed(&env, &server, &RESERVE_ONE).assert().success();

    assert_eq!(nonces(&server.exchange_bodies().await), vec![mark + 1]);
    assert_eq!(read_mark(&env), mark + 1);
}

#[tokio::test]
async fn a_mark_more_than_twelve_hours_ahead_is_ignored() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    write_mark(&env, unix_millis() + 20 * 60 * 60 * 1000);

    let before = unix_millis();
    signed(&env, &server, &RESERVE_ONE).assert().success();
    let after = unix_millis();

    let nonce = nonces(&server.exchange_bodies().await)[0];
    assert!((before..=after + 1).contains(&nonce), "{nonce}");
    assert_eq!(read_mark(&env), nonce);
}

#[cfg(unix)]
#[tokio::test]
async fn the_mark_and_its_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let env = IsolatedHome::new();
    let server = exchange().await;

    signed(&env, &server, &RESERVE_ONE).assert().success();

    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&mark_file(&env)), 0o600);
    assert_eq!(mode(&env.data_dir()), 0o700);
}

const DRY_RUN_APPROVALS: [&[&str]; 3] = [
    &["api-wallet", "create", "--dry-run"],
    &["api-wallet", "approve", "--generate", "--dry-run"],
    &["api-wallet", "revoke", "--unnamed", "--dry-run"],
];

#[tokio::test]
async fn a_dry_run_approval_consumes_no_nonce_and_writes_no_mark() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for args in DRY_RUN_APPROVALS {
        signed(&env, &server, args).assert().success();
    }

    assert!(
        !mark_file(&env).exists(),
        "a preview must not create the nonce mark"
    );
    assert!(server.exchange_bodies().await.is_empty());

    let mark = unix_millis() + 60 * 60 * 1000;
    write_mark(&env, mark);
    for args in DRY_RUN_APPROVALS {
        signed(&env, &server, args).assert().success();
    }
    assert_eq!(read_mark(&env), mark, "a preview must not raise the mark");
}

/// Commands that sign through the sender (`send_l1_action`,
/// `send_raw_l1_json_action`) or a user-signed document: each allocates its
/// nonce after the preview returns.
const DRY_RUN_SENDS: [&[&str]; 6] = [
    &["account", "reserve-weight", "--weight", "1", "--dry-run"],
    &["account", "evm-big-blocks", "--enable", "--dry-run"],
    &["subaccount", "create", "--name", "desk", "--dry-run"],
    &["staking", "claim-rewards", "--dry-run"],
    &["staking", "deposit", "--amount", "1", "--dry-run"],
    &[
        "transfer",
        "send",
        "--to",
        DESTINATION,
        "--amount",
        "1",
        "--dry-run",
    ],
];

#[tokio::test]
async fn a_dry_run_of_a_sending_command_consumes_no_nonce_and_writes_no_mark() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for args in DRY_RUN_SENDS {
        signed(&env, &server, args).assert().success();
    }

    assert!(
        !mark_file(&env).exists(),
        "a preview must not create the nonce mark"
    );
    assert!(server.exchange_bodies().await.is_empty());

    let mark = unix_millis() + 60 * 60 * 1000;
    write_mark(&env, mark);
    for args in DRY_RUN_SENDS {
        signed(&env, &server, args).assert().success();
    }
    assert_eq!(read_mark(&env), mark, "a preview must not raise the mark");
}

#[tokio::test]
async fn an_unwritable_data_directory_still_signs_and_says_so() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let blocker = env.tmp_path().join("blocker");
    std::fs::write(&blocker, b"").unwrap();

    signed(&env, &server, &RESERVE_ONE)
        .env(env::DATA_DIR, blocker.join("hyperliquid"))
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Nonce high-water mark not recorded",
        ));

    assert_eq!(server.exchange_bodies().await.len(), 1);
}

// ---- expiresAfter -------------------------------------------------------

#[tokio::test]
async fn expires_after_is_hashed_and_sent_for_hypersdk_actions() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let before = unix_millis();
    signed(&env, &server, &RESERVE_ONE)
        .args(["--expires-after", "30s"])
        .assert()
        .success();
    let after = unix_millis();

    let body = &server.exchange_bodies().await[0];
    let expires = body["expiresAfter"].as_u64().expect("expiresAfter");
    assert!(
        (before + 30_000..=after + 30_000).contains(&expires),
        "{expires}"
    );
    assert_eq!(recover_signer(body, Chain::Testnet), test_signer());
}

#[tokio::test]
async fn expires_after_is_hashed_and_sent_for_raw_l1_actions() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let before = unix_millis();
    signed(&env, &server, &["subaccount", "create", "--name", "desk"])
        .args(["--expires-after", "5m"])
        .assert()
        .success();
    let after = unix_millis();

    let body = &server.exchange_bodies().await[0];
    assert_eq!(body["action"]["type"], "createSubAccount");
    let expires = body["expiresAfter"].as_u64().expect("expiresAfter");
    assert!(
        (before + 300_000..=after + 300_000).contains(&expires),
        "{expires}"
    );
    assert_eq!(recover_signer(body, Chain::Testnet), test_signer());
}

#[tokio::test]
async fn without_expires_after_the_wire_is_unchanged() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    signed(&env, &server, &RESERVE_ONE).assert().success();
    signed(&env, &server, &["subaccount", "create", "--name", "desk"])
        .assert()
        .success();

    for body in server.exchange_bodies().await {
        assert!(body["expiresAfter"].is_null(), "{body}");
        assert!(body.as_object().unwrap().contains_key("expiresAfter"));
        assert_eq!(recover_signer(&body, Chain::Testnet), test_signer());
    }
}

#[tokio::test]
async fn expires_after_is_refused_for_user_signed_commands_even_in_a_preview() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for preview in [false, true] {
        let mut command = signed(
            &env,
            &server,
            &[
                "transfer",
                "send",
                "--to",
                "0x0000000000000000000000000000000000000001",
                "--amount",
                "1",
                "--yes",
                "--expires-after",
                "30s",
            ],
        );
        if preview {
            command.arg("--dry-run");
        }
        command
            .assert()
            .code(13)
            .stdout(predicate::str::contains("L1 actions only"));
    }
    assert!(server.exchange_bodies().await.is_empty());
}

#[tokio::test]
async fn expires_after_is_refused_for_commands_that_sign_nothing() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    env.command_with_server(&server)
        .args(["--format", "json", "--expires-after", "30s", "mids"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("signs nothing"));
}

#[test]
fn expires_after_must_be_a_positive_duration() {
    let env = IsolatedHome::new();
    for bad in ["0s", "soon", "-5s", "1.5m"] {
        env.command()
            .args([
                "--expires-after",
                bad,
                "--dry-run",
                "account",
                "reserve-weight",
                "--weight",
                "1",
            ])
            .assert()
            .code(2);
    }
}

#[tokio::test]
async fn a_preview_shows_the_computed_expiry() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let before = unix_millis();
    let json = json_stdout(
        &signed(
            &env,
            &server,
            &[
                "--dry-run",
                "--expires-after",
                "30s",
                "account",
                "reserve-weight",
                "--weight",
                "10",
            ],
        )
        .assert()
        .success(),
    );
    let after = unix_millis();

    let expires = json["expires_after"].as_u64().expect("expires_after");
    assert!(
        (before + 30_000..=after + 30_000).contains(&expires),
        "{json}"
    );

    let without = json_stdout(
        &signed(
            &env,
            &server,
            &["--dry-run", "account", "reserve-weight", "--weight", "10"],
        )
        .assert()
        .success(),
    );
    assert!(without.get("expires_after").is_none(), "{without}");
    assert!(server.exchange_bodies().await.is_empty());
}

#[tokio::test]
async fn an_unknown_outcome_names_the_nonce_and_expiry_to_burn() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let output = signed(&env, &server, &RESERVE_ONE)
        .args(["--expires-after", "1m"])
        .assert()
        .code(12)
        .get_output()
        .stdout
        .clone();

    let body = &server.exchange_bodies().await[0];
    let (nonce, expires) = (
        body["nonce"].as_u64().unwrap(),
        body["expiresAfter"].as_u64().unwrap(),
    );
    let error = support::stdout_json(&output)["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        error.contains(&format!(
            "signed with nonce {nonce}, expiring at {expires}."
        )),
        "{error}"
    );
    assert!(
        error.contains(&format!(
            "hyperliquid --testnet account invalidate-nonce --nonce {nonce}"
        )),
        "{error}"
    );
}

#[tokio::test]
async fn an_unknown_outcome_without_an_expiry_names_only_the_nonce() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let output = signed(&env, &server, &RESERVE_ONE)
        .assert()
        .code(12)
        .get_output()
        .stdout
        .clone();

    let nonce = server.exchange_bodies().await[0]["nonce"].as_u64().unwrap();
    let error = support::stdout_json(&output)["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        error.contains(&format!("signed with nonce {nonce}. ")),
        "{error}"
    );
    assert!(!error.contains("expiring"), "{error}");
}

// ---- account invalidate-nonce -------------------------------------------

#[tokio::test]
async fn invalidate_nonce_sends_a_signed_noop_carrying_the_nonce_to_burn() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let nonce = unix_millis() - 5_000;

    // No --testnet and no --yes: burning a nonce moves nothing and asks nothing.
    let mut command = env.live_command(&server);
    let json = json_stdout(
        &command
            .args([
                "--format",
                "json",
                "account",
                "invalidate-nonce",
                "--nonce",
                &nonce.to_string(),
            ])
            .assert()
            .success(),
    );

    assert_eq!(json["nonce"], nonce);
    assert_eq!(json["status"], "confirmed");
    let body = &server.exchange_bodies().await[0];
    assert_eq!(body["action"], json!({"type": "noop"}));
    assert_eq!(body["nonce"], nonce);
    assert!(body["vaultAddress"].is_null());
    assert!(body["expiresAfter"].is_null());
    assert_eq!(recover_signer(body, Chain::Mainnet), test_signer());
}

#[tokio::test]
async fn invalidate_nonce_honors_expires_after() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let nonce = unix_millis() - 5_000;

    signed(
        &env,
        &server,
        &["account", "invalidate-nonce", "--nonce", &nonce.to_string()],
    )
    .args(["--expires-after", "2m"])
    .assert()
    .success();

    let body = &server.exchange_bodies().await[0];
    assert!(body["expiresAfter"].as_u64().unwrap() > unix_millis());
    assert_eq!(recover_signer(body, Chain::Testnet), test_signer());
}

#[tokio::test]
async fn invalidate_nonce_previews_without_sending() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let nonce = unix_millis() - 5_000;

    let json = json_stdout(
        &signed(
            &env,
            &server,
            &[
                "--dry-run",
                "account",
                "invalidate-nonce",
                "--nonce",
                &nonce.to_string(),
            ],
        )
        .assert()
        .success(),
    );

    assert_eq!(json["command"], "account invalidate-nonce");
    assert_eq!(json["would_execute"], "invalidate_nonce");
    assert_eq!(json["args"]["nonce"], nonce);
    assert_eq!(json["signer"], expected_address(VALID_PRIVATE_KEY));
    assert!(server.exchange_bodies().await.is_empty());
}

#[tokio::test]
async fn invalidate_nonce_refuses_a_nonce_the_exchange_could_not_hold() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let too_old = unix_millis() - 3 * 24 * 60 * 60 * 1000;
    let too_new = unix_millis() + 2 * 24 * 60 * 60 * 1000;

    for nonce in [1, too_old, too_new] {
        for preview in [false, true] {
            let mut command = signed(
                &env,
                &server,
                &["account", "invalidate-nonce", "--nonce", &nonce.to_string()],
            );
            if preview {
                command.arg("--dry-run");
            }
            command
                .assert()
                .code(13)
                .stdout(predicate::str::contains("outside the window"));
        }
    }
    signed(
        &env,
        &server,
        &["account", "invalidate-nonce", "--nonce", "0"],
    )
    .assert()
    .code(2);
    assert!(server.exchange_bodies().await.is_empty());
}

// ---- what an ok reply must be ---------------------------------------------

/// An ok reply whose `type` is not `default`.
fn cancel_reply() -> Value {
    json!({"status": "ok", "response": {"type": "cancel", "data": {"statuses": ["success"]}}})
}

#[tokio::test]
async fn an_action_with_nothing_to_report_refuses_an_ok_reply_that_is_not_default() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server.mount_exchange(cancel_reply()).await;

    signed(&env, &server, &["account", "evm-big-blocks", "--enable"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "exchange action: unexpected response type: [untrusted remote data] Ok(Cancel { statuses: [Success] })",
        ));
    assert_eq!(server.exchange_bodies().await.len(), 1);
}

#[tokio::test]
async fn an_action_that_returns_its_payload_does_not_check_the_reply_type() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server.mount_exchange(cancel_reply()).await;

    signed(&env, &server, &RESERVE_ONE).assert().success();
    assert_eq!(server.exchange_bodies().await.len(), 1);
}

// ---- account reserve-weight ---------------------------------------------

#[tokio::test]
async fn reserve_weight_signs_the_l1_action_for_the_signer() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let json = json_stdout(
        &signed(
            &env,
            &server,
            &["account", "reserve-weight", "--weight", "10", "--yes"],
        )
        .assert()
        .success(),
    );

    assert_eq!(
        json,
        json!({
            "action": "reserve_request_weight",
            "status": "confirmed",
            "weight": 10,
            "cost_usdc": "0.0050",
            "destination": null
        })
    );
    let body = &server.exchange_bodies().await[0];
    assert_eq!(
        body["action"],
        json!({"type": "reserveRequestWeight", "weight": 10})
    );
    assert!(body["vaultAddress"].is_null());
    assert_eq!(recover_signer(body, Chain::Testnet), test_signer());
}

#[tokio::test]
async fn reserve_weight_hashes_the_destination_as_lowercase_hex() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    signed(
        &env,
        &server,
        &[
            "account",
            "reserve-weight",
            "--weight",
            "3",
            "--destination",
            DESTINATION,
            "--yes",
        ],
    )
    .assert()
    .success();

    let body = &server.exchange_bodies().await[0];
    assert_eq!(
        body["action"],
        json!({
            "type": "reserveRequestWeight",
            "weight": 3,
            "destination": DESTINATION.to_lowercase()
        })
    );
    assert_eq!(recover_signer(body, Chain::Testnet), test_signer());
}

#[tokio::test]
async fn reserve_weight_previews_its_cost_without_sending() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let json = json_stdout(
        &signed(
            &env,
            &server,
            &[
                "--dry-run",
                "account",
                "reserve-weight",
                "--weight",
                "1000",
                "--destination",
                DESTINATION,
            ],
        )
        .assert()
        .success(),
    );

    assert_eq!(json["would_execute"], "reserve_request_weight");
    assert_eq!(json["args"]["weight"], 1000);
    assert_eq!(json["args"]["cost_usdc"], "0.5000");
    assert_eq!(
        json["args"]["destination"].as_str().unwrap().to_lowercase(),
        DESTINATION.to_lowercase()
    );
    assert!(server.exchange_bodies().await.is_empty());
}

#[tokio::test]
async fn reserve_weight_spends_usdc_so_it_needs_consent() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for network in [Some("--testnet"), None] {
        let mut command = env.live_command(&server);
        command.args(["--format", "json"]);
        command.args(network);
        command
            .args(["account", "reserve-weight", "--weight", "10"])
            .assert()
            .code(13)
            .stdout(predicate::str::contains(
                "without confirmation in non-interactive mode",
            ));
    }
    assert!(server.exchange_bodies().await.is_empty());
}

#[tokio::test]
async fn reserve_weight_refuses_malformed_input() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for args in [
        vec!["--weight", "0"],
        vec!["--weight", "4294967296"],
        vec!["--weight", "-1"],
        vec!["--weight", "1", "--destination", "0x1234"],
        vec![
            "--weight",
            "1",
            "--destination",
            "0x0000000000000000000000000000000000000000",
        ],
    ] {
        signed(&env, &server, &["account", "reserve-weight", "--yes"])
            .args(&args)
            .assert()
            .code(2);
    }
    assert!(server.exchange_bodies().await.is_empty());
}
