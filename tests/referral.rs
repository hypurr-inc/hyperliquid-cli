mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::signing::{recover_signer, test_signer};
use support::{IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY, expected_address, run_json};

async fn referral_exchange(referral_state: Value) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(json!({"type": "referral"}), referral_state)
        .await;
    server
}

/// The one request `server` received, signed by the test key as an L1 action.
async fn signed_body(server: &ExchangeFixture) -> Value {
    let mut bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    let body = bodies.remove(0);
    assert_eq!(recover_signer(&body, Chain::Testnet), test_signer());
    body
}

#[test]
fn referral_set_requires_explicit_code_on_mainnet() {
    let env = IsolatedHome::new();
    env.command()
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .env("HYPERLIQUID_NETWORK", "mainnet")
        .args(["referral", "set"])
        .assert()
        .code(13)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("referral code is required"));
}

#[tokio::test]
async fn referral_status_outputs_code_and_count() {
    let env = IsolatedHome::new();
    let server = referral_exchange(json!({
        "referredBy": {
            "referrer": "0x0000000000000000000000000000000000000001",
            "code": "REFERRED"
        },
        "cumVlm": "1000.0",
        "unclaimedRewards": "1.25",
        "claimedRewards": "2.50",
        "builderRewards": "0.10",
        "referrerState": {
            "stage": "ready",
            "data": {
                "code": "MYCODE",
                "referralStates": [
                    { "user": "0x0000000000000000000000000000000000000002" },
                    { "user": "0x0000000000000000000000000000000000000003" }
                ]
            }
        }
    }))
    .await;

    let json = run_json(env.live_json(&server).args(["referral", "status"]));

    assert_eq!(json["referral_code"], "MYCODE");
    assert_eq!(json["referral_count"], 2);
    assert_eq!(json["referred_by_code"], "REFERRED");
    assert_eq!(json["unclaimed_rewards"], "1.25");
}

#[tokio::test]
async fn referral_status_success_body_with_rate_limit_text_still_succeeds() {
    let env = IsolatedHome::new();
    let server = referral_exchange(json!({
        "referredBy": {
            "referrer": "0x0000000000000000000000000000000000000001",
            "code": "RATE-LIMIT-DOCS"
        },
        "referrerState": {
            "stage": "rate-limit-aware-success",
            "data": {
                "code": "MYCODE",
                "referralStates": []
            }
        }
    }))
    .await;

    let json = run_json(env.live_json(&server).args(["referral", "status"]));

    assert_eq!(json["referral_code"], "MYCODE");
    assert_eq!(json["referred_by_code"], "RATE-LIMIT-DOCS");
    assert_eq!(json["referral_count"], 0);
}

#[tokio::test]
async fn referral_set_testnet_defaults_to_testnet_code() {
    let env = IsolatedHome::new();
    let server = referral_exchange(json!({})).await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;

    let json = run_json(env.live_json(&server).args(["referral", "set"]));

    assert_eq!(json["signer"], expected_address(VALID_PRIVATE_KEY));
    assert_eq!(json["action"], "set-referrer");
    assert_eq!(json["code"], "TESTNET");
    assert_eq!(json["network"], "Testnet");
    assert_eq!(json["query_address"], json["signer"]);
    assert_eq!(json["reversibility"], "irreversible");

    let body = signed_body(&server).await;
    assert_eq!(
        body["action"],
        json!({"type": "setReferrer", "code": "TESTNET"})
    );
}

#[tokio::test]
async fn referral_register_submits_register_referrer_action() {
    let env = IsolatedHome::new();
    let server = referral_exchange(json!({})).await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;

    env.live_json(&server)
        .args(["referral", "register", "MYCODE"])
        .assert()
        .success()
        .stdout(predicate::str::contains("register-referrer"))
        .stdout(predicate::str::contains("MYCODE"));

    let body = signed_body(&server).await;
    assert_eq!(
        body["action"],
        json!({"type": "registerReferrer", "code": "MYCODE"})
    );
}

#[test]
fn referral_register_rejects_codes_over_twenty_chars() {
    let env = IsolatedHome::new();
    env.command()
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args([
            "referral",
            "register",
            "abcdefghijklmnopqrstuvwxyz",
            "--testnet",
        ])
        .assert()
        .code(13)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "referrer code must be 20 characters or fewer",
        ));
}

#[tokio::test]
async fn referral_register_dry_run_previews_action() {
    let env = IsolatedHome::new();
    let server = referral_exchange(json!({})).await;

    let json =
        run_json(
            env.live_json(&server)
                .args(["--dry-run", "referral", "register", "MYCODE"]),
        );

    let signer = json["signer"].as_str().unwrap().to_string();
    assert_eq!(json["command"], "referral register");
    assert_eq!(json["would_execute"], "register_referrer_code");
    assert_eq!(json["args"]["network"], "Testnet");
    assert_eq!(json["args"]["signer"], signer);
    assert_eq!(json["args"]["query_address"], signer);
    assert_eq!(json["args"]["code"], "MYCODE");
    assert_eq!(json["args"]["action"]["type"], "registerReferrer");
    assert_eq!(json["args"]["reversibility"], "irreversible");
    assert_eq!(json["acting_as"], signer);
    assert!(
        server.exchange_bodies().await.is_empty(),
        "a dry run must not reach /exchange"
    );
}

#[tokio::test]
async fn referral_set_exchange_rate_limit_exits_11() {
    let env = IsolatedHome::new();
    let server = referral_exchange(json!({})).await;
    server
        .mount_exchange_failure(429, "Too many requests: rate limit exceeded")
        .await;

    env.live_command(&server)
        .args(["referral", "set", "MYCODE", "--testnet"])
        .assert()
        .code(11)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Too many requests to the Hyperliquid API",
        ))
        .stderr(predicate::str::contains("Couldn't reach").not());
}
