//! `--margin-mode` on orders: verified against the account's current mode for
//! the market (`activeAssetData`, which needs no open position), refused up
//! front on isolated-only markets, and previewed by `--dry-run`.

mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::wire::{self, MarginRule};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout};

const VAULT: &str = "0x0000000000000000000000000000000000000001";

/// Default dex: BTC and ETH trade either way, PUMP is `strictIsolated`. Dex
/// `xyz`: OPEN trades either way, HOOD is `noCross`.
async fn exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start_with(vec![Hip3Dex {
        name: "xyz",
        universe: vec![
            wire::perp_with("xyz:OPEN", 2, 10, MarginRule::Either),
            wire::perp_with("xyz:HOOD", 2, 10, MarginRule::NoCross),
        ],
        mids: json!({"xyz:OPEN": "100", "xyz:HOOD": "50"}),
    }])
    .await;
    server
        .override_info(
            json!({"type": "meta"}),
            wire::perp_meta(vec![
                wire::perp_with("BTC", 5, 10, MarginRule::Either),
                wire::perp_with("ETH", 4, 10, MarginRule::Either),
                wire::perp_with("PUMP", 2, 10, MarginRule::StrictIsolated),
            ]),
        )
        .await;
    server
}

/// [`exchange`] where every order is accepted and left resting.
async fn placing_exchange() -> ExchangeFixture {
    let server = exchange().await;
    server
        .mount_order_statuses(vec![json!({"resting": {"oid": 1, "cloid": null}})])
        .await;
    server
}

/// What `activeAssetData` answers for `coin`: the account's leverage setting.
async fn mount_mode(server: &ExchangeFixture, coin: &str, leverage_type: &str) {
    let leverage = if leverage_type == "isolated" {
        json!({"type": "isolated", "value": 10, "rawUsd": "0.0"})
    } else {
        json!({"type": "cross", "value": 20})
    };
    server
        .mount_info_containing(
            json!({"type": "activeAssetData", "coin": coin}),
            json!({
                "user": VAULT,
                "coin": coin,
                "leverage": leverage,
                "maxTradeSzs": ["1", "1"],
                "availableToTrade": ["1", "1"],
                "markPx": "100"
            }),
        )
        .await;
}

fn live(env: &IsolatedHome, server: &ExchangeFixture, args: &[&str]) -> assert_cmd::Command {
    let mut command = env.live_command(server);
    command.arg("--testnet").args(args);
    command
}

fn create(coin: &str, extra: &[&str]) -> Vec<String> {
    let mut args = ["orders", "create", "--coin", coin, "--side", "buy"]
        .map(String::from)
        .to_vec();
    args.extend(["--price", "40", "--size", "0.01"].map(String::from));
    args.extend(extra.iter().map(|arg| arg.to_string()));
    args
}

fn run(env: &IsolatedHome, server: &ExchangeFixture, args: &[String]) -> assert_cmd::Command {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    live(env, server, &args)
}

async fn assert_nothing_sent(server: &ExchangeFixture) {
    assert!(server.exchange_actions().await.is_empty());
}

// ---- verification against the account's current mode ----

#[tokio::test]
async fn isolated_passes_without_a_position_when_the_account_is_set_to_isolated() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;
    mount_mode(&server, "BTC", "isolated").await;

    run(
        &env,
        &server,
        &create("BTC", &["--margin-mode", "isolated"]),
    )
    .assert()
    .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["type"], "order");
    let lookups = server.info_requests("activeAssetData").await;
    assert_eq!(lookups.len(), 1);
    assert_eq!(lookups[0]["coin"], "BTC");
    assert_eq!(
        lookups[0]["user"].as_str().unwrap().to_lowercase(),
        expected_address(VALID_PRIVATE_KEY).to_lowercase()
    );
    assert!(
        server.info_requests("clearinghouseState").await.is_empty(),
        "the mode must not depend on an open position"
    );
}

#[tokio::test]
async fn isolated_is_refused_when_the_account_is_set_to_cross_and_names_the_fix() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "cross").await;

    run(
        &env,
        &server,
        &create("BTC", &["--margin-mode", "isolated"]),
    )
    .assert()
    .code(2)
    .stderr(predicate::str::contains("requested --margin-mode isolated"))
    .stderr(predicate::str::contains("current BTC margin mode is cross"))
    .stderr(predicate::str::contains(
        "positions update-leverage --coin BTC --leverage <N> --isolated",
    ));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn cross_is_refused_when_the_account_is_set_to_isolated_and_names_the_fix() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "isolated").await;

    run(&env, &server, &create("BTC", &["--margin-mode", "cross"]))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("requested --margin-mode cross"))
        .stderr(predicate::str::contains(
            "current BTC margin mode is isolated",
        ))
        .stderr(predicate::str::contains(
            "`positions update-leverage --coin BTC --leverage <N>` first",
        ));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn cross_passes_when_the_account_is_set_to_cross() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;
    mount_mode(&server, "BTC", "cross").await;

    run(&env, &server, &create("BTC", &["--margin-mode", "cross"]))
        .assert()
        .success();

    assert_eq!(server.exchange_actions().await.len(), 1);
}

#[tokio::test]
async fn no_margin_mode_flag_means_no_lookup() {
    let env = IsolatedHome::new();
    let server = placing_exchange().await;

    run(&env, &server, &create("BTC", &[])).assert().success();

    assert!(server.info_requests("activeAssetData").await.is_empty());
}

#[tokio::test]
async fn the_lookup_and_the_fix_follow_the_acting_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "cross").await;

    run(
        &env,
        &server,
        &create(
            "BTC",
            &["--margin-mode", "isolated", "--on-behalf-of", VAULT],
        ),
    )
    .assert()
    .code(2)
    .stderr(predicate::str::contains(format!(
        "positions update-leverage --coin BTC --leverage <N> --isolated --on-behalf-of {VAULT}"
    )));

    let lookups = server.info_requests("activeAssetData").await;
    assert_eq!(lookups[0]["user"], VAULT);
}

#[tokio::test]
async fn a_hip3_market_is_looked_up_by_its_dex_qualified_name() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    // Answers only the qualified name: the bare `OPEN` would not be found.
    mount_mode(&server, "xyz:OPEN", "isolated").await;
    server
        .mount_exchange(json!({
            "status": "ok",
            "response": {"type": "twapOrder", "data": {"status": {"running": {"twapId": 9}}}}
        }))
        .await;

    live(
        &env,
        &server,
        &[
            "orders",
            "twap-create",
            "--coin",
            "xyz:OPEN",
            "--side",
            "buy",
            "--size",
            "1",
            "--duration",
            "300",
            "--margin-mode",
            "isolated",
        ],
    )
    .assert()
    .success();

    let lookups = server.info_requests("activeAssetData").await;
    assert_eq!(lookups.len(), 1);
    assert_eq!(lookups[0]["coin"], "xyz:OPEN");
    assert_eq!(server.exchange_actions().await[0]["type"], "twapOrder");
}

#[tokio::test]
async fn a_hip3_mismatch_names_the_qualified_market() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "xyz:OPEN", "cross").await;

    live(
        &env,
        &server,
        &[
            "orders",
            "twap-create",
            "--coin",
            "xyz:OPEN",
            "--side",
            "buy",
            "--size",
            "1",
            "--duration",
            "300",
            "--margin-mode",
            "isolated",
        ],
    )
    .assert()
    .code(2)
    .stderr(predicate::str::contains(
        "current xyz:OPEN margin mode is cross",
    ))
    .stderr(predicate::str::contains(
        "positions update-leverage --coin xyz:OPEN --leverage <N> --isolated",
    ));
}

#[tokio::test]
async fn scale_reads_the_mode_once_for_the_whole_ladder() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "isolated").await;
    server
        .mount_order_statuses(vec![
            json!({"resting": {"oid": 1, "cloid": null}}),
            json!({"resting": {"oid": 2, "cloid": null}}),
        ])
        .await;

    live(
        &env,
        &server,
        &[
            "orders",
            "scale",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--start-price",
            "49000",
            "--end-price",
            "50000",
            "--total-size",
            "0.01",
            "--orders",
            "2",
            "--margin-mode",
            "isolated",
        ],
    )
    .assert()
    .success();

    assert_eq!(server.info_requests("activeAssetData").await.len(), 1);
}

#[tokio::test]
async fn tpsl_verifies_the_mode_without_needing_a_position() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "ETH", "cross").await;

    live(
        &env,
        &server,
        &[
            "orders",
            "tpsl",
            "--coin",
            "ETH",
            "--side",
            "sell",
            "--size",
            "1",
            "--stop-loss",
            "2500",
            "--margin-mode",
            "isolated",
        ],
    )
    .assert()
    .code(2)
    .stderr(predicate::str::contains("current ETH margin mode is cross"));

    assert_nothing_sent(&server).await;
}

// ---- isolated-only markets ----

#[tokio::test]
async fn cross_is_refused_on_isolated_only_markets_before_any_account_lookup() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for coin in ["xyz:HOOD", "PUMP"] {
        run(&env, &server, &create(coin, &["--margin-mode", "cross"]))
            .assert()
            .code(13)
            .stderr(predicate::str::contains(format!(
                "{coin} is an isolated-only market"
            )))
            .stderr(predicate::str::contains("--margin-mode isolated"));
    }
    live(
        &env,
        &server,
        &[
            "orders",
            "twap-create",
            "--coin",
            "PUMP",
            "--side",
            "buy",
            "--size",
            "1",
            "--duration",
            "300",
            "--margin-mode",
            "cross",
        ],
    )
    .assert()
    .code(13);
    live(
        &env,
        &server,
        &[
            "orders",
            "tpsl",
            "--coin",
            "xyz:HOOD",
            "--side",
            "sell",
            "--size",
            "1",
            "--stop-loss",
            "10",
            "--margin-mode",
            "cross",
        ],
    )
    .assert()
    .code(13);
    live(
        &env,
        &server,
        &[
            "orders",
            "scale",
            "--coin",
            "PUMP",
            "--side",
            "buy",
            "--start-price",
            "40",
            "--end-price",
            "41",
            "--total-size",
            "1",
            "--orders",
            "2",
            "--margin-mode",
            "cross",
        ],
    )
    .assert()
    .code(13);

    assert!(
        server.info_requests("activeAssetData").await.is_empty(),
        "the market rule answers without asking the account"
    );
    assert!(server.info_requests("clearinghouseState").await.is_empty());
    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn tpsl_refuses_cross_on_an_isolated_only_market_before_reading_the_position() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    // Neither --side nor --size: a live run would read the position first.
    live(
        &env,
        &server,
        &[
            "orders",
            "tpsl",
            "--coin",
            "PUMP",
            "--stop-loss",
            "10",
            "--margin-mode",
            "cross",
        ],
    )
    .assert()
    .code(13)
    .stderr(predicate::str::contains("PUMP is an isolated-only market"));

    assert!(server.info_requests("clearinghouseState").await.is_empty());
    assert!(server.info_requests("activeAssetData").await.is_empty());
}

#[tokio::test]
async fn isolated_is_accepted_on_an_isolated_only_market_without_a_lookup() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    server
        .mount_exchange(json!({
            "status": "ok",
            "response": {"type": "twapOrder", "data": {"status": {"running": {"twapId": 3}}}}
        }))
        .await;

    live(
        &env,
        &server,
        &[
            "orders",
            "twap-create",
            "--coin",
            "xyz:HOOD",
            "--side",
            "buy",
            "--size",
            "1",
            "--duration",
            "300",
            "--margin-mode",
            "isolated",
        ],
    )
    .assert()
    .success();

    assert!(server.info_requests("activeAssetData").await.is_empty());
    assert_eq!(server.exchange_actions().await[0]["twap"]["a"], 110_001);
}

// ---- dry-run previews ----

/// One command per previewing order flow, for a perpetual `coin`.
fn previews(coin: &str) -> Vec<Vec<String>> {
    let strings = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    vec![
        create(coin, &[]),
        strings(&[
            "orders",
            "scale",
            "--coin",
            coin,
            "--side",
            "buy",
            "--start-price",
            "40",
            "--end-price",
            "41",
            "--total-size",
            "1",
            "--orders",
            "2",
        ]),
        strings(&[
            "orders",
            "twap-create",
            "--coin",
            coin,
            "--side",
            "buy",
            "--size",
            "1",
            "--duration",
            "300",
        ]),
        strings(&[
            "orders",
            "tpsl",
            "--coin",
            coin,
            "--side",
            "sell",
            "--size",
            "1",
            "--stop-loss",
            "10",
        ]),
    ]
}

fn dry_run(command: &mut assert_cmd::Command, args: &[String]) -> Value {
    let assert = command
        .args(["--format", "json", "--dry-run", "--testnet"])
        .args(args)
        .assert()
        .success();
    json_stdout(&assert)["args"].clone()
}

#[tokio::test]
async fn preview_shows_the_requested_mode_and_does_not_ask_the_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "cross").await;

    for mut args in previews("BTC") {
        args.extend(["--margin-mode", "isolated"].map(String::from));
        let preview = dry_run(&mut env.live_command(&server), &args);
        assert_eq!(preview["margin_mode"], "isolated", "{args:?}");
    }

    assert!(server.info_requests("activeAssetData").await.is_empty());
}

#[tokio::test]
async fn preview_shows_the_account_mode_when_an_account_is_known() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "isolated").await;

    for args in previews("BTC") {
        let preview = dry_run(&mut env.live_command(&server), &args);
        assert_eq!(preview["margin_mode"], "isolated", "{args:?}");
    }
    let lookups = server.info_requests("activeAssetData").await;
    assert_eq!(lookups.len(), 4);
    assert!(lookups.iter().all(|lookup| lookup["coin"] == "BTC"));
}

#[tokio::test]
async fn preview_reads_the_acting_account_named_by_on_behalf_of() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "isolated").await;

    // No signer at all: the account comes from `--on-behalf-of`.
    let mut args = create("BTC", &[]);
    args.extend(["--on-behalf-of", VAULT].map(String::from));
    let preview = dry_run(&mut env.command_with_server(&server), &args);

    assert_eq!(preview["margin_mode"], "isolated");
    assert_eq!(
        server.info_requests("activeAssetData").await[0]["user"],
        VAULT
    );
}

#[tokio::test]
async fn preview_falls_back_to_the_market_default_without_an_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    mount_mode(&server, "BTC", "isolated").await;

    for (coin, expected) in [
        ("BTC", "cross"),
        ("PUMP", "isolated"),
        ("xyz:HOOD", "isolated"),
    ] {
        for args in previews(coin) {
            let preview = dry_run(&mut env.command_with_server(&server), &args);
            assert_eq!(preview["margin_mode"], expected, "{args:?}");
        }
    }

    assert!(server.info_requests("activeAssetData").await.is_empty());
}

#[tokio::test]
async fn preview_of_an_isolated_only_market_needs_no_lookup_even_with_an_account() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for args in previews("PUMP") {
        let preview = dry_run(&mut env.live_command(&server), &args);
        assert_eq!(preview["margin_mode"], "isolated", "{args:?}");
    }

    assert!(server.info_requests("activeAssetData").await.is_empty());
}

#[tokio::test]
async fn preview_keeps_the_market_default_when_the_account_cannot_be_read() {
    let env = IsolatedHome::new();
    // No activeAssetData answer is mounted, so the read fails.
    let server = exchange().await;

    for args in previews("BTC") {
        let preview = dry_run(&mut env.live_command(&server), &args);
        assert_eq!(preview["margin_mode"], "cross", "{args:?}");
    }
}

#[tokio::test]
async fn preview_refuses_cross_on_an_isolated_only_market() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for mut args in previews("PUMP") {
        args.extend(["--margin-mode", "cross"].map(String::from));
        env.live_command(&server)
            .args(["--dry-run", "--testnet"])
            .args(&args)
            .assert()
            .code(13)
            .stderr(predicate::str::contains("PUMP is an isolated-only market"));
    }
}
