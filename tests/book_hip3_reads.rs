//! `book`, `spread`, `funding`, `candles` and `subscribe orderbook|trades|candles`
//! take a DEX-qualified market (`xyz:XYZ100`) and look it up on its dex: they
//! read `perpDexs`, then that dex's `meta`, then query the market by its
//! qualified name.
//!
//! `candles` runs to the end against the mock. The others finish on a
//! WebSocket, which always connects to the public endpoint and cannot be
//! pointed at a mock, so those tests stop the CLI once the market has been
//! looked up: the look-up is what these commands changed, and a CLI that
//! cannot resolve the market exits before it ever reaches the socket.

mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::{IsolatedHome, json_stdout, std_command};

const MARKET: &str = "xyz:XYZ100";

async fn xyz_fixture() -> ExchangeFixture {
    ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await
}

async fn dex_meta_reads(server: &ExchangeFixture) -> Vec<Value> {
    server
        .info_requests("meta")
        .await
        .into_iter()
        .filter(|body| body["dex"] == "xyz")
        .collect()
}

/// What the CLI did before it was stopped.
struct Stopped {
    exit_code: Option<i32>,
    output: String,
}

/// Run `command` until the mock has seen `done`, then stop it. If the CLI
/// exits first without `done` ever being seen, that exit is the result.
async fn run_until(
    command: &Command,
    server: &ExchangeFixture,
    done: impl AsyncFn(&ExchangeFixture) -> bool,
) -> Stopped {
    let mut child = std_command(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the CLI starts");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut exit_code = None;
    loop {
        if done(server).await {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            // The CLI ended; it may have seen `done` between the two checks.
            exit_code = Some(status.code().unwrap_or(-1));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the CLI never reached the expected read"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if exit_code.is_none() {
        child.kill().ok();
    }
    let output = child.wait_with_output().unwrap();
    Stopped {
        exit_code,
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

/// Run a WebSocket-backed command on `xyz:XYZ100` until its market look-up is
/// done, then assert the look-up went to the dex and did not fail.
async fn assert_looks_the_market_up_on_its_dex(args: &[&str]) {
    let env = IsolatedHome::new();
    let server = xyz_fixture().await;
    let mut command = env.command_with_server(&server);
    command.args(["--format", "json"]).args(args);

    let stopped = run_until(&command, &server, async |server| {
        !dex_meta_reads(server).await.is_empty()
    })
    .await;

    let reads = dex_meta_reads(&server).await;
    assert_eq!(
        reads.len(),
        1,
        "{args:?} must read the dex's meta once; the CLI said: {}",
        stopped.output
    );
    assert!(
        !server.info_requests("perpDexs").await.is_empty(),
        "{args:?} must list the dexes before reading one"
    );
    assert!(
        !stopped.output.contains("Asset not found") && stopped.exit_code != Some(13),
        "{args:?} must resolve {MARKET}: {}",
        stopped.output
    );
}

#[tokio::test]
async fn book_looks_a_hip3_market_up_on_its_dex() {
    assert_looks_the_market_up_on_its_dex(&["book", MARKET]).await;
}

#[tokio::test]
async fn spread_looks_a_hip3_market_up_on_its_dex() {
    assert_looks_the_market_up_on_its_dex(&["spread", MARKET]).await;
}

#[tokio::test]
async fn subscribe_orderbook_looks_a_hip3_market_up_on_its_dex() {
    assert_looks_the_market_up_on_its_dex(&[
        "subscribe",
        "orderbook",
        "--asset",
        MARKET,
        "--max-events",
        "1",
    ])
    .await;
}

#[tokio::test]
async fn subscribe_trades_looks_a_hip3_market_up_on_its_dex() {
    assert_looks_the_market_up_on_its_dex(&[
        "subscribe",
        "trades",
        "--asset",
        MARKET,
        "--max-events",
        "1",
    ])
    .await;
}

#[tokio::test]
async fn subscribe_candles_looks_a_hip3_market_up_on_its_dex() {
    assert_looks_the_market_up_on_its_dex(&[
        "subscribe",
        "candles",
        "--asset",
        MARKET,
        "--interval",
        "1m",
        "--max-events",
        "1",
    ])
    .await;
}

/// `funding` reads the market's funding history by its qualified name before it
/// waits on the WebSocket for the live context.
#[tokio::test]
async fn funding_reads_a_hip3_markets_history_by_its_qualified_name() {
    let env = IsolatedHome::new();
    let server = xyz_fixture().await;
    server
        .mount_info_containing(json!({"type": "fundingHistory", "coin": MARKET}), json!([]))
        .await;
    let mut command = env.command_with_server(&server);
    command.args(["--format", "json", "funding", MARKET]);

    let stopped = run_until(&command, &server, async |server| {
        !server.info_requests("fundingHistory").await.is_empty()
    })
    .await;

    let history = server.info_requests("fundingHistory").await;
    assert_eq!(history.len(), 1, "{}", stopped.output);
    assert_eq!(history[0]["coin"], MARKET);
    assert_eq!(dex_meta_reads(&server).await.len(), 1);
}

#[tokio::test]
async fn candles_on_a_hip3_market_query_the_dex_market_by_its_qualified_name() {
    let env = IsolatedHome::new();
    let server = xyz_fixture().await;
    server
        .mount_info_containing(
            json!({"type": "candleSnapshot", "req": {"coin": MARKET, "interval": "1h"}}),
            json!([{
                "t": 1_700_000_000_000_u64,
                "T": 1_700_003_599_999_u64,
                "s": MARKET,
                "i": "1h",
                "o": "120.5",
                "h": "125",
                "l": "119",
                "c": "123.5",
                "v": "42",
                "n": 7
            }]),
        )
        .await;

    let assert = env
        .command_with_server(&server)
        .args(["--format", "json", "candles", MARKET, "--limit", "1"])
        .assert()
        .success();

    let rows = json_stdout(&assert);
    assert_eq!(rows[0]["coin"], MARKET);
    assert_eq!(rows[0]["close"], "123.5");
    assert_eq!(dex_meta_reads(&server).await.len(), 1);
    assert!(!server.info_requests("perpDexs").await.is_empty());
    assert_eq!(server.info_requests("candleSnapshot").await.len(), 1);
}

#[tokio::test]
async fn a_market_the_dex_does_not_list_is_not_found_after_reading_the_dex() {
    let env = IsolatedHome::new();
    let server = xyz_fixture().await;

    env.command_with_server(&server)
        .args(["--format", "json", "book", "xyz:NOPE"])
        .assert()
        .code(13);

    assert_eq!(dex_meta_reads(&server).await.len(), 1);
}
