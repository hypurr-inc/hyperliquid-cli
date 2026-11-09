//! `orders create`, `scale`, `batch-create`, `tpsl` and the `buy` shortcut: flag
//! validation, dry-run previews, the signed action each sends, mainnet
//! confirmation and how the exchange's answers are reported.

mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use support::exchange::ExchangeFixture;
use support::orders::{
    answer_info, assert_nothing_sent, dry_run, error_text, mainnet_command, stage_fixture,
    suggested_invocation, testnet_command, words, write_orders_file,
};
use support::wire::{self, MarginRule};
use support::{
    IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout, run_json, stderr_text,
};

const LIMIT_BUY: &str = "orders create --coin BTC --side buy --price 50000 --size 0.1";
const MARKET_BUY: &str = "orders create --coin BTC --side buy --type market --amount 100";
const BUILDER: &str = "0x8c967E73E7B15087c42A10D344cFf4c96D877f1D";

/// An exchange that leaves every order resting under `oid`.
async fn exchange_resting(oid: u64) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(oid)])
        .await;
    server
}

/// Run `line` offline: it must be refused before any request is made.
fn refused(line: &str) -> assert_cmd::assert::Assert {
    IsolatedHome::new()
        .command()
        .arg("--testnet")
        .args(words(line))
        .assert()
}

#[test]
fn incompatible_flags_exit_2_and_say_what_to_remove() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        (
            "orders create --coin BTC --side buy",
            &["--price", "--size"],
            &[],
        ),
        (
            "orders create --coin BTC --side buy --price 50000 --size 0.1 --amount 100",
            &["--type limit", "remove --amount", "--type market --amount"],
            &[],
        ),
        (
            "orders create --coin BTC --side buy --price 50000 --size 0.1 --amount 0",
            &["--type limit", "remove --amount"],
            &["amount must be greater than zero"],
        ),
        (
            "orders create --coin BTC --side buy --type market --amount 100 --price 50000 --size 0.1",
            &["--type market", "remove --price", "--amount"],
            &[],
        ),
        (
            "orders create --coin BTC --side buy --type market --amount 100 --trigger-price 50000",
            &[
                "--type market",
                "remove --trigger-price",
                "stop-loss/take-profit",
                "stop-limit/take-limit",
            ],
            &[],
        ),
        (
            "orders create --coin BTC --side buy --type market --amount 100 --price 0 --size 0",
            &["--type market", "remove --price"],
            &["price must be positive", "size must be greater than zero"],
        ),
        (
            "orders create --coin BTC --side sell --type stop-loss --price 49000 --size 0.1 --amount 100",
            &[
                "--type stop-loss",
                "remove --amount",
                "uses --trigger-price",
                "legacy --price",
            ],
            &[],
        ),
        (
            "orders create --coin BTC --side sell --type take-profit --price 55000 --size 0.1 --amount 100",
            &["--type take-profit", "remove --amount"],
            &[],
        ),
        (
            "orders create --coin BTC --side sell --type stop-limit --price 89500 --size 0.001",
            &["--trigger-price", "limit trigger orders"],
            &[],
        ),
        (
            "orders create --coin BTC --side sell --type stop-loss --trigger-price 49000 --price 48500 --size 0.001",
            &[
                "--type stop-loss",
                "remove one price flag",
                "stop-limit/take-limit",
            ],
            &[],
        ),
    ];
    for (line, expected, absent) in cases {
        let assert = refused(line).code(2);
        let stderr = stderr_text(&assert);
        for fragment in *expected {
            assert!(
                stderr.contains(fragment),
                "{line}: {fragment:?} in {stderr}"
            );
        }
        for fragment in *absent {
            assert!(
                !stderr.contains(fragment),
                "{line}: {fragment:?} in {stderr}"
            );
        }
    }
}

#[test]
fn a_price_that_is_not_positive_is_refused_without_ansi_in_table_output() {
    IsolatedHome::new()
        .command()
        .args(["--format", "table", "--testnet"])
        .args(words(
            "orders create --coin BTC --side buy --price -50000 --size 0.1",
        ))
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Error: Configuration error: price must be positive",
        ))
        .stderr(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn a_zero_size_is_refused() {
    refused("orders create --coin BTC --side buy --price 50000 --size 0")
        .code(2)
        .stderr(predicate::str::contains("size must be greater than zero"));
}

#[test]
fn a_cloid_that_is_not_16_bytes_is_refused_before_auth() {
    refused(&format!("{LIMIT_BUY} --cloid 0xabc"))
        .code(2)
        .stderr(predicate::str::contains("CLOID must be exactly 16 bytes"))
        .stderr(predicate::str::contains("Authentication required").not());
}

#[test]
fn a_zero_builder_address_is_refused_before_auth() {
    refused(&format!(
        "{LIMIT_BUY} --builder 0x0000000000000000000000000000000000000000 --builder-fee-rate 0.01%"
    ))
    .code(13)
    .stdout(predicate::str::is_empty())
    .stderr(predicate::str::contains(
        "builder address cannot be the zero address",
    ));
}

#[test]
fn a_market_slippage_bound_of_zero_is_refused() {
    refused(&format!("{MARKET_BUY} --max-slippage-bps 0"))
        .code(2)
        .stderr(predicate::str::contains("--max-slippage-bps"));
}

#[tokio::test]
async fn an_unknown_coin_exits_13_and_suggests_a_close_match_only_when_there_is_one() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    testnet_command(
        &env,
        &server,
        "orders create --coin BT --side buy --price 50000 --size 0.1",
    )
    .assert()
    .code(13)
    .stderr(predicate::str::contains("Did you mean BTC?"));
    for coin in ["INVALIDCOIN", "ZZZZZZZZ"] {
        testnet_command(
            &env,
            &server,
            &format!("orders create --coin {coin} --side buy --price 50000 --size 0.1"),
        )
        .assert()
        .code(13)
        .stderr(predicate::str::contains(format!(
            "Couldn't find asset \"{coin}\"."
        )))
        .stderr(predicate::str::contains("Did you mean").not());
    }
    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn a_price_that_truncates_to_zero_is_refused_before_anything_is_sent() {
    let env = IsolatedHome::new();
    let server = exchange_resting(1).await;

    env.live_json(&server)
        .args(words(
            "orders create --coin BTC --side buy --price 0.05 --size 0.001",
        ))
        .assert()
        .code(2)
        .stdout(predicate::str::contains("truncates to zero"));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn an_order_without_a_wallet_exits_10_after_validation() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    env.command_with_server(&server)
        .arg("--testnet")
        .args(words(LIMIT_BUY))
        .assert()
        .code(10)
        .stderr(predicate::str::contains("Authentication required"));
}

#[tokio::test]
async fn an_address_only_ows_signer_fails_before_any_exchange_call() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    env.command_with_server(&server)
        .args(["--ows-signer", "0x0000000000000000000000000000000000000001"])
        .args(words(LIMIT_BUY))
        .args(["--yes", "--testnet"])
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "does not have a resolved wallet for live signing",
        ));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn a_limit_order_is_signed_with_its_time_in_force_and_reported_with_its_order_id() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12345).await;

    let receipt = run_json(
        env.live_json(&server)
            .args(words(&format!("{LIMIT_BUY} --tif ioc"))),
    );

    assert_eq!(receipt[0]["order_id"], 12345);
    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["orders"][0]["t"]["limit"]["tif"], "Ioc");

    testnet_command(&env, &server, LIMIT_BUY)
        .assert()
        .success()
        .stdout(predicate::str::contains("Order ID"))
        .stdout(predicate::str::contains("12345"));
}

#[tokio::test]
async fn a_cloid_is_sent_as_c_and_previewed() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12354).await;
    let cloid = "0x1234567890abcdef1234567890abcdef";

    let preview = dry_run(&env, &server, &format!("{LIMIT_BUY} --cloid {cloid}"));
    assert_eq!(preview["args"]["cloid"], cloid);

    run_json(
        env.live_json(&server)
            .args(words(&format!("{LIMIT_BUY} --cloid {cloid}"))),
    );
    assert_eq!(server.exchange_actions().await[0]["orders"][0]["c"], cloid);
}

#[tokio::test]
async fn a_reduce_only_order_is_flagged_in_the_receipt_and_on_the_wire() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12345).await;

    for line in [
        "orders create --coin BTC --side sell --type limit --price 90000 --size 0.001 --reduce-only",
        "orders create --coin BTC --side sell --type market --amount 50 --reduce-only",
    ] {
        let receipt = run_json(env.live_json(&server).args(words(line)));
        assert_eq!(receipt[0]["reduce_only"], true, "{line}");
    }

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 2);
    for action in actions {
        assert_eq!(action["orders"][0]["r"], true);
    }
}

#[tokio::test]
async fn an_outcome_market_is_encoded_into_its_asset_id() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12345).await;
    let line = "orders create --coin #10 --side buy --type limit --price 0.5 --size 1";

    let preview = dry_run(&env, &server, line);
    assert_eq!(preview["args"]["asset_id"], 100_000_010);
    let outcome = &preview["args"]["outcome"];
    assert_eq!(outcome["outcome"], 1);
    assert_eq!(outcome["side"], 0);
    assert_eq!(outcome["encoding"], 10);

    testnet_command(&env, &server, line)
        .assert()
        .success()
        .stdout(predicate::str::contains("#10"));
    assert_eq!(
        server.exchange_actions().await[0]["orders"][0]["a"],
        100_000_010
    );
}

#[tokio::test]
async fn a_trigger_limit_order_keeps_its_limit_and_trigger_prices_distinct() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12353).await;
    let stop = "orders create --coin BTC --side sell --type stop-limit --trigger-price 90000 --price 89500 --size 0.001";
    let take = "orders create --coin BTC --side sell --type take-limit --trigger-price 110000 --price 109500 --size 0.001 --reduce-only";

    let preview = dry_run(&env, &server, stop);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["command"], "orders create");
    assert_eq!(preview["would_execute"], "submit_order");
    let args = &preview["args"];
    assert_eq!(args["type"], "stop-limit");
    assert_eq!(args["asset_id"], 0);
    assert_eq!(args["resolved_asset"], "BTC");
    assert_eq!(args["limit_px"], "89500");
    assert_eq!(args["trigger_px"], "90000");
    assert_eq!(args["is_market"], false);
    assert_eq!(args["tpsl"], "sl");
    assert_eq!(args["reduce_only"], false);

    let args = &dry_run(&env, &server, take)["args"];
    assert_eq!(args["type"], "take-limit");
    assert_eq!(args["limit_px"], "109500");
    assert_eq!(args["trigger_px"], "110000");
    assert_eq!(args["is_market"], false);
    assert_eq!(args["tpsl"], "tp");
    assert_eq!(args["reduce_only"], true);

    run_json(env.live_json(&server).args(words(stop)));
    let order = &server.exchange_actions().await[0]["orders"][0];
    assert_eq!(order["p"], "89500");
    assert_eq!(
        order["t"],
        json!({"trigger": {"isMarket": false, "triggerPx": "90000", "tpsl": "sl"}})
    );
}

#[tokio::test]
async fn legacy_stop_loss_and_take_profit_submit_reduce_only_market_triggers() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12347).await;

    for (order_type, price) in [("stop-loss", "49000"), ("take-profit", "55000")] {
        testnet_command(&env,
        &server,
        &format!("orders create --coin BTC --side sell --price {price} --size 0.1 --type {order_type}"),)
        .assert()
        .success()
        .stdout(predicate::str::contains(order_type))
        .stdout(predicate::str::contains("12347"));
    }

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 2);
    for (action, tpsl, trigger) in [(&actions[0], "sl", "49000"), (&actions[1], "tp", "55000")] {
        let order = &action["orders"][0];
        assert_eq!(order["r"], true);
        assert_eq!(
            order["t"],
            json!({"trigger": {"isMarket": true, "triggerPx": trigger, "tpsl": tpsl}})
        );
    }
}

#[tokio::test]
async fn prices_and_sizes_are_truncated_to_the_markets_precision() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12370).await;

    run_json(env.live_json(&server).args(words(
        "orders create --coin BTC --side buy --price 1234.5678 --size 0.000123",
    )));
    run_json(env.live_json(&server).args(words(
        "orders create --coin BTC --side sell --type stop-limit --price 8950.555 --trigger-price 9000.98765 --size 0.001",
    )));

    let actions = server.exchange_actions().await;
    // BTC has szDecimals 5: prices allow one decimal and five significant
    // figures, sizes allow five decimals.
    assert_eq!(actions[0]["orders"][0]["p"], "1234.5");
    assert_eq!(actions[0]["orders"][0]["s"], "0.00012");
    assert_eq!(actions[1]["orders"][0]["p"], "8950.5");
    assert_eq!(
        actions[1]["orders"][0]["t"]["trigger"]["triggerPx"],
        "9000.9"
    );
}

#[tokio::test]
async fn a_builder_fee_enters_the_signed_action_with_a_lowercased_address() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12361).await;
    let line = format!("{LIMIT_BUY} --builder {BUILDER} --builder-fee-rate 0.01%");

    let args = &dry_run(&env, &server, &line)["args"];
    assert_eq!(args["builder"]["b"], BUILDER.to_lowercase());
    assert_eq!(args["builder"]["f"], 10);
    assert_eq!(args["builder_fee_rate"], "0.01%");

    run_json(env.live_json(&server).args(words(&line)));
    assert_eq!(
        server.exchange_actions().await[0]["builder"],
        json!({"b": BUILDER.to_lowercase(), "f": 10})
    );
}

#[tokio::test]
async fn a_market_order_prices_its_protective_limit_from_the_mid_and_warns() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12346).await;

    testnet_command(
        &env,
        &server,
        &format!("{MARKET_BUY} --max-slippage-bps 250"),
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Order ID"))
    .stdout(predicate::str::contains("12346"))
    .stdout(predicate::str::contains("Slippage warning"))
    .stdout(predicate::str::contains("250 bps"))
    .stdout(predicate::str::contains("51250"));
    run_json(env.live_json(&server).args(words(
        "orders create --coin BTC --side sell --type market --amount 100 --max-slippage-bps 250",
    )));

    let actions = server.exchange_actions().await;
    assert_eq!(actions[0]["orders"][0]["p"], "51250");
    assert_eq!(
        actions[0]["orders"][0]["t"]["limit"]["tif"],
        "FrontendMarket"
    );
    assert_eq!(actions[1]["orders"][0]["p"], "48750");
}

#[tokio::test]
async fn a_market_orders_protective_price_is_rounded_to_the_tick() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12347).await;
    server
        .override_info(
            json!({"type": "allMids"}),
            json!({"BTC": "50000", "ETH": "3000", "TICKY": "94720.33"}),
        )
        .await;
    server
        .override_info(
            json!({"type": "meta"}),
            wire::perp_meta(vec![
                wire::perp_with("BTC", 5, 50, MarginRule::Either),
                wire::perp_with("TICKY", 5, 50, MarginRule::Either),
            ]),
        )
        .await;

    testnet_command(
        &env,
        &server,
        "orders create --coin TICKY --side buy --amount 11 --type market --max-slippage-bps 200",
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("96615"))
    .stdout(predicate::str::contains("96614.7366").not());

    assert_eq!(
        server.exchange_actions().await[0]["orders"][0]["p"],
        "96615"
    );
}

#[tokio::test]
async fn the_slippage_warning_is_yellow_in_pretty_output_only() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12352).await;
    let line = format!("{MARKET_BUY} --max-slippage-bps 250");

    testnet_command(&env, &server, &line)
        .env("FORCE_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\u{1b}[33mSlippage warning: market order uses a 250 bps",
        ))
        .stdout(predicate::str::contains("\u{1b}[33mfalse\u{1b}[0m").not());
    for format in ["table", "json"] {
        testnet_command(&env, &server, &line)
            .env("FORCE_COLOR", "1")
            .args(["--format", format])
            .assert()
            .success()
            .stdout(predicate::str::contains("Slippage warning"))
            .stdout(predicate::str::contains("\u{1b}[").not());
    }
}

#[tokio::test]
async fn a_normal_tpsl_order_reports_each_leg_and_sends_one_grouped_action() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![
            wire::resting_status(221),
            wire::resting_status(222),
            wire::resting_status(223),
        ])
        .await;
    let line = "orders create --coin ETH --side buy --price 2000 --size 0.1 --take-profit 2200 --stop-loss 1900 --grouping normal-tpsl";

    let rows = run_json(env.live_json(&server).args(words(line)));

    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["grouping"], "normal-tpsl");
    assert_eq!(rows[0]["grouping_wire"], "normalTpsl");
    assert_eq!(rows[0]["leg"], "parent");
    assert_eq!(rows[0]["order_id"], 221);
    assert_eq!(rows[1]["leg"], "take_profit");
    assert_eq!(rows[1]["side"], "sell");
    assert_eq!(rows[1]["reduce_only"], true);
    assert_eq!(rows[2]["leg"], "stop_loss");
    assert_eq!(rows[2]["order_id"], 223);

    run_json(env.live_json(&server).args(words(&format!(
        "{line} --builder {BUILDER} --builder-fee-rate 0.01%"
    ))));
    let actions = server.exchange_actions().await;
    assert_eq!(actions[0]["grouping"], "normalTpsl");
    assert_eq!(actions[1]["grouping"], "normalTpsl");
    assert_eq!(actions[1]["orders"].as_array().unwrap().len(), 3);
    assert_eq!(
        actions[1]["builder"],
        json!({"b": BUILDER.to_lowercase(), "f": 10})
    );
}

#[tokio::test]
async fn tpsl_prices_are_truncated_to_the_tick() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![
            wire::resting_status(12372),
            wire::resting_status(12373),
        ])
        .await;

    run_json(env.live_json(&server).args(words(
        "orders tpsl --coin BTC --side sell --size 0.1 --take-profit 6100.98765 --stop-loss 4900.123456",
    )));

    let orders = &server.exchange_actions().await[0]["orders"];
    assert_eq!(orders[0]["t"]["trigger"]["triggerPx"], "6100.9");
    assert_eq!(orders[1]["t"]["trigger"]["triggerPx"], "4900.1");
}

#[tokio::test]
async fn tpsl_reads_and_signs_for_the_acting_account() {
    let env = IsolatedHome::new();
    let subaccount = "0x0000000000000000000000000000000000000123";
    let server = exchange_resting(12354).await;
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState"}),
            wire::clearinghouse_state("1000", 5),
        )
        .await;

    run_json(env.live_json(&server).args(words(&format!(
        "orders tpsl --coin BTC --take-profit 60000 --on-behalf-of {subaccount}"
    ))));

    let reads = server.info_requests("clearinghouseState").await;
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["user"], subaccount);
    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies[0]["vaultAddress"], subaccount);
    assert_eq!(
        bodies[0]["action"]["orders"][0]["t"]["trigger"]["tpsl"],
        "tp"
    );
}

#[tokio::test]
async fn tpsl_whose_position_read_fails_reports_the_cause_not_a_missing_position() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    answer_info(&server, json!({"type": "clearinghouseState"}), 500, "").await;

    testnet_command(
        &env,
        &server,
        "orders tpsl --coin BTC --side sell --size 0.1 --take-profit +10%",
    )
    .assert()
    .code(12)
    .stderr(predicate::str::contains("require an open BTC position").not());

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn an_order_on_behalf_of_a_subaccount_is_signed_with_its_vault_address() {
    let env = IsolatedHome::new();
    let subaccount = "0x0000000000000000000000000000000000000001";
    let server = exchange_resting(12349).await;
    server
        .mount_info_containing(
            json!({"type": "activeAssetData", "coin": "BTC"}),
            json!({
                "user": subaccount, "coin": "BTC",
                "leverage": {"type": "cross", "value": 5},
                "maxTradeSzs": ["1", "1"], "availableToTrade": ["1", "1"], "markPx": "50000"
            }),
        )
        .await;

    run_json(env.live_json(&server).args(words(&format!(
        "{LIMIT_BUY} --margin-mode cross --on-behalf-of {subaccount}"
    ))));

    assert_eq!(
        server.exchange_bodies().await[0]["vaultAddress"],
        subaccount
    );
}

#[tokio::test]
async fn scale_previews_deterministic_legs_and_the_acting_account() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let subaccount = "0x0000000000000000000000000000000000000000";
    let line = "orders scale --coin BTC --side buy --start-price 80000 --end-price 90000 --total-size 0.005 --orders 5";

    let preview = run_json(
        env.command_with_server(&server)
            .args(["--format", "json", "--dry-run", "--testnet"])
            .args(words(line)),
    );
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["command"], "orders scale");
    assert_eq!(preview["would_execute"], "submit_order_batch");
    assert_eq!(preview["args"]["order_count"], 5);
    let orders = preview["args"]["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 5);
    assert_eq!(orders[0]["leg_index"], 0);
    assert_eq!(orders[0]["coin"], "BTC");
    assert_eq!(orders[0]["price"], "80000");
    assert_eq!(orders[0]["size"], "0.001");
    assert_eq!(orders[4]["leg_index"], 4);
    assert_eq!(orders[4]["price"], "90000");
    assert_eq!(orders[4]["size"], "0.001");

    let preview = run_json(
        env.live_command(&server)
            .args(["--format", "json", "--dry-run", "--testnet"])
            .args(words(line))
            .args(["--on-behalf-of", subaccount]),
    );
    assert_eq!(preview["args"]["on_behalf_of"], subaccount);
    assert_eq!(preview["signer"], expected_address(VALID_PRIVATE_KEY));
    assert_eq!(preview["acting_as"], subaccount);
    assert_eq!(preview["vault_address"], subaccount);
}

/// `orders batch-create` of the fixture file on testnet, in `format`. The CLI
/// reads order files relative to its working directory.
fn batch_create(env: &IsolatedHome, server: &ExchangeFixture, format: &str) -> Command {
    let file = stage_fixture(env, "orders_batch_create.json");
    let mut command = env.live_command(server);
    command
        .args(["--format", format, "--testnet"])
        .args(["orders", "batch-create", "--orders-file", &file])
        .current_dir(env.tmp_path());
    command
}

#[tokio::test]
async fn generated_batch_legs_never_consult_the_default_builder_configuration() {
    // A half-configured default builder is a configuration error for orders
    // that can carry a builder fee, but scale and batch-create legs are sent
    // without one, so they must not fail on it.
    let half_configured = "0x1111111111111111111111111111111111111111";
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let batch = format!(
        "orders batch-create --orders-file {}",
        stage_fixture(&env, "orders_batch_create.json")
    );
    let scale = "orders scale --coin BTC --side buy --start-price 80000 --end-price 90000 --total-size 0.005 --orders 5";

    let dry_run_with_half_configured_builder = |line: &str| {
        let mut command = env.command_with_server(&server);
        command
            .env(
                hyperliquid_cli::env::DEFAULT_BUILDER_ADDRESS,
                half_configured,
            )
            .args(["--format", "json", "--dry-run", "--testnet"])
            .args(words(line))
            .current_dir(env.tmp_path());
        command
    };
    for line in [scale, batch.as_str()] {
        dry_run_with_half_configured_builder(line)
            .assert()
            .success();
    }
    dry_run_with_half_configured_builder(LIMIT_BUY)
        .assert()
        .code(2)
        .stdout(predicate::str::contains("both must be configured"));
}

#[tokio::test]
async fn batch_create_previews_the_legs_of_its_file() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let file = stage_fixture(&env, "orders_batch_create.json");

    let preview = run_json(
        env.command_with_server(&server)
            .args(["--format", "json", "--dry-run", "--testnet"])
            .args(["orders", "batch-create", "--orders-file", &file])
            .current_dir(env.tmp_path()),
    );

    assert_eq!(preview["command"], "orders batch-create");
    assert_eq!(preview["would_execute"], "submit_order_batch");
    let orders = preview["args"]["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0]["leg_index"], 0);
    assert_eq!(orders[0]["coin"], "BTC");
    assert_eq!(orders[0]["price"], "50000");
    assert_eq!(orders[0]["size"], "0.001");
    assert_eq!(orders[1]["leg_index"], 1);
    assert_eq!(orders[1]["coin"], "ETH");
    assert_eq!(orders[1]["reduce_only"], true);
    assert_eq!(preview["args"]["orders_file"], file);
    assert!(preview["args"]["on_behalf_of"].is_null());
}

#[test]
fn batch_create_file_errors_name_the_file_and_the_leg_field() {
    let cases: [(&str, &[&str], &[&str]); 2] = [
        (
            "orders_batch_create_bad.json",
            &["orders[0].price is required"],
            &[],
        ),
        (
            "orders_batch_create_multi_bad.json",
            &["orders[1].price", "orders[1].size"],
            &["orders[0]"],
        ),
    ];
    for (name, expected, absent) in cases {
        let env = IsolatedHome::new();
        let file = stage_fixture(&env, name);
        let assert = env
            .command()
            .args(["--dry-run", "--testnet", "orders", "batch-create"])
            .args(["--orders-file", &file])
            .current_dir(env.tmp_path())
            .assert()
            .code(2);
        let stderr = stderr_text(&assert);
        assert!(stderr.contains(&file), "{name}: {stderr}");
        for fragment in expected {
            assert!(stderr.contains(fragment), "{name}: {fragment} in {stderr}");
        }
        for fragment in absent {
            assert!(!stderr.contains(fragment), "{name}: {fragment} in {stderr}");
        }
    }
}

#[tokio::test]
async fn batch_create_sends_one_order_action_with_every_leg() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(331), wire::resting_status(332)])
        .await;

    run_json(&mut batch_create(&env, &server, "json"));

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["grouping"], "na");
    let orders = actions[0]["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0]["a"], 0);
    assert_eq!(orders[0]["b"], true);
    assert_eq!(orders[0]["p"], "50000");
    assert_eq!(orders[0]["s"], "0.001");
    assert_eq!(orders[0]["r"], false);
    assert_eq!(orders[1]["a"], 1);
    assert_eq!(orders[1]["b"], false);
    assert_eq!(orders[1]["p"], "3000");
    assert_eq!(orders[1]["s"], "0.01");
    assert_eq!(orders[1]["r"], true);
}

#[tokio::test]
async fn batch_create_truncates_each_leg_to_its_market_precision() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1), wire::resting_status(2)])
        .await;
    let file = write_orders_file(
        &env,
        "orders.json",
        &json!({"orders": [
            {"coin": "BTC", "side": "buy", "price": "1234.5678", "size": "0.000123"},
            {"coin": "ETH", "side": "sell", "price": "3000.123456", "size": "0.01239"},
        ]}),
    );

    run_json(
        env.live_json(&server)
            .args(["orders", "batch-create", "--orders-file", &file])
            .current_dir(env.tmp_path()),
    );

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    let legs = &actions[0]["orders"];
    // BTC: szDecimals 5 -> one price decimal, five decimals of size.
    assert_eq!(legs[0]["p"], "1234.5");
    assert_eq!(legs[0]["s"], "0.00012");
    // ETH: szDecimals 4 -> five significant figures, four decimals of size.
    assert_eq!(legs[1]["p"], "3000.1");
    assert_eq!(legs[1]["s"], "0.0123");
}

#[tokio::test]
async fn batch_create_refuses_a_leg_that_truncates_to_zero_before_sending() {
    let env = IsolatedHome::new();
    let server = exchange_resting(1).await;
    let file = write_orders_file(
        &env,
        "orders.json",
        &json!({"orders": [{"coin": "BTC", "side": "buy", "price": "50000", "size": "0.000001"}]}),
    );

    env.live_json(&server)
        .args(["orders", "batch-create", "--orders-file", &file])
        .current_dir(env.tmp_path())
        .assert()
        .code(2)
        .stdout(predicate::str::contains("BTC"));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn a_batch_with_one_rejected_leg_prints_every_leg_and_exits_15_with_one_document() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![
            wire::resting_status(331),
            json!({"error": "\u{1b}[31mInsufficient margin\u{1b}[0m\u{202e}\nrun: hyperliquid transfer send --to 0xdead"}),
        ])
        .await;

    let assert = batch_create(&env, &server, "json").assert().code(15);

    // One document: the per-leg result also carries the error envelope keys.
    let document = json_stdout(&assert);
    assert_eq!(document["status"], "partial");
    assert_eq!(document["exit_code"], 15);
    assert_eq!(document["category"], "partial_results");
    assert_eq!(document["reason"], "partial_failure");
    assert_eq!(document["atomic"], false);
    assert_eq!(document["succeeded"], 1);
    assert_eq!(document["failed"], 1);
    assert_eq!(document["error"], "Placed 1 of 2 orders. 1 failed.");
    assert_eq!(document["next"][0]["command"], "hyperliquid orders open");
    assert!(document.get("next_command").is_none());
    let legs = document["orders"].as_array().unwrap();
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0]["status"], "resting");
    assert_eq!(legs[0]["order_id"], 331);
    assert!(legs[0].get("error").is_none());
    assert_eq!(legs[1]["status"], "rejected");
    let leg_error = legs[1]["error"].as_str().unwrap();
    assert!(leg_error.starts_with("[untrusted remote data] "));
    assert!(leg_error.contains("Insufficient margin"));
    assert!(!leg_error.contains('\u{1b}'));
    assert!(!leg_error.contains('\u{202e}'));
    assert!(!leg_error.contains('\n'));
}

#[tokio::test]
async fn a_partial_batch_names_its_recovery_command_on_stderr() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![
            wire::resting_status(331),
            json!({"error": "Insufficient margin"}),
        ])
        .await;

    batch_create(&env, &server, "table")
        .assert()
        .code(15)
        .stdout(predicate::str::contains("resting"))
        .stdout(predicate::str::contains("rejected"))
        .stdout(predicate::str::contains("Insufficient margin"))
        .stderr(predicate::str::contains("Placed 1 of 2 orders. 1 failed."))
        .stderr(predicate::str::contains("Check what is live:"))
        .stderr(predicate::str::contains("    hyperliquid orders open"));
}

#[tokio::test]
async fn a_batch_rejected_on_every_leg_is_a_labelled_error() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![
            json!({"error": "first problem"}),
            json!({"error": "second problem"}),
        ])
        .await;

    batch_create(&env, &server, "table")
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "[untrusted remote data] first problem",
        ))
        .stderr(predicate::str::contains(
            "[untrusted remote data] second problem",
        ));
}

#[tokio::test]
async fn an_answer_with_the_wrong_number_of_statuses_is_an_error_naming_the_mismatch() {
    let env = IsolatedHome::new();

    let none = ExchangeFixture::start().await;
    none.mount_order_statuses(vec![]).await;
    let assert = env.live_json(&none).args(words(LIMIT_BUY)).assert().code(1);
    let document = json_stdout(&assert);
    let error = error_text(&document);
    assert!(
        error.contains("The exchange returned 0 statuses for 1 order"),
        "{error}"
    );
    assert!(error.contains("hyperliquid orders open"), "{error}");
    assert_eq!(document["category"], "internal");

    let two = ExchangeFixture::start().await;
    two.mount_order_statuses(vec![wire::resting_status(1), wire::resting_status(2)])
        .await;
    env.live_json(&two)
        .args(words(LIMIT_BUY))
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "The exchange returned 2 statuses for 1 order",
        ));

    let short_batch = exchange_resting(331).await;
    batch_create(&env, &short_batch, "json")
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "The exchange returned 1 status for 2 orders",
        ));
}

#[tokio::test]
async fn a_rejected_order_names_what_the_exchange_said() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![json!({"error": "Order has invalid size."})])
        .await;

    let assert = env
        .live_json(&server)
        .args(words(LIMIT_BUY))
        .assert()
        .code(13);

    assert_eq!(
        json_stdout(&assert)["error"],
        "Unsupported input: Couldn't place order. Exchange said: [untrusted remote data] Order has invalid size."
    );
}

#[tokio::test]
async fn mainnet_asks_for_confirmation_and_abandons_on_no() {
    let env = IsolatedHome::new();
    let server = exchange_resting(1).await;

    mainnet_command(&env, &server, LIMIT_BUY)
        .write_stdin("n\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "Mainnet order confirmation required",
        ))
        .stderr(predicate::str::contains("not reduce-only"))
        .stderr(predicate::str::contains("Slippage warning").not())
        .stderr(predicate::str::contains("order placement cancelled"));
    mainnet_command(&env, &server, &format!("{LIMIT_BUY} --reduce-only"))
        .write_stdin("n\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("reduce-only"))
        .stderr(predicate::str::contains("not reduce-only").not());
    mainnet_command(&env,
    &server,
    "orders create --coin BTC --side sell --type stop-limit --trigger-price 90000 --price 89500 --size 0.001",)
    .write_stdin("n\n")
    .assert()
    .code(2)
    .stderr(predicate::str::contains("trigger price 90000"))
    .stderr(predicate::str::contains("at price 89500"));
    mainnet_command(
        &env,
        &server,
        &format!("{MARKET_BUY} --max-slippage-bps 250"),
    )
    .write_stdin("n\n")
    .assert()
    .code(2)
    .stderr(predicate::str::contains("Slippage warning"))
    .stderr(predicate::str::contains("250 bps"))
    .stderr(predicate::str::contains("order placement cancelled"));

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn mainnet_places_the_order_once_the_prompt_is_accepted_or_skipped() {
    let env = IsolatedHome::new();
    let server = exchange_resting(12350).await;

    mainnet_command(&env, &server, LIMIT_BUY)
        .write_stdin("yes\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("12350"))
        .stderr(predicate::str::contains("Network: mainnet"));
    mainnet_command(&env, &server, &format!("{LIMIT_BUY} --yes"))
        .assert()
        .success()
        .stderr(predicate::str::contains("Mainnet order confirmation required").not());
    testnet_command(&env, &server, LIMIT_BUY)
        .assert()
        .success()
        .stderr(predicate::str::contains("Mainnet order confirmation required").not());

    assert_eq!(server.exchange_actions().await.len(), 3);
}

#[tokio::test]
async fn mainnet_json_without_yes_is_an_error_not_a_prompt() {
    let env = IsolatedHome::new();
    let server = exchange_resting(1).await;

    mainnet_command(&env, &server, LIMIT_BUY)
        .args(["--format", "json"])
        .write_stdin("n\n")
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "without confirmation in non-interactive mode",
        ))
        .stdout(predicate::str::contains("\"error\""))
        .stderr(predicate::str::is_empty());

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn buy_previews_the_same_order_as_a_market_create() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let preview = |line: &str| {
        run_json(
            env.command_with_server(&server)
                .args(["--format", "json", "--dry-run", "--testnet"])
                .args(words(line))
                .args(["--coin", "BTC", "--size", "0.001"]),
        )
    };

    let buy = preview("buy");
    let create = preview("orders create --side buy --type market");

    assert_eq!(buy["command"], "buy");
    assert_eq!(buy["would_execute"], create["would_execute"]);
    assert_eq!(buy["args"], create["args"]);
    assert_eq!(buy["args"]["side"], "buy");
    assert_eq!(buy["args"]["type"], "market");
    assert_eq!(buy["args"]["size"], "0.001");
    assert_eq!(buy["args"]["asset_id"], 0);
    assert_eq!(buy["args"]["resolved_asset"], "BTC");
}

#[tokio::test]
async fn buy_without_a_size_or_amount_suggests_a_command_that_runs() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let assert = env
        .live_json(&server)
        .env("HYPERLIQUID_AGENT", "1")
        .args(words("--dry-run buy --coin BTC"))
        .assert()
        .code(2);
    let error = json_stdout(&assert);
    let message = error_text(&error);
    assert!(
        message.contains("--size") || message.contains("--amount"),
        "{message}"
    );

    let suggestion = suggested_invocation(message);
    let preview = run_json(env.command_with_server(&server).args(&suggestion));
    assert_eq!(preview["command"], "buy");
    assert_eq!(preview["args"]["resolved_asset"], "BTC");
}

#[tokio::test]
async fn the_outcome_shortcuts_preview_under_their_own_command_name() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), wire::outcome_meta(&[10]))
        .await;

    for (command, side) in [("outcome-buy", "buy"), ("outcome-sell", "sell")] {
        let preview = dry_run(
            &env,
            &server,
            &format!("{command} --coin #10 --price 0.5 --size 1"),
        );

        assert_eq!(preview["command"], command);
        assert_eq!(preview["would_execute"], "submit_order");
        assert_eq!(preview["args"]["side"], side);
        assert_eq!(preview["args"]["outcome"]["asset_id"], 100_000_010);
    }
}

#[test]
fn the_outcome_shortcuts_refuse_a_market_that_is_not_an_outcome() {
    for command in ["outcome-buy", "outcome-sell"] {
        refused(&format!(
            "--dry-run {command} --coin BTC --price 50000 --size 1"
        ))
        .code(13)
        .stderr(predicate::str::contains("invalid outcome notation 'BTC'"));
    }
}
