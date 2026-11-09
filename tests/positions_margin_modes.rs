//! `positions update-leverage` and `positions update-margin` against markets
//! with different margin rules: cross-or-isolated, `noCross` and
//! `strictIsolated` (plus the legacy `onlyIsolated` flag alone), on the default
//! dex and on the HIP-3 dex `xyz`.

mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, fixture_perp_meta, json_stdout};

const VAULT: &str = "0x0000000000000000000000000000000000000001";

fn perp(name: &str, only_isolated: bool, margin_mode: Value) -> Value {
    json!({
        "name": name,
        "szDecimals": 2,
        "maxLeverage": 10,
        "onlyIsolated": only_isolated,
        "marginMode": margin_mode,
        "growthMode": "disabled"
    })
}

/// Default dex: BTC and ETH trade either way, PUMP is `strictIsolated`.
/// Dex `xyz` (slot 1, asset ids 110000..): HOOD `noCross`, STRICT
/// `strictIsolated`, LEGACY carries only the deprecated `onlyIsolated` flag,
/// OPEN trades either way.
async fn exchange() -> ExchangeFixture {
    exchange_listing(vec![
        perp("BTC", false, Value::Null),
        perp("ETH", false, Value::Null),
        perp("PUMP", true, json!("strictIsolated")),
    ])
    .await
}

/// [`exchange`] with `universe` as the default dex's markets.
async fn exchange_listing(universe: Vec<Value>) -> ExchangeFixture {
    exchange_with_meta(fixture_perp_meta(universe)).await
}

/// [`exchange`] answering the default dex's `meta` with `meta`.
async fn exchange_with_meta(meta: Value) -> ExchangeFixture {
    let server = ExchangeFixture::start_with(vec![Hip3Dex {
        name: "xyz",
        universe: vec![
            perp("xyz:HOOD", true, json!("noCross")),
            perp("xyz:STRICT", true, json!("strictIsolated")),
            perp("xyz:LEGACY", true, Value::Null),
            perp("xyz:OPEN", false, Value::Null),
        ],
        mids: json!({}),
    }])
    .await;
    server.override_info(json!({"type": "meta"}), meta).await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

fn position(coin: &str, leverage_type: &str) -> Value {
    json!({
        "type": "oneWay",
        "position": {
            "coin": coin,
            "szi": "1",
            "leverage": {"type": leverage_type, "value": 4},
            "entryPx": "100",
            "positionValue": "100",
            "unrealizedPnl": "0",
            "returnOnEquity": "0",
            "liquidationPx": null,
            "marginUsed": "25",
            "maxLeverage": 10,
            "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
        }
    })
}

async fn mount_positions(server: &ExchangeFixture, positions: Vec<Value>) {
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState"}),
            json!({
                "marginSummary": {
                    "accountValue": "1000",
                    "totalNtlPos": "100",
                    "totalRawUsd": "1000",
                    "totalMarginUsed": "25"
                },
                "crossMarginSummary": {
                    "accountValue": "1000",
                    "totalNtlPos": "100",
                    "totalRawUsd": "1000",
                    "totalMarginUsed": "25"
                },
                "crossMaintenanceMarginUsed": "10",
                "withdrawable": "900",
                "assetPositions": positions,
                "time": 1700000000000_u64
            }),
        )
        .await;
}

fn run(env: &IsolatedHome, server: &ExchangeFixture, args: &[&str]) -> assert_cmd::Command {
    let mut command = env.live_command(server);
    command.args(["--testnet", "positions"]).args(args);
    command
}

// ---- update-leverage ----

#[tokio::test]
async fn update_leverage_refuses_cross_on_every_isolated_only_market_and_names_the_fix() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for coin in ["xyz:HOOD", "xyz:STRICT", "xyz:LEGACY", "PUMP"] {
        run(
            &env,
            &server,
            &["update-leverage", "--coin", coin, "--leverage", "7"],
        )
        .assert()
        .code(13)
        .stderr(predicate::str::contains(format!(
            "{coin} is an isolated-only market"
        )))
        .stderr(predicate::str::contains(format!(
            "positions update-leverage --coin {coin} --leverage 7 --isolated"
        )));
    }

    assert!(
        server.exchange_actions().await.is_empty(),
        "nothing may reach the exchange"
    );
}

#[tokio::test]
async fn update_leverage_fix_command_keeps_the_acting_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    run(
        &env,
        &server,
        &[
            "update-leverage",
            "--coin",
            "xyz:HOOD",
            "--leverage",
            "7",
            "--on-behalf-of",
            VAULT,
        ],
    )
    .assert()
    .code(13)
    .stderr(predicate::str::contains(format!(
        "positions update-leverage --coin xyz:HOOD --leverage 7 --isolated --on-behalf-of {VAULT}"
    )));
}

#[tokio::test]
async fn update_leverage_isolated_is_sent_on_an_isolated_only_market() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    run(
        &env,
        &server,
        &[
            "update-leverage",
            "--coin",
            "xyz:HOOD",
            "--leverage",
            "7",
            "--isolated",
        ],
    )
    .assert()
    .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["type"], "updateLeverage");
    assert_eq!(actions[0]["asset"], 110_000);
    assert_eq!(actions[0]["isCross"], false);
    assert_eq!(actions[0]["leverage"], 7);
}

#[tokio::test]
async fn update_leverage_cross_is_still_sent_on_a_market_that_allows_it() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    run(
        &env,
        &server,
        &["update-leverage", "--coin", "xyz:OPEN", "--leverage", "3"],
    )
    .assert()
    .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions[0]["asset"], 110_003);
    assert_eq!(actions[0]["isCross"], true);
}

#[tokio::test]
async fn update_leverage_dry_run_refuses_cross_on_an_isolated_only_market() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    env.command_with_server(&server)
        .args([
            "--dry-run",
            "--testnet",
            "positions",
            "update-leverage",
            "--coin",
            "xyz:STRICT",
            "--leverage",
            "5",
        ])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("--isolated"));
}

#[tokio::test]
async fn update_leverage_on_behalf_of_puts_the_vault_on_the_wire() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    run(
        &env,
        &server,
        &[
            "update-leverage",
            "--coin",
            "ETH",
            "--leverage",
            "5",
            "--isolated",
            "--on-behalf-of",
            VAULT,
        ],
    )
    .assert()
    .success();

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["action"]["type"], "updateLeverage");
    assert_eq!(bodies[0]["vaultAddress"], VAULT);
}

#[tokio::test]
async fn update_leverage_without_on_behalf_of_sends_no_vault() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    run(
        &env,
        &server,
        &["update-leverage", "--coin", "ETH", "--leverage", "5"],
    )
    .assert()
    .success();

    assert!(server.exchange_bodies().await[0]["vaultAddress"].is_null());
}

#[tokio::test]
async fn update_leverage_dry_run_reports_the_acting_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let signer = expected_address(VALID_PRIVATE_KEY);

    let assert = env
        .live_command(&server)
        .args([
            "--format",
            "json",
            "--dry-run",
            "--testnet",
            "positions",
            "update-leverage",
            "--coin",
            "ETH",
            "--leverage",
            "5",
            "--on-behalf-of",
            VAULT,
        ])
        .assert()
        .success();

    let preview = json_stdout(&assert);
    assert_eq!(preview["signer"], signer);
    assert_eq!(preview["acting_as"], VAULT);
    assert_eq!(preview["vault_address"], VAULT);
    assert_eq!(preview["args"]["on_behalf_of"], VAULT);
    assert!(server.exchange_actions().await.is_empty());
}

/// A collateral token the SDK cannot find fails its decode with a plain error,
/// so the resolver falls back to the raw `meta`, which must carry the rules too.
#[tokio::test]
async fn the_raw_metadata_fallback_applies_the_same_margin_rules() {
    let env = IsolatedHome::new();
    let server = exchange_with_meta(json!({
        "universe": [
            perp("BTC", false, Value::Null),
            perp("PUMP", true, json!("strictIsolated")),
            perp("NOCROSS", true, json!("noCross")),
            perp("LEGACY", true, Value::Null),
            perp("FLEX", false, Value::Null),
        ],
        "collateralToken": 99
    }))
    .await;

    for coin in ["PUMP", "NOCROSS", "LEGACY"] {
        run(
            &env,
            &server,
            &["update-leverage", "--coin", coin, "--leverage", "7"],
        )
        .assert()
        .code(13)
        .stderr(predicate::str::contains(format!(
            "{coin} is an isolated-only market"
        )));
    }
    run(
        &env,
        &server,
        &["update-margin", "--coin", "PUMP", "--amount", "-1"],
    )
    .assert()
    .code(13)
    .stderr(predicate::str::contains("strict-isolated"));
    run(
        &env,
        &server,
        &["update-leverage", "--coin", "FLEX", "--leverage", "7"],
    )
    .assert()
    .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["asset"], 4);
    assert_eq!(actions[0]["isCross"], true);
}

// ---- update-margin ----

#[tokio::test]
async fn update_margin_removal_sends_a_negative_ntli() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("ETH", "isolated")]).await;

    run(
        &env,
        &server,
        &["update-margin", "--coin", "ETH", "--amount", "-5"],
    )
    .assert()
    .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["type"], "updateIsolatedMargin");
    assert_eq!(actions[0]["ntli"], -5_000_000);
    assert_eq!(actions[0]["isBuy"], true);
}

#[tokio::test]
async fn update_margin_addition_still_sends_a_positive_ntli() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("ETH", "isolated")]).await;

    run(
        &env,
        &server,
        &["update-margin", "--coin", "ETH", "--amount", "5.25"],
    )
    .assert()
    .success();

    assert_eq!(server.exchange_actions().await[0]["ntli"], 5_250_000);
}

#[tokio::test]
async fn update_margin_zero_is_refused_as_a_configuration_error() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    run(
        &env,
        &server,
        &["update-margin", "--coin", "ETH", "--amount", "0"],
    )
    .assert()
    .code(2)
    .stderr(predicate::str::contains("amount must not be zero"));

    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn update_margin_removal_is_refused_on_a_strict_isolated_market_before_any_lookup() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("xyz:STRICT", "isolated")]).await;

    for coin in ["xyz:STRICT", "PUMP"] {
        run(
            &env,
            &server,
            &["update-margin", "--coin", coin, "--amount", "-1"],
        )
        .assert()
        .code(13)
        .stderr(predicate::str::contains(format!(
            "{coin} is a strict-isolated market"
        )))
        .stderr(predicate::str::contains("cannot be removed"));
    }

    assert!(server.exchange_actions().await.is_empty());
    assert!(
        server.info_requests("clearinghouseState").await.is_empty(),
        "a local refusal needs no account lookup"
    );
}

#[tokio::test]
async fn update_margin_removal_is_allowed_on_no_cross_and_legacy_isolated_only_markets() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(
        &server,
        vec![
            position("xyz:HOOD", "isolated"),
            position("xyz:LEGACY", "isolated"),
        ],
    )
    .await;

    for coin in ["xyz:HOOD", "xyz:LEGACY"] {
        run(
            &env,
            &server,
            &["update-margin", "--coin", coin, "--amount", "-2"],
        )
        .assert()
        .success();
    }

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["asset"], 110_000);
    assert_eq!(actions[0]["ntli"], -2_000_000);
    assert_eq!(actions[1]["asset"], 110_002);
}

#[tokio::test]
async fn update_margin_addition_is_allowed_on_a_strict_isolated_market() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("xyz:STRICT", "isolated")]).await;

    run(
        &env,
        &server,
        &["update-margin", "--coin", "xyz:STRICT", "--amount", "3"],
    )
    .assert()
    .success();

    assert_eq!(server.exchange_actions().await[0]["ntli"], 3_000_000);
}

#[tokio::test]
async fn update_margin_checks_the_dex_qualified_position_of_the_signer() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("xyz:HOOD", "isolated")]).await;

    run(
        &env,
        &server,
        &["update-margin", "--coin", "xyz:HOOD", "--amount", "1"],
    )
    .assert()
    .success();

    let lookups = server.info_requests("clearinghouseState").await;
    assert_eq!(lookups.len(), 1);
    assert_eq!(lookups[0]["dex"], "xyz");
    assert_eq!(
        lookups[0]["user"].as_str().unwrap().to_lowercase(),
        expected_address(VALID_PRIVATE_KEY).to_lowercase()
    );
}

#[tokio::test]
async fn update_margin_without_an_open_position_is_refused_with_the_fix() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    // A position on another market must not satisfy the check, and a closed
    // one on this market is not open.
    let mut closed = position("ETH", "isolated");
    closed["position"]["szi"] = json!("0");
    mount_positions(&server, vec![position("BTC", "isolated"), closed]).await;

    run(
        &env,
        &server,
        &["update-margin", "--coin", "ETH", "--amount", "1"],
    )
    .assert()
    .code(13)
    .stderr(predicate::str::contains("no open ETH position"))
    .stderr(predicate::str::contains(
        "positions update-leverage --coin ETH --leverage <N> --isolated",
    ));

    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn update_margin_on_a_cross_position_is_refused_with_the_fix() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("ETH", "cross")]).await;

    run(
        &env,
        &server,
        &["update-margin", "--coin", "ETH", "--amount", "1"],
    )
    .assert()
    .code(13)
    .stderr(predicate::str::contains("uses cross margin"))
    .stderr(predicate::str::contains(
        "positions update-leverage --coin ETH --leverage 4 --isolated",
    ));

    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn update_margin_on_behalf_of_checks_the_vault_and_puts_it_on_the_wire() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("ETH", "isolated")]).await;

    run(
        &env,
        &server,
        &[
            "update-margin",
            "--coin",
            "ETH",
            "--amount",
            "-1",
            "--on-behalf-of",
            VAULT,
        ],
    )
    .assert()
    .success();

    let lookups = server.info_requests("clearinghouseState").await;
    assert_eq!(lookups[0]["user"], VAULT);
    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies[0]["vaultAddress"], VAULT);
    assert_eq!(bodies[0]["action"]["ntli"], -1_000_000);
}

#[tokio::test]
async fn update_margin_fix_command_keeps_the_acting_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_positions(&server, vec![position("ETH", "cross")]).await;

    run(
        &env,
        &server,
        &[
            "update-margin",
            "--coin",
            "ETH",
            "--amount",
            "1",
            "--on-behalf-of",
            VAULT,
        ],
    )
    .assert()
    .code(13)
    .stderr(predicate::str::contains(format!(
        "positions update-leverage --coin ETH --leverage 4 --isolated --on-behalf-of {VAULT}"
    )));
}

#[tokio::test]
async fn update_margin_dry_run_reports_the_direction_and_the_acting_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for (amount, direction, ntli) in [("2.5", "add", 2_500_000), ("-2.5", "remove", -2_500_000)] {
        let assert = env
            .live_command(&server)
            .args([
                "--format",
                "json",
                "--dry-run",
                "--testnet",
                "positions",
                "update-margin",
                "--coin",
                "ETH",
                "--amount",
                amount,
                "--on-behalf-of",
                VAULT,
            ])
            .assert()
            .success();

        let preview = json_stdout(&assert);
        assert_eq!(preview["args"]["direction"], direction);
        assert_eq!(preview["args"]["ntli"], ntli);
        assert_eq!(preview["args"]["amount"], amount);
        assert_eq!(preview["acting_as"], VAULT);
        assert_eq!(preview["vault_address"], VAULT);
    }

    assert!(server.exchange_actions().await.is_empty());
    assert!(server.info_requests("clearinghouseState").await.is_empty());
}

#[tokio::test]
async fn update_margin_dry_run_refuses_strict_isolated_removal_without_an_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    env.command_with_server(&server)
        .args([
            "--dry-run",
            "--testnet",
            "positions",
            "update-margin",
            "--coin",
            "xyz:STRICT",
            "--amount",
            "-1",
        ])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("strict-isolated"));
}
