//! What reaches stdout and stderr in each output format. `assert_cmd` pipes
//! both, so every run here is a non-TTY run unless a test forces color
//! through the environment.

mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, wire};

fn validator(name: &str, index: u64) -> Value {
    wire::validator(&format!("0x{index:040x}"), name)
}

async fn validators_server(validators: Vec<Value>) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "validatorSummaries"}),
            Value::Array(validators),
        )
        .await;
    server
}

fn validators_command(home: &IsolatedHome, server: &ExchangeFixture, format: &str) -> Command {
    let mut command = home.command_with_server(server);
    command.args(["--no-update-check", "--format", format]);
    command
}

#[tokio::test]
async fn table_on_a_pipe_is_tab_separated_without_borders() {
    let home = IsolatedHome::new();
    let server = validators_server(vec![validator("Validator One", 2)]).await;

    let assert = validators_command(&home, &server, "table")
        .args(["staking", "validators"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(!stdout.contains(['╭', '│', '╰', '─']), "{stdout}");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{stdout}");
    let columns = lines[0].split('\t').count();
    assert!(columns > 1);
    assert_eq!(lines[1].split('\t').count(), columns);
    assert!(lines[1].contains("Validator One"));
}

#[tokio::test]
async fn pretty_on_a_pipe_has_no_ansi_and_no_timing_footer() {
    let home = IsolatedHome::new();
    let server = validators_server(vec![validator("Validator One", 2)]).await;

    validators_command(&home, &server, "pretty")
        .args(["staking", "validators"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Validator One"))
        .stdout(predicate::str::contains('\u{1b}').not())
        .stderr(predicate::str::contains("Completed in").not())
        .stderr(predicate::str::is_empty());
}

#[tokio::test]
async fn color_precedence_follows_no_color_then_force_color() {
    let home = IsolatedHome::new();
    let server = validators_server(vec![validator("Validator One", 2)]).await;
    let pretty = |colors: &[(&str, &str)]| {
        let mut command = validators_command(&home, &server, "pretty");
        command.args(["staking", "validators"]);
        for (name, value) in colors {
            command.env(name, value);
        }
        command.assert().success()
    };

    pretty(&[("FORCE_COLOR", "1")]).stdout(predicate::str::contains("\u{1b}[36m"));
    pretty(&[("FORCE_COLOR", "1"), ("NO_COLOR", "1")])
        .stdout(predicate::str::contains('\u{1b}').not());
    pretty(&[("FORCE_COLOR", "0")]).stdout(predicate::str::contains('\u{1b}').not());
}

#[tokio::test]
async fn json_on_a_pipe_is_one_compact_line() {
    let home = IsolatedHome::new();
    let server = validators_server(vec![validator("Validator One", 2)]).await;

    let assert = validators_command(&home, &server, "json")
        .args(["staking", "validators"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert_eq!(stdout.trim_end().lines().count(), 1, "{stdout}");
    serde_json::from_str::<Value>(&stdout).unwrap();
}

#[tokio::test]
async fn empty_result_prints_header_on_stdout_and_sentence_on_stderr() {
    let home = IsolatedHome::new();
    let server = validators_server(vec![]).await;

    let assert = validators_command(&home, &server, "table")
        .args(["staking", "validators"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(!stdout.contains("Message"), "{stdout}");
    assert!(!stderr.trim().is_empty());
    assert_eq!(stderr.trim().lines().count(), 1, "{stderr}");

    validators_command(&home, &server, "json")
        .args(["staking", "validators"])
        .assert()
        .success()
        .stdout(predicate::str::diff("[]\n"))
        .stderr(predicate::str::is_empty());
}

#[tokio::test]
async fn max_results_truncation_is_reported_on_stderr_for_human_formats_only() {
    let home = IsolatedHome::new();
    let server = validators_server(vec![validator("Validator One", 2), validator("Two", 4)]).await;
    let notice = "Showing 1 of 2. Remove --max-results to see all.";

    for format in ["table", "pretty"] {
        validators_command(&home, &server, format)
            .args(["--max-results", "1", "staking", "validators"])
            .assert()
            .success()
            .stderr(predicate::str::contains(notice));
    }

    validators_command(&home, &server, "json")
        .args(["--max-results", "1", "staking", "validators"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[tokio::test]
async fn cells_with_control_and_bidi_characters_are_escaped() {
    let home = IsolatedHome::new();
    let hostile = "evil\u{1b}[31m\nFORGED\tROW\u{202e}REV";
    let server = validators_server(vec![validator(hostile, 6)]).await;

    for format in ["table", "pretty"] {
        let assert = validators_command(&home, &server, format)
            .args(["staking", "validators"])
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(
            stdout.contains("evil\\x1b[31m\\nFORGED\\tROW\\u{202e}REV"),
            "{stdout}"
        );
        assert!(!stdout.contains('\u{1b}'), "{stdout}");
        assert!(!stdout.contains('\u{202e}'), "{stdout}");
        assert!(!stdout.contains("\nFORGED"), "{stdout}");
    }
}
