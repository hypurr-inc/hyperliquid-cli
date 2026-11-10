mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, default_reply};
use support::reads::read_json;
use support::signing::assert_signed_by_test_key;
use support::{IsolatedHome, run_json, stderr_text, wire};

const USER: &str = "0x0000000000000000000000000000000000000001";
const VAULT: &str = "0x0000000000000000000000000000000000000002";

const ALPHA: &str = "0x0000000000000000000000000000000000000011";
const BETA: &str = "0x0000000000000000000000000000000000000022";
const GAMMA: &str = "0x0000000000000000000000000000000000000033";

fn vault_details(name: &str, description: &str, apr: f64, leader_fraction: f64) -> Value {
    json!({
        "name": name,
        "vaultAddress": VAULT,
        "leader": USER,
        "description": description,
        "portfolio": [
            ["allTime", {
                "accountValueHistory": [[1700000000000_u64, 12345.67]],
                "pnlHistory": [[1700000000000_u64, "234.56"]],
                "vlm": "98765.43"
            }]
        ],
        "apr": apr,
        "leaderFraction": leader_fraction,
        "leaderCommission": 0.05,
        "followers": [],
        "maxDistributable": 1000.0,
        "maxWithdrawable": 500.0,
        "isClosed": false,
        "relationship": {"type": "normal"},
        "allowDeposits": true,
        "alwaysCloseOnWithdraw": false
    })
}

fn vault_details_request() -> Value {
    json!({"type": "vaultDetails", "vaultAddress": VAULT})
}

/// A vault with details and one open BTC position.
async fn vault_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            vault_details_request(),
            vault_details("Test Vault", "market neutral test strategy", 0.1234, 0.1),
        )
        .await;
    server
        .mount_info(
            json!({"type": "clearinghouseState", "user": VAULT}),
            wire::perp_state_with(
                "15000",
                "2550",
                "100",
                "1000",
                vec![wire::position("BTC", "cross")],
            ),
        )
        .await;
    server
}

/// The stats dataset behind `vault list` and `vault search`, and one
/// deposit of `USER` in the first vault.
async fn stats_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_vault_stats(
            "Mainnet",
            vec![
                wire::stats_vault(
                    "Alpha Carry",
                    ALPHA,
                    "0x00000000000000000000000000000000000000aa",
                    "1000.5",
                    json!(0.12),
                    "normal",
                ),
                wire::stats_vault(
                    "Beta Yield",
                    BETA,
                    "0x00000000000000000000000000000000000000bb",
                    "2500",
                    json!(0.05),
                    "parent",
                ),
                wire::stats_vault(
                    "Gamma Hedge",
                    GAMMA,
                    "0x00000000000000000000000000000000000000cc",
                    "10",
                    json!(0.25),
                    "child",
                ),
            ],
        )
        .await;
    server
        .mount_info(
            json!({"type": "userVaultEquities", "user": USER}),
            json!([{"vaultAddress": ALPHA, "equity": "42.5", "lockedUntilTimestamp": null}]),
        )
        .await;
    server
}

fn names(rows: &Value) -> Vec<&str> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn vault_get_json_shows_details() {
    let server = vault_exchange().await;

    let json = read_json(&server, &["vault", "get", VAULT]);

    assert_eq!(json["name"], "Test Vault");
    assert_eq!(json["vault_address"], VAULT);
    assert_eq!(json["tvl"], "12345.67");
    assert_eq!(json["apr"], "0.1234");
    assert_eq!(json["allow_deposits"], true);
}

#[tokio::test]
async fn vault_get_json_accepts_scientific_notation_fields() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            vault_details_request(),
            vault_details(
                "Scientific Vault",
                "scientific notation fields",
                -5.262868504855727e-3,
                7.445690056409042e-7,
            ),
        )
        .await;

    let json = read_json(&server, &["vault", "get", VAULT]);

    assert_eq!(json["name"], "Scientific Vault");
    assert_eq!(json["leader_fraction"], "0.0000007445690056409042");
    assert_eq!(json["apr"], "-0.005262868504855727");
}

#[tokio::test]
async fn vault_get_null_response_returns_clean_not_found_error() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(vault_details_request(), Value::Null)
        .await;

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["vault", "get", VAULT])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("vault details not found"));
}

#[tokio::test]
async fn vault_positions_show_open_positions() {
    let server = vault_exchange().await;

    let positions = read_json(&server, &["vault", "positions", VAULT]);

    assert_eq!(positions["vault_address"], VAULT);
    assert_eq!(
        positions["positions"],
        json!([{
            "coin": "BTC", "side": "long", "size": "1", "entry": "100", "mark": "100",
            "position_value": "100", "unrealized_pnl": "0", "margin_used": "25",
            "leverage": "cross 4x", "liquidation_price": null
        }])
    );
}

#[tokio::test]
async fn vault_list_sorts_by_tvl_and_limits_results() {
    let server = stats_exchange().await;

    let rows = read_json(&server, &["vault", "list", "--limit", "2"]);

    assert_eq!(names(&rows), ["Beta Yield", "Alpha Carry"]);
    assert_eq!(rows[0]["vault_address"], BETA);
    assert_eq!(rows[0]["tvl"], "2500");
    assert_eq!(rows[0]["kind"], "parent");
}

#[tokio::test]
async fn vault_list_filters_kind_and_sorts() {
    let server = stats_exchange().await;

    let child = read_json(
        &server,
        &["vault", "list", "--kind", "child", "--sort", "apr"],
    );
    assert_eq!(names(&child), ["Gamma Hedge"]);
    assert_eq!(child[0]["apr"], "0.25");
    assert_eq!(child[0]["kind"], "child");

    // `protocol` and `user` are the labels of the exchange's own vaults and of
    // everything users run.
    let protocol = read_json(&server, &["vault", "list", "--kind", "protocol"]);
    assert_eq!(names(&protocol), ["Alpha Carry"]);
    assert_eq!(protocol[0]["kind"], "normal");

    let user = read_json(
        &server,
        &["vault", "list", "--kind", "user", "--sort", "name"],
    );
    assert_eq!(names(&user), ["Beta Yield", "Gamma Hedge"]);

    let parent = read_json(&server, &["vault", "list", "--kind", "parent"]);
    assert_eq!(names(&parent), ["Beta Yield"]);
}

#[tokio::test]
async fn vault_search_matches_name_leader_or_address_and_adds_the_users_deposit() {
    let server = stats_exchange().await;

    let by_leader = read_json(&server, &["vault", "search", "00bb"]);
    assert_eq!(names(&by_leader), ["Beta Yield"]);

    let by_name = read_json(&server, &["vault", "search", "alpha", "--user", USER]);
    assert_eq!(names(&by_name), ["Alpha Carry"]);
    assert_eq!(by_name[0]["user_deposit"], "42.5");

    assert_eq!(
        read_json(&server, &["vault", "search", "does-not-exist"]),
        json!([])
    );
}

#[test]
fn vault_search_rejects_malformed_user_before_network() {
    IsolatedHome::new()
        .command()
        .args(["vault", "search", "alpha", "--user", "not-an-address"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "address must be a 0x-prefixed 40-byte hex string",
        ));
}

#[test]
fn vault_actions_reject_bad_vault_and_amount_before_auth() {
    let zero = "0x0000000000000000000000000000000000000000";
    let cases: [(&str, &str, &str); 3] = [
        (zero, "1", "zero address"),
        ("INVALID", "1", "0x-prefixed"),
        (VAULT, "0.0000001", "amount"),
    ];

    for action in ["deposit", "withdraw"] {
        for (vault, amount, message) in cases {
            IsolatedHome::new()
                .command()
                .args(["vault", action, "--vault", vault, "--amount", amount])
                .arg("--testnet")
                .assert()
                .code(2)
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains(message));
        }
    }
}

#[tokio::test]
async fn vault_list_table_output_is_uncolored() {
    let server = stats_exchange().await;

    IsolatedHome::new()
        .command_with_server(&server)
        .env("FORCE_COLOR", "1")
        .args(["--format", "table", "vault", "list", "--limit", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Beta Yield"))
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[tokio::test]
async fn vault_deposit_and_withdraw_submit_vault_transfer_actions() {
    let server = ExchangeFixture::start().await;
    server.mount_exchange(default_reply()).await;
    let home = IsolatedHome::new();

    for (action, amount, deposit, usd) in [
        ("deposit", "1000", true, 1_000_000_000_u64),
        ("withdraw", "500", false, 500_000_000),
    ] {
        let receipt = run_json(
            home.live_json(&server)
                .args(["vault", action, "--vault", VAULT, "--amount", amount]),
        );

        assert_eq!(receipt["action"], action);
        assert_eq!(receipt["status"], "submitted");
        assert_eq!(receipt["network"], "Testnet");
        assert_eq!(receipt["vault_address"], VAULT);
        assert_eq!(receipt["amount"], amount);
        assert_eq!(receipt["asset"], "USDC");
        assert_eq!(receipt["reversibility"], "partially_reversible");
        assert_signed_by_test_key(&receipt);
        assert_eq!(
            server.exchange_actions().await.last().unwrap(),
            &json!({
                "type": "vaultTransfer",
                "vaultAddress": VAULT,
                "isDeposit": deposit,
                "usd": usd
            })
        );
    }
    assert_eq!(server.exchange_actions().await.len(), 2);
}

#[test]
fn vault_actions_without_wallet_exit_10_after_validation() {
    for action in ["deposit", "withdraw"] {
        IsolatedHome::new()
            .command()
            .args(["vault", action, "--vault", VAULT, "--amount", "1"])
            .arg("--testnet")
            .assert()
            .code(10)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Authentication required"));
    }
}

fn reserve_state(supply_rate: &str, balance: &str, oracle: &str, ltv: &str) -> Value {
    json!({
        "borrowYearlyRate": "0.05",
        "supplyYearlyRate": supply_rate,
        "balance": balance,
        "utilization": "0.31617754",
        "oraclePx": oracle,
        "ltv": ltv,
        "totalSupplied": "41203454.1833538637",
        "totalBorrowed": "13027606.7840966005"
    })
}

fn token(name: &str, index: u64) -> Value {
    json!({
        "name": name,
        "szDecimals": 8,
        "weiDecimals": 8,
        "index": index,
        "tokenId": format!("0x{index:032x}"),
        "isCanonical": true,
        "evmContract": null,
        "fullName": null
    })
}

/// USDC (token 0) and HYPE (token 150) with reserve states, `USER`'s
/// borrow/lend position and an exchange that accepts actions.
async fn borrowlend_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({"universe": [], "tokens": [token("USDC", 0), token("HYPE", 150)]}),
        )
        .await;
    server
        .mount_info(
            json!({"type": "allBorrowLendReserveStates"}),
            json!([
                [
                    0,
                    reserve_state("0.0142", "28175848.640569929", "1.0", "0.0")
                ],
                [
                    150,
                    reserve_state("0.0", "1354673.0887382801", "39.917", "0.5")
                ]
            ]),
        )
        .await;
    server
        .mount_info(
            json!({"type": "borrowLendReserveState", "token": 0}),
            reserve_state("0.0142", "28175848.640569929", "1.0", "0.0"),
        )
        .await;
    server
        .mount_info(
            json!({"type": "borrowLendUserState", "user": USER}),
            json!({
                "tokenToState": [
                    [0, {
                        "borrow": {"basis": "0.0", "value": "0.0"},
                        "supply": {"basis": "40.0", "value": "42.5"}
                    }],
                    [150, {
                        "borrow": {"basis": "3.5", "value": "3.75"},
                        "supply": {"basis": "0.0", "value": "0.0"}
                    }],
                    [7, {
                        "borrow": {"basis": "0.0", "value": "0.0"},
                        "supply": {"basis": "0.0", "value": "0.0"}
                    }]
                ],
                "health": "healthy",
                "healthFactor": null
            }),
        )
        .await;
    server.mount_exchange(default_reply()).await;
    server
}

#[tokio::test]
async fn borrowlend_rates_and_get_show_reserve_rows() {
    let server = borrowlend_exchange().await;

    let rates = read_json(&server, &["borrowlend", "rates"]);
    assert_eq!(rates[0]["token"], "USDC");
    assert_eq!(rates[0]["supply_rate"], "0.0142");
    assert_eq!(rates[0]["borrow_rate"], "0.05");
    assert_eq!(rates[0]["total_supply"], "41203454.1833538637");
    assert_eq!(rates[0]["total_borrow"], "13027606.7840966005");
    assert!(
        rates[0]["note"]
            .as_str()
            .unwrap()
            .contains("Live reserve state")
    );

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["borrowlend", "get", "USDC"])
        .assert()
        .success()
        .stdout(predicate::str::contains("USDC"))
        .stdout(predicate::str::contains("0.0142"))
        .stdout(predicate::str::contains("0.05"))
        .stdout(predicate::str::contains("Live reserve state"));
}

#[tokio::test]
async fn borrowlend_reserve_unavailable_fallback_is_explicit() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({"universe": [], "tokens": [token("USDC", 0)]}),
        )
        .await;
    for request in [
        json!({"type": "allBorrowLendReserveStates"}),
        json!({"type": "borrowLendReserveState", "token": 0}),
    ] {
        server
            .mount_info_error(request, 400, json!({"error": "unknown info request type"}))
            .await;
    }

    let rates = read_json(&server, &["borrowlend", "rates"]);
    assert_eq!(rates[0]["token"], "USDC");
    assert_eq!(rates[0]["supply_rate"], "0");
    assert_eq!(rates[0]["borrow_rate"], "0");
    assert!(
        rates[0]["note"]
            .as_str()
            .unwrap()
            .contains("unavailable from public Hyperliquid API")
    );

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["borrowlend", "get", "USDC"])
        .assert()
        .success()
        .stdout(predicate::str::contains("USDC"))
        .stdout(predicate::str::contains(
            "unavailable from public Hyperliquid API",
        ));
}

#[tokio::test]
async fn borrowlend_user_splits_supplied_and_borrowed_balances_without_wallet() {
    let server = borrowlend_exchange().await;

    let json = read_json(&server, &["borrowlend", "user", USER]);

    assert_eq!(json["user"], USER);
    let positions = json["positions"].as_array().unwrap();
    assert_eq!(positions.len(), 2, "an all-zero token is not a position");
    let usdc = positions
        .iter()
        .find(|position| position["token"] == "USDC")
        .unwrap();
    assert_eq!(usdc["supplied"], "42.5");
    assert_eq!(usdc["borrowed"], "0");
    assert_eq!(usdc["supply_basis"], "40");
    let hype = positions
        .iter()
        .find(|position| position["token"] == "HYPE")
        .unwrap();
    assert_eq!(hype["supplied"], "0");
    assert_eq!(hype["borrowed"], "3.75");
    assert_eq!(hype["borrow_basis"], "3.5");
    assert_eq!(json["health"], "healthy");
    assert!(json["health_factor"].is_null());
    // The spot balances of an account are not its borrow/lend position: a
    // unified account holds a different number there.
    assert!(
        server
            .info_requests("spotClearinghouseState")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn borrowlend_user_reports_an_unhealthy_state_and_its_factor() {
    let server = borrowlend_exchange().await;
    server
        .override_info(
            json!({"type": "borrowLendUserState", "user": USER}),
            json!({
                "tokenToState": [[999, {
                    "borrow": {"basis": "1", "value": "2"},
                    "supply": {"basis": "0", "value": "0"}
                }]],
                "health": "liquidatable",
                "healthFactor": 0.97
            }),
        )
        .await;

    let json = read_json(&server, &["borrowlend", "user", USER]);

    assert_eq!(json["health"], "liquidatable");
    assert_eq!(json["health_factor"], "0.97");
    // A token the spot metadata does not list keeps its index as its name.
    assert_eq!(json["positions"][0]["token"], "#999");
    assert_eq!(json["positions"][0]["borrowed"], "2");
}

#[tokio::test]
async fn borrowlend_user_of_a_zero_state_unified_account_is_empty_but_healthy() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "borrowLendUserState", "user": USER}),
            json!({"tokenToState": [], "health": "healthy", "healthFactor": null}),
        )
        .await;

    let json = read_json(&server, &["borrowlend", "user", USER]);
    assert_eq!(json["positions"], json!([]));
    assert_eq!(json["health"], "healthy");
    assert!(json["health_factor"].is_null());

    let people = IsolatedHome::new()
        .command_with_server(&server)
        .args(["--format", "table", "borrowlend", "user", USER])
        .assert()
        .success();
    assert!(stderr_text(&people).contains("No borrow/lend positions (health: healthy)."));
}

#[tokio::test]
async fn borrowlend_user_table_and_pretty_show_borrowed_amounts_as_positive() {
    let server = borrowlend_exchange().await;

    for args in [
        vec!["borrowlend", "user", USER],
        vec!["--format", "table", "borrowlend", "user", USER],
    ] {
        let assert = IsolatedHome::new()
            .command_with_server(&server)
            .args(args)
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        let hype_line = stdout
            .lines()
            .find(|line| line.contains("HYPE"))
            .expect("HYPE borrow row should be rendered");
        let fields = hype_line
            .split_whitespace()
            .filter(|field| *field != "│" && *field != "|")
            .collect::<Vec<_>>();

        assert_eq!(fields, vec![USER, "HYPE", "0", "3.75", "healthy"]);
    }
}

#[tokio::test]
async fn borrowlend_supply_dry_run_emits_core_writer_preview() {
    let server = borrowlend_exchange().await;

    let json = run_json(IsolatedHome::new().live_json(&server).args([
        "--dry-run",
        "borrowlend",
        "supply",
        "USDC",
        "--amount",
        "10",
    ]));

    assert_eq!(json["dry_run"], true);
    assert_eq!(json["command"], "borrowlend supply");
    assert_eq!(json["would_execute"], "supply_borrowlend");
    assert_eq!(json["args"]["operation"], "supply");
    assert_eq!(json["args"]["encoded_operation"], 0);
    assert_eq!(json["args"]["token"], "USDC");
    assert_eq!(json["args"]["token_index"], 0);
    assert_eq!(json["args"]["amount"], "10");
    assert_eq!(json["args"]["network"], "Testnet");
    assert_eq!(json["args"]["wei"], 1_000_000_000_u64);
    assert_eq!(json["args"]["reversibility"], "partially_reversible");
    assert_eq!(
        json["args"]["verified_shape"]["transport"],
        "HyperEVM CoreWriter.sendRawAction"
    );
    assert_eq!(json["args"]["verified_shape"]["action_id"], 15);
    assert_signed_by_test_key(&json);
    assert!(json["vault_address"].is_null());
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn borrowlend_withdraw_dry_run_supports_max_preview() {
    let server = borrowlend_exchange().await;

    let json = run_json(IsolatedHome::new().live_json(&server).args([
        "--dry-run",
        "borrowlend",
        "withdraw",
        "USDC",
        "--max",
    ]));

    assert_eq!(json["command"], "borrowlend withdraw");
    assert_eq!(json["would_execute"], "withdraw_borrowlend");
    assert_eq!(json["args"]["operation"], "withdraw");
    assert_eq!(json["args"]["encoded_operation"], 1);
    assert_eq!(json["args"]["max"], true);
    assert_eq!(json["args"]["network"], "Testnet");
    assert_eq!(json["args"]["wei"], 0);
    assert_eq!(json["args"]["reversibility"], "partially_reversible");
    assert_signed_by_test_key(&json);
    assert!(json["vault_address"].is_null());
}

#[tokio::test]
async fn borrowlend_action_validation_fails_before_auth() {
    let server = borrowlend_exchange().await;

    IsolatedHome::new()
        .command_with_server(&server)
        .args([
            "--dry-run",
            "borrowlend",
            "supply",
            "USDC",
            "--amount",
            "0",
            "--testnet",
        ])
        .assert()
        .code(13)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "borrow/lend amount must be greater than zero",
        ));
}

#[tokio::test]
async fn borrowlend_live_actions_submit_verified_exchange_shape() {
    let server = borrowlend_exchange().await;
    let home = IsolatedHome::new();

    let supply =
        run_json(
            home.live_json(&server)
                .args(["borrowlend", "supply", "USDC", "--amount", "5"]),
        );
    assert_eq!(supply["status"], "submitted");
    assert_eq!(supply["action"], "borrowlend");
    assert_eq!(supply["operation"], "supply");
    assert_eq!(supply["token"], "USDC");
    assert_eq!(supply["token_index"], 0);
    assert_eq!(supply["amount"], "5");
    assert_eq!(supply["max"], false);
    assert_eq!(supply["network"], "Testnet");
    assert_eq!(supply["reversibility"], "partially_reversible");
    assert_signed_by_test_key(&supply);

    let withdraw =
        run_json(
            home.live_json(&server)
                .args(["borrowlend", "withdraw", "USDC", "--max"]),
        );
    assert_eq!(withdraw["operation"], "withdraw");
    assert_eq!(withdraw["amount"], Value::Null);
    assert_eq!(withdraw["max"], true);
    assert_eq!(withdraw["reversibility"], "partially_reversible");
    assert_signed_by_test_key(&withdraw);

    assert_eq!(
        server.exchange_actions().await,
        [
            json!({"type": "borrowLend", "operation": "supply", "token": 0, "amount": "5"}),
            json!({"type": "borrowLend", "operation": "withdraw", "token": 0, "amount": null}),
        ]
    );
}
