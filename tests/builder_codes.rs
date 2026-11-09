//! `builder max-fee`, `builder approved` and `builder approve`: reading an
//! account's approvals and signing a new one.

mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::json;
use support::exchange::ExchangeFixture;
use support::orders::words;
use support::signing::{recover_signer, test_signer};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, run_json};

const USER: &str = "0x00000000000000000000000000000000000000aa";
const BUILDER: &str = "0x00000000000000000000000000000000000000bb";
const SECOND_BUILDER: &str = "0x00000000000000000000000000000000000000cc";

async fn mount_max_fee(server: &ExchangeFixture, builder: &str, tenths_bps: u64) {
    server
        .mount_info(
            json!({"type": "maxBuilderFee", "user": USER, "builder": builder}),
            json!(tenths_bps),
        )
        .await;
}

#[tokio::test]
async fn max_fee_reads_tenths_of_a_basis_point_and_a_percentage() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    mount_max_fee(&server, BUILDER, 10).await;

    let fee = run_json(env.command_with_server(&server).args(words(&format!(
        "--format json builder max-fee --user {USER} --builder {BUILDER}"
    ))));

    assert_eq!(fee["user"].as_str().unwrap().to_ascii_lowercase(), USER);
    assert_eq!(
        fee["builder"].as_str().unwrap().to_ascii_lowercase(),
        BUILDER
    );
    assert_eq!(fee["max_fee_tenths_bps"], 10);
    assert_eq!(fee["max_fee_rate"], "0.01%");
}

#[tokio::test]
async fn approved_lists_each_builder_with_its_fee_cap() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "approvedBuilders", "user": USER}),
            json!([BUILDER, SECOND_BUILDER]),
        )
        .await;
    mount_max_fee(&server, BUILDER, 10).await;
    mount_max_fee(&server, SECOND_BUILDER, 1).await;

    let rows = run_json(env.command_with_server(&server).args(words(&format!(
        "--format json builder approved --user {USER}"
    ))));

    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["user"].as_str().unwrap().to_ascii_lowercase(), USER);
    assert_eq!(
        rows[0]["builder"].as_str().unwrap().to_ascii_lowercase(),
        BUILDER
    );
    assert_eq!(rows[0]["max_fee_tenths_bps"], 10);
    assert_eq!(rows[0]["max_fee_rate"], "0.01%");
    assert_eq!(
        rows[1]["builder"].as_str().unwrap().to_ascii_lowercase(),
        SECOND_BUILDER
    );
    assert_eq!(rows[1]["max_fee_tenths_bps"], 1);
    assert_eq!(rows[1]["max_fee_rate"], "0.001%");
}

#[tokio::test]
async fn approved_with_no_builders_is_an_empty_json_array() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "approvedBuilders", "user": USER}), json!([]))
        .await;

    let rows = run_json(env.command_with_server(&server).args(words(&format!(
        "--format json builder approved --user {USER}"
    ))));

    assert_eq!(rows, json!([]));
}

#[test]
fn approved_refuses_a_bad_user_before_any_request() {
    IsolatedHome::new()
        .command()
        .args(["builder", "approved", "--user", "not-an-address"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "was not found as an address, alias, or id",
        ));
}

#[tokio::test]
async fn approve_dry_run_previews_the_action() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let signer = expected_address(VALID_PRIVATE_KEY);

    let preview = run_json(env.live_command(&server).args(words(&format!(
        "--format json --dry-run builder approve --builder {BUILDER} --max-fee-rate 0.001%"
    ))));

    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["command"], "builder approve");
    assert_eq!(preview["would_execute"], "approve_builder_fee");
    assert_eq!(preview["args"]["network"], "Mainnet");
    assert_eq!(preview["args"]["signer"], signer);
    assert_eq!(preview["args"]["query_address"], signer);
    assert_eq!(preview["args"]["max_fee_rate"], "0.001%");
    assert_eq!(preview["args"]["max_fee_tenths_bps"], 1);
    assert_eq!(preview["args"]["reversibility"], "reversible");
    assert_eq!(preview["args"]["action"]["type"], "approveBuilderFee");
    assert_eq!(preview["args"]["action"]["maxFeeRate"], "0.001%");
    assert_eq!(
        preview["args"]["action"]["builder"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
        BUILDER
    );
    assert_eq!(preview["signer"], signer);
    assert_eq!(preview["acting_as"], signer);
    assert!(preview["vault_address"].is_null());
}

#[test]
fn approve_refuses_a_fee_that_is_not_a_multiple_of_the_unit_before_auth() {
    IsolatedHome::new()
        .command()
        .args(words(&format!(
            "builder approve --builder {BUILDER} --max-fee-rate 0.0001%"
        )))
        .assert()
        .code(13)
        .stderr(predicate::str::contains("multiple of 0.001%"));
}

#[tokio::test]
async fn approve_submits_a_payload_signed_by_the_key_with_yes() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    let user = expected_address(VALID_PRIVATE_KEY);

    let receipt = run_json(
        env.command_with_server(&server)
            .args(["--format", "json", "--private-key", VALID_PRIVATE_KEY])
            .args(words(&format!(
                "builder approve --builder {BUILDER} --max-fee-rate 0.001% -y"
            ))),
    );

    assert_eq!(receipt["status"], "submitted");
    assert_eq!(receipt["signer"], user);
    assert_eq!(receipt["query_address"], user);
    assert_eq!(receipt["max_fee_tenths_bps"], 1);
    assert_eq!(receipt["network"], "Mainnet");
    assert_eq!(receipt["reversibility"], "reversible");

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert_eq!(body["action"]["type"], "approveBuilderFee");
    assert_eq!(body["action"]["maxFeeRate"], "0.001%");
    assert_eq!(
        body["action"]["builder"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
        BUILDER
    );
    assert_eq!(body["action"]["nonce"], body["nonce"]);
    assert_eq!(recover_signer(body, Chain::Mainnet), test_signer());
}
