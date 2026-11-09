mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::signing::{recover_signer, test_signer};
use support::{IsolatedHome, run_json, wire};

fn token(name: &str, index: u32, wei_decimals: u32) -> Value {
    json!({
        "name": name,
        "index": index,
        "tokenId": format!("0x{index:032x}"),
        "szDecimals": if name == "HYPE" { 2 } else { 6 },
        "weiDecimals": wei_decimals,
        "evmContract": null
    })
}

/// An exchange with the HIP-3 dexes `xyz` (market `xyz:TOKEN`, collateral
/// USDC) and `builderdex` (market `TSLA`, collateral USDH), a priority-gas
/// auction at 0.01, and every action accepted.
async fn advanced_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start_with(vec![
        Hip3Dex {
            name: "xyz",
            universe: vec![wire::perp_with(
                "xyz:TOKEN",
                2,
                10,
                wire::MarginRule::Either,
            )],
            mids: json!({"xyz:TOKEN": "1.23"}),
        },
        Hip3Dex {
            name: "builderdex",
            universe: vec![wire::perp_with("TSLA", 3, 10, wire::MarginRule::Either)],
            mids: json!({"builderdex:TSLA": "250"}),
        },
    ])
    .await;
    server
        .override_info(
            json!({"type": "meta", "dex": "builderdex"}),
            json!({
                "universe": [wire::perp_with("TSLA", 3, 10, wire::MarginRule::Either)],
                "collateralToken": 1
            }),
        )
        .await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({"universe": [], "tokens": [token("USDC", 0, 6), token("USDH", 1, 6)]}),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "gossipPriorityAuctionStatus"}),
            json!([
                [null, null, null, null, null],
                [{
                    "startTimeSeconds": 1710000000,
                    "durationSeconds": 180,
                    "startGas": "100",
                    "currentGas": "0.01",
                    "endGas": "10"
                }]
            ]),
        )
        .await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// An exchange listing the spot market HYPE/USDH, whose quote is not USDC.
async fn spot_quote_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "allMids"}), json!({"@1339": "100"}))
        .await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({
                "universe": [{"tokens": [1, 2], "index": 1339, "name": "HYPE/USDH"}],
                "tokens": [token("USDC", 0, 6), token("HYPE", 1, 8), token("USDH", 2, 6)]
            }),
        )
        .await;
    server
}

/// What `--dry-run` of `args` prints against `server`.
fn dry_run(server: &ExchangeFixture, args: &[&str]) -> Value {
    let env = IsolatedHome::new();
    run_json(
        env.command_with_server(server)
            .args(["--format", "json", "--dry-run"])
            .args(args),
    )
}

#[tokio::test]
async fn perps_list_dex_filters_to_hip3_markets() {
    let server = advanced_exchange().await;

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["perps", "list", "--dex", "xyz"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TOKEN"))
        .stdout(predicate::str::contains("110000"));
}

#[tokio::test]
async fn perps_get_dex_accepts_unprefixed_token_from_prefixed_market() {
    let server = advanced_exchange().await;

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["perps", "get", "TOKEN", "--dex", "xyz"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TOKEN"))
        .stdout(predicate::str::contains("110000"));
}

#[tokio::test]
async fn orders_create_dry_run_resolves_hip3_dex_markets() {
    let server = advanced_exchange().await;

    let json = dry_run(
        &server,
        &[
            "orders",
            "create",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--type",
            "market",
            "--amount",
            "12",
        ],
    );

    assert_eq!(json["args"]["asset_id"], 110000);
    assert_eq!(json["args"]["resolved_asset"], "xyz:TOKEN");
    assert_eq!(json["args"]["amount_unit"], "USDC");
}

/// `orders create` on the HIP-3 market with the exchange failing the dex
/// balance read, and the account-mode read too when `fail_abstraction`.
async fn hip3_order_attempt(
    server: &ExchangeFixture,
    fail_abstraction: bool,
) -> assert_cmd::assert::Assert {
    if fail_abstraction {
        server
            .mount_info_failure(json!({"type": "userAbstraction"}), 500)
            .await;
    } else {
        server
            .mount_info_containing(json!({"type": "userAbstraction"}), json!("disabled"))
            .await;
    }
    server
        .mount_info_failure(json!({"type": "clearinghouseState"}), 500)
        .await;

    IsolatedHome::new()
        .live_command(server)
        .args([
            "orders",
            "create",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--price",
            "1.23",
            "--size",
            "10",
            "--testnet",
        ])
        .assert()
}

#[tokio::test]
async fn hip3_margin_helper_does_not_transfer_when_the_dex_balance_cannot_be_read() {
    let server = advanced_exchange().await;

    hip3_order_attempt(&server, false)
        .await
        .code(12)
        .stderr(predicate::str::contains(
            "The Hyperliquid API failed to process the request.",
        ));
    assert!(
        server.exchange_actions().await.is_empty(),
        "a failed balance read must not plan a transfer or place the order"
    );
}

#[tokio::test]
async fn hip3_margin_helper_does_not_skip_the_transfer_when_the_account_mode_cannot_be_read() {
    let server = advanced_exchange().await;

    hip3_order_attempt(&server, true).await.code(12);
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn orders_create_dry_run_resolves_non_xyz_hip3_dex_markets() {
    let server = advanced_exchange().await;

    let json = dry_run(
        &server,
        &[
            "orders",
            "create",
            "--coin",
            "builderdex:TSLA",
            "--side",
            "buy",
            "--type",
            "market",
            "--amount",
            "12",
        ],
    );

    assert_eq!(json["args"]["asset_id"], 120000);
    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
    assert_eq!(json["args"]["amount_unit"], "USDH");
}

#[tokio::test]
async fn orders_tpsl_dry_run_resolves_hip3_dex_markets() {
    let server = advanced_exchange().await;

    let json = dry_run(
        &server,
        &[
            "orders",
            "tpsl",
            "--coin",
            "builderdex:TSLA",
            "--side",
            "sell",
            "--size",
            "0.1",
            "--take-profit",
            "260",
        ],
    );

    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
    assert_eq!(json["args"]["legs"][0]["order"]["a"], 120000);
}

#[tokio::test]
async fn orders_twap_create_dry_run_resolves_hip3_dex_markets() {
    let server = advanced_exchange().await;

    let json = dry_run(
        &server,
        &[
            "orders",
            "twap-create",
            "--coin",
            "builderdex:TSLA",
            "--side",
            "buy",
            "--size",
            "0.1",
            "--duration",
            "300",
        ],
    );

    assert_eq!(json["args"]["asset_id"], 120000);
    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
}

#[tokio::test]
async fn orders_twap_cancel_dry_run_resolves_hip3_dex_markets() {
    let server = advanced_exchange().await;

    let json = dry_run(
        &server,
        &[
            "orders",
            "twap-cancel",
            "--coin",
            "builderdex:TSLA",
            "12345",
        ],
    );

    assert_eq!(json["args"]["twap_id"], 12345);
    assert_eq!(json["args"]["resolved_asset"], "builderdex:TSLA");
}

#[tokio::test]
async fn orders_bracket_dry_run_resolves_hip3_dex_markets() {
    let server = advanced_exchange().await;

    let json = dry_run(
        &server,
        &[
            "orders",
            "bracket",
            "--coin",
            "xyz:TOKEN",
            "--side",
            "buy",
            "--size",
            "0.5",
            "--take-profit",
            "+5%",
            "--stop-loss",
            "-2%",
        ],
    );

    assert_eq!(json["args"]["entry_plan"]["resolved_asset"], "xyz:TOKEN");
    assert_eq!(
        json["args"]["protection_plan"]["resolved_asset"],
        "xyz:TOKEN"
    );
}

#[tokio::test]
async fn orders_create_dry_run_reports_non_usdc_spot_quote_amount_unit() {
    let server = spot_quote_exchange().await;

    let json = dry_run(
        &server,
        &[
            "orders",
            "create",
            "--coin",
            "HYPE/USDH",
            "--side",
            "buy",
            "--type",
            "market",
            "--amount",
            "10",
        ],
    );

    assert_eq!(json["args"]["asset_id"], 11339);
    assert_eq!(json["args"]["resolved_asset"], "HYPE/USDH");
    assert_eq!(json["args"]["amount_unit"], "USDH");
}

#[tokio::test]
async fn perps_list_unknown_dex_exits_13() {
    let server = advanced_exchange().await;

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["perps", "list", "--dex", "nonexistent"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("Unknown DEX: nonexistent"));
}

#[tokio::test]
async fn prio_status_shows_auction_status() {
    let server = advanced_exchange().await;

    IsolatedHome::new()
        .command_with_server(&server)
        .args(["prio", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Current Gas"))
        .stdout(predicate::str::contains("0.01"));
}

#[test]
fn prio_bid_without_required_parameters_exits_2_with_usage() {
    IsolatedHome::new()
        .command()
        .args(["prio", "bid"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--max"))
        .stderr(predicate::str::contains("--ip"));
}

#[tokio::test]
async fn prio_bid_with_explicit_parameters_submits_signed_bid() {
    let server = advanced_exchange().await;

    IsolatedHome::new()
        .live_command(&server)
        .args([
            "prio",
            "bid",
            "--slot",
            "0",
            "--max",
            "1",
            "--ip",
            "203.0.113.10",
            "--testnet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("submitted"))
        .stdout(predicate::str::contains("203.0.113.10"));

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0]["action"],
        json!({
            "type": "gossipPriorityBid",
            "slotId": 0,
            "ip": "203.0.113.10",
            "maxGas": 1_000_001_u64
        })
    );
    assert_eq!(recover_signer(&bodies[0], Chain::Testnet), test_signer());
}

#[tokio::test]
async fn prio_bid_cap_below_current_gas_does_not_exit_zero_or_submit() {
    let server = advanced_exchange().await;

    IsolatedHome::new()
        .live_command(&server)
        .args([
            "prio",
            "bid",
            "--slot",
            "0",
            "--max",
            "0.00000001",
            "--ip",
            "203.0.113.10",
            "--testnet",
        ])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("priority bid not submitted"));

    assert!(
        server.exchange_actions().await.is_empty(),
        "bid cap rejection must not submit"
    );
}

#[tokio::test]
async fn prio_bid_compares_decimal_current_gas_after_converting_to_wei() {
    let server = advanced_exchange().await;

    IsolatedHome::new()
        .live_command(&server)
        .args([
            "prio",
            "bid",
            "--slot",
            "0",
            "--max",
            "0.009",
            "--ip",
            "203.0.113.10",
            "--testnet",
        ])
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "current gas 0.01 is at or above --max 0.009",
        ));

    assert!(
        server.exchange_actions().await.is_empty(),
        "decimal current gas must be compared as wei before bidding"
    );
}

/// The bid cap is counted in HyperCore wei (10^8 per HYPE) whatever the spot
/// token list says: a bid that needed the token's decimals would read
/// `spotMeta`, and one that fell back to HyperEVM's 10^18 would send a cap
/// 10^10 times too large.
#[tokio::test]
async fn prio_bid_counts_the_cap_in_hypercore_wei_without_a_token_lookup() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "gossipPriorityAuctionStatus"}),
            json!([
                [null, null, null, null, null],
                [{"startTimeSeconds": 1710000000, "durationSeconds": 180, "startGas": "100", "endGas": "10"}]
            ]),
        )
        .await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;

    IsolatedHome::new()
        .live_json(&server)
        .args(["prio", "bid", "--max", "1.5", "--ip", "203.0.113.10"])
        .assert()
        .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["maxGas"], 150_000_000_u64);
    assert!(
        server.info_requests("spotMeta").await.is_empty(),
        "the cap must not depend on a spot token lookup"
    );
}
