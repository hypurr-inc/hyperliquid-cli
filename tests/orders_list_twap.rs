//! `orders open`, the TWAP commands and `orders schedule-cancel`: listing in
//! each output format, the signed action each sends, and the mainnet prompt.

mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::orders::{assert_nothing_sent, error_text, mainnet_command, testnet_command, words};
use support::{IsolatedHome, json_stdout, run_json, stderr_text, unix_millis, wire};

const TWAP: &str = "orders twap-create --coin BTC --side buy --size 1.0 --duration 3600";
const SUBACCOUNT: &str = "0x0000000000000000000000000000000000000123";

fn twap_reply(status: Value) -> Value {
    json!({"status": "ok", "response": {"type": "twapOrder", "data": {"status": status}}})
}

fn twap_running(twap_id: u64) -> Value {
    twap_reply(json!({"running": {"twapId": twap_id}}))
}

fn default_reply() -> Value {
    json!({"status": "ok", "response": {"type": "default"}})
}

async fn exchange_answering(reply: Value) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server.mount_exchange(reply).await;
    server
}

async fn exchange_listing(open: Vec<Value>) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(json!({"type": "frontendOpenOrders"}), Value::Array(open))
        .await;
    server
}

#[tokio::test]
async fn an_empty_open_orders_list_uses_theme_colors_in_pretty_output() {
    let env = IsolatedHome::new();
    let server = exchange_listing(vec![]).await;

    testnet_command(&env, &server, "orders open")
        .env("FORCE_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[36mCoin\u{1b}[0m"))
        .stdout(predicate::str::contains("No open orders").not())
        .stderr(predicate::str::contains("No open orders."));
}

#[tokio::test]
async fn an_empty_open_orders_list_is_uncolored_in_table_and_json() {
    let env = IsolatedHome::new();
    let server = exchange_listing(vec![]).await;

    testnet_command(&env, &server, "orders open")
        .args(["--format", "table"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Coin"))
        .stdout(predicate::str::contains("No open orders").not())
        .stdout(predicate::str::contains("\u{1b}[").not())
        .stderr(predicate::str::contains("No open orders."));

    let rows = run_json(env.live_json(&server).args(["orders", "open"]));
    assert_eq!(rows, json!([]));
}

#[tokio::test]
async fn open_orders_are_listed_and_select_narrows_the_json_columns() {
    let env = IsolatedHome::new();
    let server = exchange_listing(vec![wire::basic_order("BTC", 12345)]).await;

    testnet_command(&env, &server, "orders open")
        .assert()
        .success()
        .stdout(predicate::str::contains("BTC"))
        .stdout(predicate::str::contains("12345"));

    let rows =
        run_json(
            env.live_json(&server)
                .args(["--select", "coin,price,size", "orders", "open"]),
        );
    let first = rows[0].as_object().unwrap();
    assert_eq!(
        first.keys().map(String::as_str).collect::<Vec<_>>(),
        ["coin", "price", "size"]
    );
    assert_eq!(first["coin"], "BTC");
    assert_eq!(first["price"], "50000");
    assert_eq!(first["size"], "0.1");
}

#[tokio::test]
async fn a_twap_is_signed_with_its_asset_side_minutes_and_randomize_flag() {
    let env = IsolatedHome::new();
    let server = exchange_answering(twap_running(77738308)).await;

    testnet_command(&env, &server, TWAP)
        .assert()
        .success()
        .stdout(predicate::str::contains("77738308"))
        .stdout(predicate::str::contains("running"))
        .stderr(predicate::str::contains("Mainnet TWAP confirmation required").not());
    run_json(
        env.live_json(&server)
            .args(words(&format!("{TWAP} --randomize"))),
    );

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["type"], "twapOrder");
    assert_eq!(actions[0]["twap"]["a"], 0);
    assert_eq!(actions[0]["twap"]["b"], true);
    assert_eq!(actions[0]["twap"]["m"], 60);
    assert_eq!(actions[0]["twap"]["t"], false);
    assert_eq!(actions[1]["twap"]["t"], true);
}

#[tokio::test]
async fn a_twap_size_is_normalized_and_its_units_reported() {
    let env = IsolatedHome::new();
    let server = exchange_answering(twap_running(77738311)).await;

    testnet_command(
        &env,
        &server,
        "orders twap-create --coin BTC --side buy --size 0.00070 --duration 300",
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("77738311"))
    .stdout(predicate::str::contains("300"))
    .stdout(predicate::str::contains("5"));

    let twap = &server.exchange_actions().await[0]["twap"];
    assert_eq!(twap["s"], "0.0007");
    assert_eq!(twap["m"], 5);
}

#[tokio::test]
async fn unsupported_twap_durations_are_refused_before_submission() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    for (duration, message) in [
        ("299", "TWAP duration must be at least 300 seconds"),
        (
            "301",
            "TWAP duration must be a whole-minute value in seconds",
        ),
        ("604860", "cannot exceed 7 days"),
    ] {
        testnet_command(
            &env,
            &server,
            &format!("orders twap-create --coin BTC --side buy --size 1.0 --duration {duration}"),
        )
        .assert()
        .code(2)
        .stderr(predicate::str::contains(message));
    }

    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn a_mainnet_twap_asks_first_unless_yes_is_given() {
    let env = IsolatedHome::new();
    let server = exchange_answering(twap_running(77738309)).await;

    mainnet_command(&env, &server, TWAP)
        .write_stdin("n\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "Mainnet TWAP confirmation required",
        ))
        .stderr(predicate::str::contains("BTC"))
        .stderr(predicate::str::contains("buy"))
        .stderr(predicate::str::contains("1.0"))
        .stderr(predicate::str::contains("3600"))
        .stderr(predicate::str::contains("mainnet"))
        .stderr(predicate::str::contains("TWAP creation cancelled"));
    assert_nothing_sent(&server).await;

    mainnet_command(&env, &server, &format!("{TWAP} --yes"))
        .assert()
        .success()
        .stdout(predicate::str::contains("77738309"))
        .stderr(predicate::str::contains("Mainnet TWAP confirmation required").not());
    assert_eq!(server.exchange_actions().await.len(), 1);
}

#[test]
fn twap_commands_name_the_arguments_they_require() {
    let cases = [
        ("orders twap-cancel 42", &["--coin", "required"][..]),
        (
            "orders twap-create --coin BTC --side buy",
            &["--size", "--duration"][..],
        ),
    ];
    for (line, expected) in cases {
        let assert = IsolatedHome::new()
            .command()
            .arg("--testnet")
            .args(words(line))
            .assert()
            .code(2);
        let stderr = stderr_text(&assert);
        for fragment in expected {
            assert!(stderr.contains(fragment), "{line}: {fragment} in {stderr}");
        }
    }
}

#[tokio::test]
async fn cancelling_a_twap_names_its_coins_asset_and_id() {
    let env = IsolatedHome::new();
    let server = exchange_answering(
        json!({"status": "ok", "response": {"type": "twapCancel", "data": {"status": "success"}}}),
    )
    .await;

    testnet_command(&env, &server, "orders twap-cancel 42 --coin ETH")
        .assert()
        .success()
        .stdout(predicate::str::contains("ETH"))
        .stdout(predicate::str::contains("42"))
        .stdout(predicate::str::contains("cancelled"));

    assert_eq!(
        server.exchange_actions().await,
        [json!({"type": "twapCancel", "a": 1, "t": 42})]
    );
}

#[tokio::test]
async fn schedule_cancel_on_mainnet_asks_first_and_places_with_yes() {
    let env = IsolatedHome::new();
    let server = exchange_answering(default_reply()).await;

    for flags in ["--in 5m", "--clear"] {
        mainnet_command(&env, &server, &format!("orders schedule-cancel {flags}"))
            .write_stdin("n\n")
            .assert()
            .code(2)
            .stderr(predicate::str::contains(
                "Mainnet schedule-cancel confirmation required",
            ))
            .stderr(predicate::str::contains("action cancelled"));
    }
    assert_nothing_sent(&server).await;

    mainnet_command(&env, &server, "orders schedule-cancel --in 5m --yes")
        .assert()
        .success()
        .stderr(predicate::str::contains("Mainnet schedule-cancel confirmation required").not());
    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["type"], "scheduleCancel");
}

#[test]
fn schedule_cancel_needs_yes_without_an_interactive_terminal() {
    let env = IsolatedHome::new();

    for (agent, flags) in [(false, "--in 5m"), (true, "--clear")] {
        let mut command = env.command();
        if agent {
            command.env("HYPERLIQUID_AGENT", "1");
        }
        let assert = command
            .args(["--format", "json", "orders", "schedule-cancel"])
            .args(words(flags))
            .assert()
            .code(13);
        let document = json_stdout(&assert);
        assert_eq!(document["category"], "unsupported", "{flags}");
        assert_eq!(document["exit_code"], 13, "{flags}");
        assert!(
            error_text(&document).contains("without confirmation in non-interactive mode"),
            "{flags}"
        );
    }
}

#[tokio::test]
async fn schedule_cancel_sends_the_time_five_minutes_ahead_or_clears_it() {
    let env = IsolatedHome::new();
    let server = exchange_answering(default_reply()).await;

    let before = unix_millis();
    testnet_command(&env, &server, "orders schedule-cancel --in 5m")
        .assert()
        .success()
        .stdout(predicate::str::contains("scheduled"))
        .stdout(predicate::str::contains("Scheduled Time"));
    let after = unix_millis();
    testnet_command(&env, &server, "orders schedule-cancel --clear")
        .assert()
        .success()
        .stdout(predicate::str::contains("cleared"));

    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["action"]["type"], "scheduleCancel");
    let time = bodies[0]["action"]["time"]
        .as_u64()
        .expect("scheduled time in milliseconds");
    assert!(
        (before + 299_000..=after + 301_000).contains(&time),
        "{time} is not five minutes after the request"
    );
    assert!(bodies[0]["vaultAddress"].is_null());
    assert_eq!(bodies[1]["action"]["type"], "scheduleCancel");
    assert!(bodies[1]["action"]["time"].is_null());
}

#[tokio::test]
async fn schedule_cancel_on_behalf_of_a_subaccount_sets_the_vault_address() {
    let env = IsolatedHome::new();
    let server = exchange_answering(default_reply()).await;

    for (flags, clears) in [("--in 5m", false), ("--clear", true)] {
        run_json(env.live_json(&server).args(words(&format!(
            "orders schedule-cancel {flags} --on-behalf-of {SUBACCOUNT}"
        ))));

        let body = server.exchange_bodies().await.pop().unwrap();
        assert_eq!(body["vaultAddress"], SUBACCOUNT, "{flags}");
        assert_eq!(body["action"]["time"].is_null(), clears, "{flags}");
    }
}
