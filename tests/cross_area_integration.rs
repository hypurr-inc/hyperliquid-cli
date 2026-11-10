mod support;

use predicates::prelude::*;
use serde_json::json;
use support::exchange::ExchangeFixture;
use support::reads::read_json;
use support::{
    API_OVERRIDE_ENV, IsolatedHome, MAINNET_API_OVERRIDE_ENV, TEST_ACCOUNT_PASSPHRASE,
    TESTNET_API_OVERRIDE_ENV, mount_account_state, mount_successful_exchange_actions, wire,
};

const IMPORT_PRIVATE_KEY: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000006";
const USER_ADDRESS: &str = "0x0000000000000000000000000000000000000001";
const VALIDATOR_ADDRESS: &str = "0x0000000000000000000000000000000000000002";

async fn mount_borrowlend_and_staking_public(server: &ExchangeFixture) {
    server
        .mount_info(
            json!({"type": "allBorrowLendReserveStates"}),
            json!([[0, {
                "borrowYearlyRate": "0.05",
                "supplyYearlyRate": "0.0142",
                "balance": "1000",
                "utilization": "0.2",
                "oraclePx": "1.0",
                "ltv": "0.0",
                "totalSupplied": "1000",
                "totalBorrowed": "200"
            }]]),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "borrowLendUserState"}),
            json!({
                "tokenToState": [[0, {
                    "borrow": {"basis": "0.0", "value": "0.0"},
                    "supply": {"basis": "1000.0", "value": "1000.0"}
                }]],
                "health": "healthy",
                "healthFactor": null
            }),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "delegatorSummary"}),
            json!({
                "delegated": "123.45",
                "undelegated": "10",
                "totalPendingWithdrawal": "5.5",
                "nPendingWithdrawals": 2
            }),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "delegations"}),
            json!([{
                "validator": VALIDATOR_ADDRESS,
                "amount": "100.0",
                "lockedUntilTimestamp": 1700000000000_u64
            }]),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "delegatorRewards"}),
            json!([{"time": 1700000000000_u64, "source": "staking", "totalAmount": "1.25"}]),
        )
        .await;
    server
        .mount_info(
            json!({"type": "validatorSummaries"}),
            json!([wire::validator(VALIDATOR_ADDRESS, "Validator One")]),
        )
        .await;
}

async fn mock_cross_area_server(order_response: serde_json::Value) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    mount_account_state(
        &server,
        vec![wire::basic_order("BTC", 4242)],
        vec![wire::fill("BTC", 4242)],
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
    let server = mock_cross_area_server(wire::order_success_response(4242)).await;

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
    let server = mock_cross_area_server(wire::order_success_response(4242)).await;

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
    let server = mock_cross_area_server(wire::order_success_response(4242)).await;

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
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "meta"}),
            wire::perp_meta(vec![
                wire::perp_with("BTC", 5, 50, wire::MarginRule::Either),
                wire::perp_with("kPEPE", 0, 10, wire::MarginRule::Either),
            ]),
        )
        .await;
    let spot_meta = json!({
        "universe": [{"name": "PURR/USDC", "index": 0, "tokens": [1, 0]}],
        "tokens": [
            {"name": "USDC", "index": 0, "tokenId": "0x00000000000000000000000000000000", "szDecimals": 6, "weiDecimals": 6, "evmContract": null},
            {"name": "PURR", "index": 1, "tokenId": "0x00000000000000000000000000000001", "szDecimals": 0, "weiDecimals": 5, "evmContract": null}
        ]
    });
    server
        .override_info(json!({"type": "spotMeta"}), spot_meta.clone())
        .await;
    server
        .mount_info(
            json!({"type": "spotMetaAndAssetCtxs"}),
            json!([spot_meta, [{"coin": "PURR/USDC", "markPx": "0.1", "midPx": "0.1"}]]),
        )
        .await;

    let perps = read_json(&server, &["perps", "list"]);
    assert_eq!(perps[0]["name"], "BTC");
    assert_eq!(perps[0]["index"], 0);
    assert_eq!(perps[0]["max_leverage"], 50);
    assert_eq!(perps[0]["sz_decimals"], 5);
    assert_eq!(perps[0]["collateral"], "USDC");
    assert_eq!(perps[0]["tick_size_at_price_1"], "0.10");
    assert_eq!(perps[1]["name"], "kPEPE");
    assert_eq!(perps[1]["sz_decimals"], 0);
    assert_eq!(perps[1]["tick_size_at_price_1"], "0.0001");
    assert_eq!(read_json(&server, &["perps", "get", "kPEPE"]), perps[1]);

    let spot = read_json(&server, &["spot", "list"]);
    assert_eq!(spot[0]["symbol"], "PURR/USDC");
    assert_eq!(spot[0]["index"], 10_000);
    assert_eq!(spot[0]["base"], "PURR");
    assert_eq!(spot[0]["quote"], "USDC");
    assert_eq!(spot[0]["base_sz_decimals"], 0);
    assert_eq!(spot[0]["quote_sz_decimals"], 6);
    assert_eq!(spot[0]["tick_size_at_price_1"], "0.0001");
    let pair = read_json(&server, &["spot", "get", "PURR/USDC"]);
    for (key, value) in spot[0].as_object().unwrap() {
        assert_eq!(&pair[key], value, "spot get repeats the list field {key}");
    }
}

#[tokio::test]
async fn testnet_mode_uses_testnet_endpoint_override() {
    let env = IsolatedHome::new();
    let mainnet = ExchangeFixture::start().await;
    let testnet = mock_cross_area_server(wire::order_success_response(4242)).await;

    let status = env
        .account_command(TEST_ACCOUNT_PASSPHRASE)
        .env(MAINNET_API_OVERRIDE_ENV, mainnet.uri())
        .env(TESTNET_API_OVERRIDE_ENV, testnet.uri())
        .args(["--format", "json", "--testnet", "status"])
        .assert()
        .success();

    let status = support::json_stdout(&status);
    assert_eq!(status["network"], "testnet");
    assert!(
        status["api_url"]
            .as_str()
            .unwrap()
            .starts_with(&testnet.uri())
    );
    assert!(
        mainnet.received_requests().await.unwrap().is_empty(),
        "the mainnet endpoint is never read on the testnet"
    );
}

#[tokio::test]
async fn network_and_auth_errors_include_recovery_suggestions() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .env(API_OVERRIDE_ENV, "http://127.0.0.1:9")
        .args(["perps", "list"])
        .assert()
        .code(12)
        .stderr(predicate::str::contains("127.0.0.1:9"));

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["orders", "open"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("Authentication required"))
        .stderr(predicate::str::contains("hyperliquid setup"));
}
