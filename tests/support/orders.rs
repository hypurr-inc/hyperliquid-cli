//! What the order command tests share: command lines written as one string,
//! order files in the test's private directory, and the standing assertions
//! about what an order command did or did not send.

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::exchange::ExchangeFixture;
use super::{IsolatedHome, run_json};

/// The arguments of a command line written as one string. For lines whose
/// arguments contain no spaces.
pub fn words(line: &str) -> std::str::SplitWhitespace<'_> {
    line.split_whitespace()
}

/// A signing command on testnet against `server` with human-readable output:
/// what a test of the pretty output or a prompt starts from. A test of the
/// signed request starts from [`IsolatedHome::live_json`].
pub fn testnet_command(env: &IsolatedHome, server: &ExchangeFixture, line: &str) -> Command {
    let mut command = env.live_command(server);
    command.arg("--testnet").args(words(line));
    command
}

/// A signing command on mainnet against `server`: it must confirm first.
pub fn mainnet_command(env: &IsolatedHome, server: &ExchangeFixture, line: &str) -> Command {
    let mut command = env.live_command(server);
    command.args(words(line));
    command
}

/// The preview `--dry-run` prints for `line` on testnet, run without a signer
/// against `server`.
pub fn dry_run(env: &IsolatedHome, server: &ExchangeFixture, line: &str) -> Value {
    run_json(
        env.command_with_server(server)
            .args(["--format", "json", "--dry-run", "--testnet"])
            .args(words(line)),
    )
}

/// Put the file `tests/fixtures/<name>` in the test's private directory and
/// return the path to give the CLI. It reads order files relative to its
/// working directory, so the command runs with `current_dir(env.tmp_path())`.
pub fn stage_fixture(env: &IsolatedHome, name: &str) -> String {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::copy(&source, env.tmp_path().join(name))
        .unwrap_or_else(|error| panic!("fixture {}: {error}", source.display()));
    name.to_string()
}

/// Write `orders` as the file `name` in the test's private directory and
/// return the path to give the CLI (see [`stage_fixture`]).
pub fn write_orders_file(env: &IsolatedHome, name: &str, orders: &Value) -> String {
    std::fs::write(env.tmp_path().join(name), orders.to_string()).unwrap();
    name.to_string()
}

/// Answer every `/info` read whose body contains `request` (`{}` for all of
/// them) with HTTP `status` and the text `body` as it is, JSON or not: the
/// reads that fail, are limited or cannot be decoded. It outranks every other
/// mount, the fixture's included.
pub async fn answer_info(server: &ExchangeFixture, request: Value, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(request))
        .respond_with(ResponseTemplate::new(status).set_body_raw(body, "application/json"))
        .with_priority(1)
        .mount(server)
        .await;
}

/// The `/exchange` actions the mock received were none: the command stopped
/// before signing anything.
pub async fn assert_nothing_sent(server: &ExchangeFixture) {
    let actions = server.exchange_actions().await;
    assert!(actions.is_empty(), "nothing should be sent: {actions:?}");
}

/// The command line an error message suggests: the indented line that starts
/// with `hyperliquid `, as its arguments. The CLI's usage errors end in a
/// runnable example, so a test runs it instead of comparing its text.
pub fn suggested_invocation(message: &str) -> Vec<String> {
    let line = message
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("hyperliquid "))
        .unwrap_or_else(|| panic!("no suggested command in: {message}"));
    line.split_whitespace().map(String::from).collect()
}

/// An error document's `error` text, from the JSON a command printed.
pub fn error_text(document: &Value) -> &str {
    document["error"]
        .as_str()
        .unwrap_or_else(|| panic!("no error text in: {document}"))
}
