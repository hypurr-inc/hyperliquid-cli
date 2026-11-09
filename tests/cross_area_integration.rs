mod support;

use predicates::prelude::*;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use support::{
    API_OVERRIDE_ENV, IsolatedHome, MAINNET_API_OVERRIDE_ENV, TEST_ACCOUNT_PASSPHRASE,
    TESTNET_API_OVERRIDE_ENV, fixture_basic_order, fixture_fill, fixture_order_success_response,
    fixture_perp_meta_btc_eth, mount_account_state, mount_common_public_endpoints,
    mount_market_meta, mount_override_healthcheck, mount_perp_dexs, mount_spot_meta,
    mount_successful_exchange_actions,
};

const IMPORT_PRIVATE_KEY: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000006";
const USER_ADDRESS: &str = "0x0000000000000000000000000000000000000001";
const VALIDATOR_ADDRESS: &str = "0x0000000000000000000000000000000000000002";

async fn mount_borrowlend_and_staking_public(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({
            "type": "allBorrowLendReserveStates"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            [
                0,
                {
                    "borrowYearlyRate": "0.05",
                    "supplyYearlyRate": "0.0142",
                    "balance": "1000",
                    "utilization": "0.2",
                    "oraclePx": "1.0",
                    "ltv": "0.0",
                    "totalSupplied": "1000",
                    "totalBorrowed": "200"
                }
            ]
        ])))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "delegatorSummary"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "delegated": "123.45",
            "undelegated": "10",
            "totalPendingWithdrawal": "5.5",
            "nPendingWithdrawals": 2
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "delegations"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "validator": VALIDATOR_ADDRESS,
                "amount": "100.0",
                "lockedUntilTimestamp": 1700000000000_u64
            }
        ])))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "delegatorRewards"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "time": 1700000000000_u64,
                "source": "staking",
                "totalAmount": "1.25"
            }
        ])))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "validatorSummaries"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "validator": VALIDATOR_ADDRESS,
                "signer": "0x0000000000000000000000000000000000000003",
                "name": "Validator One",
                "description": "test validator",
                "nRecentBlocks": 1,
                "stake": 123456789_u64,
                "isJailed": false,
                "unjailableAfter": null,
                "isActive": true,
                "commission": "0.04",
                "stats": [
                    ["day", {"uptimeFraction": "1.0", "predictedApr": "0.025", "nSamples": 1440}]
                ]
            }
        ])))
        .mount(server)
        .await;
}

async fn mock_cross_area_server(order_response: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    mount_common_public_endpoints(&server).await;
    mount_account_state(
        &server,
        vec![fixture_basic_order("BTC", 4242)],
        vec![fixture_fill("BTC", 4242)],
        "1000",
        "1000",
        5,
    )
    .await;
    mount_borrowlend_and_staking_public(&server).await;
    mount_successful_exchange_actions(&server, order_response).await;

    server
}

#[tokio::test]
async fn first_visit_setup_then_query_and_first_order_succeed() {
    let env = IsolatedHome::new();
    let server = mock_cross_area_server(fixture_order_success_response(4242)).await;

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .arg("setup")
        .write_stdin(format!("2\ny\n\n\n{IMPORT_PRIVATE_KEY}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("Test query succeeded"))
        .stdout(predicate::str::contains("Setup complete"));

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["perps", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stderr(predicate::str::contains("Completed in").not());

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args([
            "orders", "create", "--coin", "BTC", "--side", "buy", "--price", "50000", "--size",
            "0.1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("4242"))
        .stdout(predicate::str::contains("resting"));
}

#[tokio::test]
async fn unauthenticated_read_only_commands_work_and_auth_commands_exit_10() {
    let env = IsolatedHome::new();
    let server = mock_cross_area_server(fixture_order_success_response(4242)).await;

    for args in [
        vec!["perps", "list"],
        vec!["spot", "list"],
        vec!["mids"],
        vec!["account", "fills", USER_ADDRESS],
        vec!["account", "orders", USER_ADDRESS],
        vec!["account", "portfolio", USER_ADDRESS],
        vec!["status"],
        vec!["borrowlend", "rates"],
        vec!["staking", "validators"],
    ] {
        env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
            .args(args)
            .assert()
            .success();
    }

    for args in [
        vec![
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--price",
            "50000",
            "--size",
            "0.1",
            "--testnet",
        ],
        vec!["orders", "cancel", "12345", "--testnet"],
        vec!["orders", "cancel-all", "-y", "--testnet"],
        vec![
            "positions",
            "update-leverage",
            "--coin",
            "BTC",
            "--leverage",
            "5",
            "--testnet",
        ],
        vec!["transfer", "spot-to-perp", "--amount", "1", "--testnet"],
        vec![
            "staking",
            "delegate",
            "--validator",
            VALIDATOR_ADDRESS,
            "--amount",
            "1",
            "--testnet",
        ],
    ] {
        env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
            .args(args)
            .assert()
            .code(10)
            .stderr(predicate::str::contains("hyperliquid setup"));
    }

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["wallet", "reset"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing to reset"));
}

#[tokio::test]
async fn stored_account_aliases_resolve_for_public_user_lookup_commands() {
    let env = IsolatedHome::new();
    let server = mock_cross_area_server(fixture_order_success_response(4242)).await;

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args([
            "account",
            "add",
            IMPORT_PRIVATE_KEY,
            "--alias",
            "main",
            "--type",
            "main-wallet",
            "--default",
        ])
        .assert()
        .success();

    for args in [
        vec!["account", "portfolio", "main", "--testnet"],
        vec!["borrowlend", "user", "main", "--testnet"],
    ] {
        env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("USDC"))
            .stdout(predicate::str::contains("1000"));
    }

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["account", "fills", "main", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stdout(predicate::str::contains("4242"));

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["account", "orders", "main", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stdout(predicate::str::contains("4242"));

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["account", "subaccounts", "main", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No subaccounts.").not())
        .stderr(predicate::str::contains("No subaccounts."));

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["staking", "summary", "main", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("123.45"))
        .stdout(predicate::str::contains("1.25"));

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["staking", "rewards", "main", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("staking"))
        .stdout(predicate::str::contains("1.25"));

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args(["--account", "main", "orders", "open", "--testnet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stdout(predicate::str::contains("4242"));

    env.account_command_with_server(TEST_ACCOUNT_PASSPHRASE, &server)
        .args([
            "--account",
            "main",
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--price",
            "50000",
            "--size",
            "0.1",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("4242"));
}

#[tokio::test]
async fn market_lists_expose_sizes_and_price_ticks_for_agents() {
    let env = IsolatedHome::new();
    let server = MockServer::start().await;
    mount_override_healthcheck(&server).await;
    mount_market_meta(&server, fixture_perp_meta_btc_eth()).await;
    mount_perp_dexs(&server, serde_json::json!([])).await;
    mount_spot_meta(
        &server,
        serde_json::json!({
            "universe": [{"name": "PURR/USDC", "index": 0, "tokens": [1, 0]}],
            "tokens": [
                {"name": "USDC", "index": 0, "tokenId": "0x00000000000000000000000000000000", "szDecimals": 6, "weiDecimals": 6, "evmContract": null},
                {"name": "PURR", "index": 1, "tokenId": "0x00000000000000000000000000000001", "szDecimals": 0, "weiDecimals": 5, "evmContract": null}
            ]
        }),
    )
    .await;

    let run = |args: &[&str]| -> serde_json::Value {
        let output = env
            .command_with_server(&server)
            .args(["--format", "json"])
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    };

    let tick = |market: &serde_json::Value| -> f64 {
        market["tick_size_at_price_1"]
            .as_str()
            .expect("tick size is a decimal string")
            .parse()
            .unwrap()
    };
    let perps = run(&["perps", "list"]);
    assert_eq!(perps[0]["name"], "BTC");
    assert_eq!(perps[0]["index"], 0);
    assert_eq!(perps[0]["max_leverage"], 50);
    assert_eq!(perps[0]["sz_decimals"], 5);
    assert_eq!(perps[0]["collateral"], "USDC");
    assert_eq!(tick(&perps[0]), 0.1);
    assert_eq!(perps[1]["name"], "ETH");
    assert_eq!(tick(&perps[1]), 0.01);
    assert_eq!(run(&["perps", "get", "ETH"]), perps[1]);

    let spot = run(&["spot", "list"]);
    assert_eq!(spot[0]["symbol"], "PURR/USDC");
    assert_eq!(spot[0]["index"], 10_000);
    assert_eq!(spot[0]["base"], "PURR");
    assert_eq!(spot[0]["quote"], "USDC");
    assert_eq!(spot[0]["base_sz_decimals"], 0);
    assert_eq!(spot[0]["quote_sz_decimals"], 6);
    assert_eq!(tick(&spot[0]), 0.0001);
    assert_eq!(run(&["spot", "get", "PURR/USDC"]), spot[0]);
}

#[tokio::test]
async fn testnet_mode_uses_testnet_endpoint_override() {
    let env = IsolatedHome::new();
    let mainnet = MockServer::start().await;
    let testnet = mock_cross_area_server(fixture_order_success_response(4242)).await;

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .env(MAINNET_API_OVERRIDE_ENV, mainnet.uri())
        .env(TESTNET_API_OVERRIDE_ENV, testnet.uri())
        .args(["--format", "json", "--testnet", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("testnet"))
        .stdout(predicate::str::contains(testnet.uri()));
}

#[test]
fn network_and_auth_errors_include_recovery_suggestions() {
    let env = IsolatedHome::new();

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .env(API_OVERRIDE_ENV, "http://127.0.0.1:9")
        .args(["perps", "list"])
        .assert()
        .code(12)
        .stderr(predicate::str::contains("127.0.0.1:9"));

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["orders", "open"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("Authentication required"))
        .stderr(predicate::str::contains("hyperliquid setup"));
}
