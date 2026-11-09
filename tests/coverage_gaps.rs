//! Behavior the changelog promises that no other test pinned: `subaccount list`
//! and the `report-incident` labels.

mod support;

use serde_json::json;
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout, run_json, wire};

#[tokio::test]
async fn subaccount_list_prints_the_accounts_of_a_master_address() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let master = expected_address(VALID_PRIVATE_KEY);
    let subaccount = "0x00000000000000000000000000000000000000aa";
    server
        .mount_info_containing(
            json!({"type": "subAccounts", "user": master.to_lowercase()}),
            json!([{
                "name": "market-maker-1",
                "subAccountUser": subaccount,
                "master": master,
                "clearinghouseState": wire::clearinghouse_state("900", 5),
                "spotState": {"balances": []}
            }]),
        )
        .await;

    let rows = run_json(env.command_with_server(&server).args([
        "--format",
        "json",
        "subaccount",
        "list",
        &master,
    ]));

    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["name"], "market-maker-1");
    assert_eq!(
        rows[0]["address"].as_str().unwrap().to_lowercase(),
        subaccount
    );
    assert_eq!(rows[0]["withdrawable"], "900");
    assert_eq!(rows[0]["positions_count"], 1);
    assert_eq!(server.info_requests("subAccounts").await.len(), 1);
}

#[test]
fn report_incident_labels_the_issue_bug_and_agent_incident() {
    let env = IsolatedHome::new();

    let assert = env
        .command()
        .args(["--format", "json", "report-incident", "--error", "boom"])
        .assert()
        .success();

    let url = json_stdout(&assert)["url"].as_str().unwrap().to_string();
    assert!(
        url.ends_with("&labels=bug%2Cagent-incident"),
        "the labels end the URL: {url}"
    );
}
