//! A bare key that is an API wallet reads the agent's own, empty account. The
//! docs' common pitfall is answered with the master it signs for instead.

mod support;

use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::signing::test_signer;
use support::{IsolatedHome, VALID_PRIVATE_KEY, wire};

const MASTER: &str = "0x00000000000000000000000000000000000000aa";

/// A server where `user`'s clearinghouse state is the support fixture's.
async fn server_with_state(user: &str) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState", "user": user}),
            wire::clearinghouse_state("900", 5),
        )
        .await;
    server
}

async fn mount_role(server: &ExchangeFixture, role: Value) {
    server
        .mount_info_containing(json!({"type": "userRole", "user": test_signer()}), role)
        .await;
}

#[tokio::test]
async fn a_bare_api_wallet_key_is_refused_with_its_master_by_every_signer_derived_read() {
    let env = IsolatedHome::new();
    let server = server_with_state(&test_signer()).await;
    mount_role(&server, json!({"role": "agent", "data": {"user": MASTER}})).await;

    for args in [
        &["positions", "list"][..],
        &["staking", "summary"],
        &["account", "orders"],
        &["orders", "open"],
        &["orders", "history"],
        &["api-wallet", "list"],
        &["subaccount", "list"],
        &["watch", "risk", "--max-events", "1"],
    ] {
        let output = env
            .live_json(&server)
            .args(args)
            .assert()
            .code(13)
            .get_output()
            .stdout
            .clone();
        // Addresses are printed checksummed.
        let text = String::from_utf8(output).unwrap().to_lowercase();
        let agent = test_signer();
        assert!(
            text.contains(&format!(
                "the selected signer {agent} is an api wallet for {MASTER}"
            )),
            "{args:?}: {text}"
        );
        assert!(
            text.contains(&format!("pass {MASTER} as the account argument")),
            "{args:?}: {text}"
        );
    }
}

#[tokio::test]
async fn naming_the_master_reads_it_without_asking_for_the_role() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    mount_role(&server, json!({"role": "agent", "data": {"user": MASTER}})).await;
    server
        .mount_info_containing(
            json!({"type": "userAbstraction", "user": MASTER}),
            json!("default"),
        )
        .await;

    env.live_json(&server)
        .args(["account", "abstraction", MASTER])
        .assert()
        .success();

    assert!(server.info_requests("userRole").await.is_empty());
}

#[tokio::test]
async fn an_ordinary_signer_reads_its_own_account_after_one_role_lookup() {
    let env = IsolatedHome::new();
    let server = server_with_state(&test_signer()).await;
    mount_role(&server, json!({"role": "user"})).await;

    env.live_json(&server)
        .args(["positions", "list"])
        .assert()
        .success();

    assert_eq!(server.info_requests("userRole").await.len(), 1);
}

#[tokio::test]
async fn an_unreadable_role_does_not_block_the_read() {
    let env = IsolatedHome::new();
    // No `userRole` answer: the mock server replies 404.
    let server = server_with_state(&test_signer()).await;

    env.live_json(&server)
        .args(["positions", "list"])
        .assert()
        .success();

    assert_eq!(server.info_requests("userRole").await.len(), 1);
}

#[tokio::test]
async fn a_stored_api_wallet_already_reads_its_master_and_asks_nothing() {
    let env = IsolatedHome::new().with_stored_account("bot", VALID_PRIVATE_KEY, Some(MASTER));
    let server = server_with_state(MASTER).await;

    env.stored_json(&server)
        .args(["positions", "list"])
        .assert()
        .success();

    assert!(server.info_requests("userRole").await.is_empty());
}
