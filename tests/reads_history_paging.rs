//! History reads the API cuts off at a fixed page size: the cut-off is
//! announced on stderr (naming the `--start` that continues), `--all` reads
//! every page, and fill rows carry what the API sends.

mod support;

use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, wire};

const USER: &str = "0x0000000000000000000000000000000000000001";
const START: u64 = 1_700_000_000_000;

fn run(server: &ExchangeFixture, format: &str, args: &[&str]) -> assert_cmd::assert::Assert {
    let env = IsolatedHome::new();
    env.command_with_server(server)
        .args(["--format", format])
        .args(args)
        .assert()
}

fn fill_at(time: u64) -> Value {
    let mut fill = wire::fill("BTC", 4242);
    fill["time"] = json!(time);
    fill["tid"] = json!(time);
    fill
}

/// `count` fills at consecutive milliseconds from `from`.
fn fills(from: u64, count: u64) -> Vec<Value> {
    (from..from + count).map(fill_at).collect()
}

fn fills_request(start: u64) -> Value {
    json!({"type": "userFillsByTime", "user": USER, "startTime": start})
}

#[tokio::test]
async fn a_full_fills_page_is_announced_with_the_start_that_continues() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(fills_request(START), json!(fills(START, 2000)))
        .await;

    let assert = run(
        &server,
        "json",
        &["account", "fills", USER, "--start", &START.to_string()],
    )
    .success();

    let rows = support::json_stdout(&assert);
    assert_eq!(
        rows.as_array().unwrap().len(),
        2000,
        "stdout stays the array"
    );
    let warning: Value = serde_json::from_str(support::stderr_text(&assert).trim()).unwrap();
    assert_eq!(warning["warning"]["code"], "truncated");
    assert_eq!(warning["warning"]["returned"], 2000);
    assert_eq!(warning["warning"]["cap"], 2000);
    assert_eq!(warning["warning"]["next_start"], START + 1999);
}

#[tokio::test]
async fn a_full_fills_page_in_a_human_format_says_so_in_words() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(fills_request(START), json!(fills(START, 2000)))
        .await;

    let assert = run(
        &server,
        "table",
        &["account", "fills", USER, "--start", &START.to_string()],
    )
    .success();

    let stderr = support::stderr_text(&assert);
    assert!(
        stderr.contains(&format!("Continue with --start {}", START + 1999)),
        "{stderr}"
    );
    assert!(stderr.contains("--all"), "{stderr}");
}

#[tokio::test]
async fn a_short_fills_page_is_not_announced() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(fills_request(START), json!(fills(START, 1999)))
        .await;

    let assert = run(
        &server,
        "json",
        &["account", "fills", USER, "--start", &START.to_string()],
    )
    .success();

    assert_eq!(support::stderr_text(&assert), "");
}

#[tokio::test]
async fn all_reads_every_page_and_drops_the_rows_the_boundary_repeats() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(fills_request(START), json!(fills(START, 2000)))
        .await;
    // The next page starts at the last timestamp read, so it repeats that row.
    server
        .mount_info(
            fills_request(START + 1999),
            json!([fill_at(START + 1999), fill_at(START + 2000)]),
        )
        .await;

    let assert = run(
        &server,
        "json",
        &[
            "account",
            "fills",
            USER,
            "--start",
            &START.to_string(),
            "--all",
        ],
    )
    .success();

    let rows = support::json_stdout(&assert);
    let times: Vec<u64> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["time"].as_u64().unwrap())
        .collect();
    assert_eq!(times.len(), 2001);
    assert_eq!(times.last(), Some(&(START + 2000)));
    assert_eq!(
        times.iter().filter(|time| **time == START + 1999).count(),
        1,
        "the repeated boundary row is dropped"
    );
    assert_eq!(support::stderr_text(&assert), "");
    assert_eq!(server.info_requests("userFillsByTime").await.len(), 2);
}

#[tokio::test]
async fn all_steps_past_a_millisecond_the_api_keeps_repeating() {
    let server = ExchangeFixture::start().await;
    // A full page that is entirely one millisecond: asking again from the same
    // start would never advance.
    let page = |tid_from: u64| -> Vec<Value> {
        (tid_from..tid_from + 2000)
            .map(|tid| {
                let mut fill = fill_at(START);
                fill["tid"] = json!(tid);
                fill
            })
            .collect()
    };
    server
        .mount_info(fills_request(START), json!(page(0)))
        .await;
    server
        .mount_info(fills_request(START + 1), json!([fill_at(START + 1)]))
        .await;

    let assert = run(
        &server,
        "json",
        &[
            "account",
            "fills",
            USER,
            "--start",
            &START.to_string(),
            "--all",
        ],
    )
    .success();

    // The same page comes back for the repeated start; it adds nothing, so the
    // reader moves one millisecond on instead of looping.
    assert_eq!(
        support::json_stdout(&assert).as_array().unwrap().len(),
        2001
    );
}

#[tokio::test]
async fn recent_fills_at_the_cap_say_earlier_ones_are_not_shown() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "userFills", "user": USER}),
            json!(fills(START, 2000)),
        )
        .await;

    let assert = run(&server, "json", &["account", "fills", USER]).success();

    let warning: Value = serde_json::from_str(support::stderr_text(&assert).trim()).unwrap();
    assert_eq!(warning["warning"]["code"], "truncated");
    assert_eq!(warning["warning"]["next_start"], Value::Null);
    assert!(
        warning["warning"]["message"]
            .as_str()
            .unwrap()
            .contains("Pass --start")
    );
}

#[tokio::test]
async fn fills_carry_hash_builder_fee_liquidation_and_twap_and_survive_a_new_direction() {
    let server = ExchangeFixture::start().await;
    let mut liquidation = fill_at(START);
    liquidation["hash"] = json!("0xdeadbeef");
    liquidation["dir"] = json!("A Direction The SDK Has Never Heard Of");
    liquidation["builderFee"] = json!("0.0125");
    liquidation["twapId"] = json!(77);
    liquidation["liquidation"] =
        json!({"liquidatedUser": USER, "markPx": "74999.0", "method": "market"});
    server
        .mount_info(
            json!({"type": "userFills", "user": USER}),
            json!([fill_at(START + 1), liquidation]),
        )
        .await;

    let rows = support::json_stdout(&run(&server, "json", &["account", "fills", USER]).success());

    let plain = &rows[0];
    assert_eq!(plain["hash"], "0xabc");
    for absent in ["builder_fee", "liquidation", "twap_id"] {
        assert!(plain.get(absent).is_none(), "{absent} only when present");
    }
    let rich = &rows[1];
    assert_eq!(rich["direction"], "A Direction The SDK Has Never Heard Of");
    assert_eq!(rich["builder_fee"], "0.0125");
    assert_eq!(rich["twap_id"], 77);
    assert_eq!(rich["hash"], "0xdeadbeef");
    assert_eq!(
        rich["liquidation"],
        json!({"liquidated_user": USER, "mark_px": "74999.0", "method": "market"})
    );
}

#[tokio::test]
async fn spot_fills_name_the_pair_as_the_markets_list_it() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({
                "universe": [{"name": "@107", "index": 107, "tokens": [1, 0]}],
                "tokens": [
                    {"name": "USDC", "index": 0, "tokenId": "0x00000000000000000000000000000000", "szDecimals": 6, "weiDecimals": 6, "evmContract": null},
                    {"name": "HYPE", "index": 1, "tokenId": "0x00000000000000000000000000000001", "szDecimals": 2, "weiDecimals": 8, "evmContract": null}
                ]
            }),
        )
        .await;
    let mut spot = fill_at(START);
    spot["coin"] = json!("@107");
    spot["dir"] = json!("Buy");
    server
        .mount_info(
            json!({"type": "userFills", "user": USER}),
            json!([spot, fill_at(START + 1)]),
        )
        .await;

    let rows = support::json_stdout(&run(&server, "json", &["account", "fills", USER]).success());

    assert_eq!(rows[0]["coin"], "HYPE/USDC");
    assert_eq!(rows[1]["coin"], "BTC");
}

fn ledger_request(start: u64) -> Value {
    json!({"type": "userNonFundingLedgerUpdates", "user": USER, "startTime": start})
}

fn funding_request(start: u64) -> Value {
    json!({"type": "userFunding", "user": USER, "startTime": start})
}

fn timed(count: u64) -> Vec<Value> {
    (0..count)
        .map(|offset| json!({"time": START + offset, "hash": format!("0x{offset:x}"), "delta": {"type": "deposit", "usdc": "1.0"}}))
        .collect()
}

#[tokio::test]
async fn a_full_ledger_page_is_announced_at_two_thousand_rows() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(ledger_request(START), json!(timed(2000)))
        .await;

    let assert = run(
        &server,
        "json",
        &["account", "ledger", USER, "--start", &START.to_string()],
    )
    .success();

    assert_eq!(
        support::json_stdout(&assert).as_array().unwrap().len(),
        2000
    );
    let warning: Value = serde_json::from_str(support::stderr_text(&assert).trim()).unwrap();
    assert_eq!(warning["warning"]["cap"], 2000);
    assert_eq!(warning["warning"]["next_start"], START + 1999);
}

#[tokio::test]
async fn a_full_funding_page_is_announced_at_five_hundred_rows() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(funding_request(START), json!(timed(500)))
        .await;

    let assert = run(
        &server,
        "json",
        &["account", "funding", USER, "--start", &START.to_string()],
    )
    .success();

    assert_eq!(support::json_stdout(&assert).as_array().unwrap().len(), 500);
    let warning: Value = serde_json::from_str(support::stderr_text(&assert).trim()).unwrap();
    assert_eq!(warning["warning"]["cap"], 500);
    assert_eq!(warning["warning"]["next_start"], START + 499);
}

#[tokio::test]
async fn all_pages_the_funding_history_too() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(funding_request(START), json!(timed(500)))
        .await;
    server
        .mount_info(
            funding_request(START + 499),
            json!([
                {"time": START + 499, "hash": "0x1f3", "delta": {"type": "deposit", "usdc": "1.0"}},
                {"time": START + 500, "hash": "0x1f4", "delta": {"type": "deposit", "usdc": "1.0"}}
            ]),
        )
        .await;

    let assert = run(
        &server,
        "json",
        &[
            "account",
            "funding",
            USER,
            "--start",
            &START.to_string(),
            "--all",
        ],
    )
    .success();

    assert_eq!(support::json_stdout(&assert).as_array().unwrap().len(), 501);
    assert_eq!(support::stderr_text(&assert), "");
}
