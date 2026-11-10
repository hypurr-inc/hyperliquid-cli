mod support;

use serde_json::json;
use support::exchange::ExchangeFixture;
use support::reads::read_json;
use support::{IsolatedHome, json_stdout, mount_account_state, wire};

const USER: &str = "0x0000000000000000000000000000000000000000";
const START_MS: u64 = 1_777_593_600_000;
const END_MS: u64 = 1_777_680_000_000;

/// `account fills` and its siblings reading `[START_MS, END_MS]`.
const WINDOW: [&str; 4] = ["--start", "1777593600000", "--end", "1777680000000"];

#[tokio::test]
async fn account_fees_and_portfolio_history_are_snake_case() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "userFees", "user": USER}),
            json!({
                "dailyUserVlm": "123.45",
                "feeSchedule": {"cross": "0.00035", "add": "0.0001"}
            }),
        )
        .await;
    server
        .mount_info(
            json!({"type": "portfolio", "user": USER}),
            json!({
                "day": {
                    "accountValueHistory": [[START_MS, "1000"]],
                    "pnlHistory": [[START_MS, "12.5"]],
                    "vlm": "50"
                },
                "allTime": {
                    "accountValueHistory": [[START_MS, "900"], [END_MS, "1000"]]
                },
                "perpDayVlm": "25"
            }),
        )
        .await;

    let fees = read_json(&server, &["account", "fees", USER]);
    assert_eq!(fees["daily_user_vlm"], "123.45");
    assert_eq!(fees["fee_schedule"]["cross"], "0.00035");

    let portfolio = read_json(&server, &["account", "portfolio-history", USER]);
    assert!(portfolio.get("all_time").is_some());
    assert_eq!(portfolio["day"]["account_value_history"][0][1], "1000");
    assert_eq!(portfolio["perp_day_vlm"], "25");
}

#[tokio::test]
async fn account_fills_by_time_parses_rfc3339_and_projects_json() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({
                "type": "userFillsByTime",
                "user": USER,
                "startTime": START_MS,
                "endTime": END_MS,
                "aggregateByTime": true
            }),
            json!([wire::fill("BTC", 4242)]),
        )
        .await;

    let fills = read_json(
        &server,
        &[
            "--select",
            "coin,time",
            "account",
            "fills",
            USER,
            "--start",
            "2026-05-01T00:00:00Z",
            "--end",
            "2026-05-02T00:00:00Z",
            "--aggregate-by-time",
        ],
    );

    assert_eq!(
        fills,
        json!([{"coin": "BTC", "time": 1_700_000_000_000_u64}])
    );
}

#[test]
fn account_fills_aggregate_by_time_requires_start() {
    let assert = IsolatedHome::new()
        .command()
        .args(["--format", "json", "account", "fills", USER])
        .arg("--aggregate-by-time")
        .assert()
        .code(2);
    let usage = json_stdout(&assert);

    assert_eq!(usage["category"], "usage");
    assert_eq!(usage["exit_code"], 2);
    assert!(usage["error"].as_str().unwrap().contains("--start"));
}

#[tokio::test]
async fn ledger_funding_and_twap_payloads_are_time_bounded() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({
                "type": "userNonFundingLedgerUpdates",
                "user": USER,
                "startTime": START_MS,
                "endTime": END_MS
            }),
            json!([{
                "time": START_MS,
                "hash": "0xabc",
                "delta": {"type": "deposit", "amount": "100"}
            }]),
        )
        .await;
    server
        .mount_info(
            json!({
                "type": "userFunding",
                "user": USER,
                "startTime": START_MS,
                "endTime": END_MS
            }),
            json!([{
                "time": START_MS,
                "coin": "BTC",
                "usdc": "1.25",
                "szi": "0.5",
                "fundingRate": "0.0001"
            }]),
        )
        .await;
    server
        .mount_info(
            json!({"type": "twapHistory", "user": USER}),
            json!([{"state": {"coin": "BTC", "twapId": 55}, "status": {"status": "finished"}}]),
        )
        .await;
    server
        .mount_info(
            json!({
                "type": "userTwapSliceFillsByTime",
                "user": USER,
                "startTime": START_MS,
                "endTime": END_MS,
                "aggregateByTime": true
            }),
            json!([{"twapId": 55, "fill": wire::fill("BTC", 4242)}]),
        )
        .await;

    let ledger = read_json(
        &server,
        &[&["account", "ledger", USER][..], &WINDOW].concat(),
    );
    assert_eq!(ledger[0]["delta"]["type"], "deposit");
    assert_eq!(ledger[0]["delta"]["amount"], "100");

    let funding = read_json(
        &server,
        &[&["account", "funding", USER][..], &WINDOW].concat(),
    );
    assert_eq!(funding[0]["funding_rate"], "0.0001");

    let twap_history = read_json(&server, &["account", "twap-history", USER]);
    assert_eq!(twap_history[0]["state"]["twap_id"], 55);

    let twap_fills = read_json(
        &server,
        &[
            &["account", "twap-fills", USER][..],
            &WINDOW,
            &["--aggregate-by-time"],
        ]
        .concat(),
    );
    assert_eq!(twap_fills[0]["twap_id"], 55);
    assert_eq!(twap_fills[0]["fill"]["coin"], "BTC");
}

#[tokio::test]
async fn order_status_uses_the_public_order_status_payload() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "orderStatus", "user": USER, "oid": 123}),
            json!({
                "status": "order",
                "order": {
                    "status": "filled",
                    "statusTimestamp": 1_700_000_000_001_u64,
                    "order": {"coin": "BTC", "oid": 123}
                }
            }),
        )
        .await;

    let status = read_json(
        &server,
        &["orders", "status", "--user", USER, "--oid", "123"],
    );

    assert_eq!(status["order"]["status"], "filled");
    assert_eq!(status["order"]["status_timestamp"], 1_700_000_000_001_u64);
}

#[tokio::test]
async fn order_status_normalizes_a_cloid_to_32_hex_digits() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({
                "type": "orderStatus",
                "user": USER,
                "oid": "0x00000000000000000000000000000abc"
            }),
            json!({
                "status": "order",
                "order": {
                    "status": "open",
                    "statusTimestamp": 1_700_000_000_002_u64,
                    "order": {"coin": "BTC", "oid": 124}
                }
            }),
        )
        .await;

    let status = read_json(
        &server,
        &["orders", "status", "--user", USER, "--cloid", "0xabc"],
    );

    assert_eq!(status["order"]["status"], "open");
    assert_eq!(status["order"]["status_timestamp"], 1_700_000_000_002_u64);
}

#[tokio::test]
async fn staking_history_is_exposed() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "delegatorHistory", "user": USER}),
            json!([{
                "time": START_MS,
                "hash": "0xdef",
                "delta": {
                    "type": "delegate",
                    "validator": "0x1111111111111111111111111111111111111111",
                    "amount": "250000000"
                }
            }]),
        )
        .await;

    let history = read_json(&server, &["staking", "history", USER]);

    assert_eq!(history[0]["delta"]["type"], "delegate");
    assert_eq!(history[0]["delta"]["amount"], "250000000");
}

#[tokio::test]
async fn account_fills_json_exposes_public_fill_fields() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "userFills", "user": USER}),
            json!([wire::fill("BTC", 4242)]),
        )
        .await;

    let fills = read_json(&server, &["account", "fills", USER]);

    let fill = &fills[0];
    assert_eq!(fill["coin"], "BTC");
    assert_eq!(fill["price"], "50000");
    assert_eq!(fill["size"], "0.1");
    assert_eq!(fill["direction"], "Open Long");
    assert_eq!(fill["closed_pnl"], "0");
    assert_eq!(fill["fee"], "0.01");
    assert_eq!(fill["fee_token"], "USDC");
    assert_eq!(fill["oid"], 4242);
    assert_eq!(fill["time"], 1_700_000_000_000_u64);
}

#[tokio::test]
async fn account_fills_with_no_rows_print_an_empty_json_array() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "userFills", "user": USER}), json!([]))
        .await;

    assert_eq!(read_json(&server, &["account", "fills", USER]), json!([]));
}

fn stdout_with_color(server: &ExchangeFixture, format: &str, args: &[&str]) -> String {
    let assert = IsolatedHome::new()
        .command_with_server(server)
        .env("FORCE_COLOR", "1")
        .args(["--format", format])
        .args(args)
        .assert()
        .success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

#[tokio::test]
async fn account_fills_colour_closed_pnl_only_in_pretty_output() {
    let server = ExchangeFixture::start().await;
    let fill = |oid: u64, closed_pnl: &str| {
        let mut fill = wire::fill("BTC", oid);
        fill["closedPnl"] = closed_pnl.into();
        fill
    };
    server
        .mount_info(
            json!({"type": "userFills", "user": USER}),
            json!([fill(1, "12.34"), fill(2, "-5.67"), fill(3, "0")]),
        )
        .await;

    let pretty = stdout_with_color(&server, "pretty", &["account", "fills", USER]);
    assert!(pretty.contains("\u{1b}[32m12.34\u{1b}[0m"), "{pretty}");
    assert!(pretty.contains("\u{1b}[31m-5.67\u{1b}[0m"), "{pretty}");
    assert!(!pretty.contains("\u{1b}[32m0\u{1b}[0m"), "{pretty}");
    assert!(!pretty.contains("\u{1b}[31m0\u{1b}[0m"), "{pretty}");

    for format in ["table", "json"] {
        let plain = stdout_with_color(&server, format, &["account", "fills", USER]);
        assert!(plain.contains("12.34"), "{format}: {plain}");
        assert!(!plain.contains("\u{1b}["), "{format}: {plain}");
    }
}

#[tokio::test]
async fn account_portfolio_colours_position_pnl_only_in_pretty_output() {
    let server = ExchangeFixture::start().await;
    mount_account_state(&server, vec![], vec![], "1000", "1000", 5).await;

    let pretty = stdout_with_color(&server, "pretty", &["account", "portfolio", USER]);
    assert!(pretty.contains("pnl=\u{1b}[32m100\u{1b}[0m"), "{pretty}");

    let table = stdout_with_color(&server, "table", &["account", "portfolio", USER]);
    assert!(table.contains("pnl=100"), "{table}");
    assert!(!table.contains("\u{1b}["), "{table}");
    let json = stdout_with_color(&server, "json", &["account", "portfolio", USER]);
    assert!(!json.contains("\u{1b}["));
}
