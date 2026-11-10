//! The stream-bound contract of `--watch` and `subscribe` driven through the
//! CLI: JSON streams need a bound, the flags beat the environment, and a
//! closed pipe or a signal ends a stream cleanly. The stream rules themselves
//! (`subscribe` events, gaps, projection) are in `subscribe_streams.rs`.

mod support;

use std::io::{BufRead, BufReader, Read};
use std::process::Stdio;

use hyperliquid_cli::env;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::{FORMAT_ENV, IsolatedHome, WATCH_MAX_TICKS_ENV, std_command, wire};

const USER: &str = "0x0000000000000000000000000000000000000001";

fn lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8(stdout.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn watch_mids_json_max_ticks_flag_beats_the_environment_and_bounds_the_output() {
    let server = ExchangeFixture::start().await;

    let assert = IsolatedHome::new()
        .json_command_for(&server)
        .env(WATCH_MAX_TICKS_ENV, "5")
        .args([
            "--select",
            "coin,price",
            "mids",
            "--watch",
            "--max-ticks",
            "1",
        ])
        .assert()
        .success();

    assert_eq!(
        lines(&assert.get_output().stdout),
        [json!([
            {"coin": "BTC", "price": "50000"},
            {"coin": "ETH", "price": "3000"}
        ])]
    );
}

#[tokio::test]
async fn watch_mids_human_formats_on_a_pipe_with_a_bound_stream_ndjson() {
    let server = ExchangeFixture::start().await;

    for extra in [&[][..], &["--format", "table"][..]] {
        let assert = IsolatedHome::new()
            .command_with_server(&server)
            .args(["mids", "--watch", "--max-ticks", "1"])
            .args(extra)
            .assert()
            .success()
            .stdout(predicate::str::contains("EnterAlternateScreen").not())
            .stderr(predicate::str::contains(
                "Streaming JSON lines because stdout isn't a terminal.",
            ));

        assert_eq!(
            lines(&assert.get_output().stdout),
            [json!({"BTC": "50000", "ETH": "3000"})],
            "{extra:?}"
        );
    }
}

#[tokio::test]
async fn an_unbounded_mids_watch_is_refused_before_anything_is_drawn() {
    let server = ExchangeFixture::start().await;
    let home = IsolatedHome::new();

    home.command_with_server(&server)
        .args(["--format", "pretty", "mids", "--watch"])
        .assert()
        .failure()
        .code(13)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Non-TTY"))
        .stderr(predicate::str::contains("--format json"))
        .stderr(predicate::str::contains("--max-ticks"));

    home.command_with_server(&server)
        .env_remove(FORMAT_ENV)
        .args(["mids", "--watch"])
        .assert()
        .failure()
        .code(13)
        .stdout(predicate::str::contains(
            "JSON watch output must be bounded",
        ))
        .stdout(predicate::str::contains("--max-ticks"));
}

#[test]
fn watch_max_ticks_zero_is_rejected_by_the_parser() {
    IsolatedHome::new()
        .command()
        .env_remove(FORMAT_ENV)
        .args(["mids", "--watch", "--max-ticks", "0"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicate::str::contains("value must be at least 1"));
}

#[tokio::test]
async fn other_snapshot_watches_print_one_line_per_tick() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "frontendOpenOrders", "user": USER}),
            json!([wire::basic_order("BTC", 4242)]),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "candleSnapshot", "req": {"coin": "BTC"}}),
            json!([{
                "t": 1_700_000_000_000_u64, "T": 1_700_003_599_999_u64, "s": "BTC", "i": "1h",
                "o": "120.5", "h": "125", "l": "119", "c": "123.5", "v": "42", "n": 7
            }]),
        )
        .await;
    let home = IsolatedHome::new();

    let orders = home
        .json_command_for(&server)
        .args([
            "--ows-signer",
            USER,
            "orders",
            "open",
            "--watch",
            "--max-ticks",
            "1",
        ])
        .assert()
        .success();
    let orders = lines(&orders.get_output().stdout);
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0][0]["oid"], 4242);

    let candles = home
        .json_command_for(&server)
        .args([
            "candles",
            "BTC",
            "--limit",
            "1",
            "--watch",
            "--max-ticks",
            "1",
        ])
        .assert()
        .success();
    let candles = lines(&candles.get_output().stdout);
    assert_eq!(candles.len(), 1);
    assert_eq!(candles[0][0]["close"], "123.5");
}

#[test]
fn a_json_subscribe_needs_a_bound_from_a_flag_or_the_environment() {
    let home = IsolatedHome::new();

    home.command()
        .env_remove(FORMAT_ENV)
        .args(["subscribe", "all-mids"])
        .assert()
        .failure()
        .code(13)
        .stdout(predicate::str::contains(
            "JSON subscribe output must be bounded",
        ))
        .stdout(predicate::str::contains("--max-events"));

    // An agent gets JSON-style bounds even when it asks for the pretty format.
    home.command()
        .env(env::AGENT, "1")
        .args(["--format", "pretty", "subscribe", "all-mids"])
        .assert()
        .failure()
        .code(13)
        .stderr(predicate::str::contains(
            "JSON subscribe output must be bounded",
        ));

    let bounded = home
        .command()
        .env(env::SUBSCRIBE_MAX_EVENTS, "0")
        .args(["subscribe", "all-mids"])
        .assert()
        .success();
    assert_eq!(
        lines(&bounded.get_output().stdout),
        [json!({"type": "subscribed", "subscription": "allMids(None)"})]
    );

    home.command()
        .env_remove(FORMAT_ENV)
        .env(env::SUBSCRIBE_MAX_EVENTS, "not-a-number")
        .args(["subscribe", "all-mids"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicate::str::contains(
            "HYPERLIQUID_SUBSCRIBE_MAX_EVENTS must be a non-negative integer",
        ));
}

#[tokio::test]
async fn watch_mids_ends_with_exit_0_when_stdout_is_closed() {
    let server = ExchangeFixture::start().await;
    let home = IsolatedHome::new();
    let mut command = home.json_command_for(&server);
    command.args(["mids", "--watch", "--max-ticks", "50"]);

    let mut child = std_command(&command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Nothing reads stdout: the first snapshot is written to a closed pipe.
    drop(child.stdout.take());

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}

#[cfg(unix)]
#[tokio::test]
async fn watch_mids_sigterm_exits_143_with_complete_json_lines() {
    let server = ExchangeFixture::start().await;
    let home = IsolatedHome::new();
    let mut command = home.json_command_for(&server);
    command.args(["mids", "--watch", "--max-ticks", "50"]);

    let mut child = std_command(&command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first_line = String::new();
    stdout.read_line(&mut first_line).unwrap();
    assert!(serde_json::from_str::<Value>(first_line.trim()).is_ok());

    let status = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(143));
    for line in rest.lines() {
        assert!(
            serde_json::from_str::<Value>(line).is_ok(),
            "truncated line: {line}"
        );
    }
    assert!(rest.is_empty() || rest.ends_with('\n'));
}
