//! A bare key that is an API wallet reads the agent's own, empty account. The
//! docs' common pitfall is answered with the master it signs for instead.

mod support;

use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::legacy_accounts_fixture::{self, Row};
use support::{
    API_OVERRIDE_ENV, IsolatedHome, VALID_PRIVATE_KEY, expected_address,
    fixture_clearinghouse_state,
};

const MASTER: &str = "0x00000000000000000000000000000000000000aa";

fn signer() -> String {
    expected_address(VALID_PRIVATE_KEY).to_lowercase()
}

/// A server where `user`'s clearinghouse state is the support fixture's.
async fn server_with_state(user: &str) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState", "user": user}),
            fixture_clearinghouse_state("900", 5),
        )
        .await;
    server
}

async fn mount_role(server: &ExchangeFixture, role: Value) {
    server
        .mount_info_containing(json!({"type": "userRole", "user": signer()}), role)
        .await;
}

fn bare_key(env: &IsolatedHome, server: &ExchangeFixture, args: &[&str]) -> assert_cmd::Command {
    let mut command = env.live_command(server);
    command.args(["--format", "json", "--testnet"]).args(args);
    command
}

#[tokio::test]
async fn a_bare_api_wallet_key_is_refused_with_its_master_by_every_signer_derived_read() {
    let env = IsolatedHome::new();
    let server = server_with_state(&signer()).await;
    mount_role(&server, json!({"role": "agent", "data": {"user": MASTER}})).await;

    for args in [
        &["positions", "list"][..],
        &["staking", "summary"],
        &["account", "orders"],
        &["api-wallet", "list"],
        &["subaccount", "list"],
        &["watch", "risk", "--max-events", "1"],
    ] {
        let output = bare_key(&env, &server, args)
            .assert()
            .code(13)
            .get_output()
            .stdout
            .clone();
        // Addresses are printed checksummed.
        let text = String::from_utf8(output).unwrap().to_lowercase();
        let agent = expected_address(VALID_PRIVATE_KEY).to_lowercase();
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

    bare_key(&env, &server, &["account", "abstraction", MASTER])
        .assert()
        .success();

    assert!(server.info_requests("userRole").await.is_empty());
}

#[tokio::test]
async fn an_ordinary_signer_reads_its_own_account_after_one_role_lookup() {
    let env = IsolatedHome::new();
    let server = server_with_state(&signer()).await;
    mount_role(&server, json!({"role": "user"})).await;

    bare_key(&env, &server, &["positions", "list"])
        .assert()
        .success();

    assert_eq!(server.info_requests("userRole").await.len(), 1);
}

#[tokio::test]
async fn an_unreadable_role_does_not_block_the_read() {
    let env = IsolatedHome::new();
    // No `userRole` answer: the mock server replies 404.
    let server = server_with_state(&signer()).await;

    bare_key(&env, &server, &["positions", "list"])
        .assert()
        .success();

    assert_eq!(server.info_requests("userRole").await.len(), 1);
}

#[tokio::test]
async fn a_stored_api_wallet_already_reads_its_master_and_asks_nothing() {
    let env = IsolatedHome::new();
    let agent = expected_address(VALID_PRIVATE_KEY);
    for db in env.accounts_db_candidates() {
        let key_file = db.parent().unwrap().join("accounts.key");
        legacy_accounts_fixture::create_accounts_db_with_key_file(
            &db,
            &key_file,
            &[Row::new("bot", &agent, VALID_PRIVATE_KEY)
                .agent_of(MASTER)
                .default_account()],
        );
    }
    let server = server_with_state(MASTER).await;

    let mut command = env.account_command_without_passphrase();
    command
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["--format", "json", "--testnet", "positions", "list"])
        .assert()
        .success();

    assert!(server.info_requests("userRole").await.is_empty());
}
