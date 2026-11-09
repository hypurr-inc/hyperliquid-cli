//! The harness itself: the CLI under test is spawned in one place, cut off from
//! the machine, and confined to its own directories.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use hyperliquid_cli::env;
use support::exchange::ExchangeFixture;
use support::{
    CLOSED_PROXY_URL, IsolatedHome, LOOPBACK_NO_PROXY, TEST_ACCOUNT_PASSPHRASE, VALID_PRIVATE_KEY,
    expected_address, hermetic_command, json_stdout, run_json_failure, rust_files,
};

/// The test files, relative to `tests/`, whose code spawns the binary directly.
fn files_that_spawn_the_binary(tests: &Path) -> Vec<String> {
    let needle = ["cargo", "_bin("].concat();
    let mut spawning: Vec<String> = rust_files(tests)
        .iter()
        .map(|path| {
            path.strip_prefix(tests)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| !name.starts_with("support/") && name != "hermetic_harness.rs")
        .filter(|name| {
            fs::read_to_string(tests.join(name))
                .unwrap()
                .contains(&needle)
        })
        .collect();
    spawning.sort();
    spawning
}

#[test]
fn only_the_harness_spawns_the_binary() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let spawning = files_that_spawn_the_binary(&tests);

    assert!(
        spawning.is_empty(),
        "spawn the CLI with support::hermetic_command() or IsolatedHome::command(): {spawning:?}"
    );
}

/// The environment a process gets when it is started from a shell that sets
/// `inherited`, with the variables the hermetic command sets or removes applied
/// on top: `env(1)` stands in for the CLI.
fn environment_a_hermetic_child_sees(inherited: &[(&str, &str)]) -> BTreeMap<String, String> {
    let hermetic = hermetic_command();
    let mut probe = std::process::Command::new("env");
    probe.envs(inherited.iter().copied());
    for (name, value) in hermetic.get_envs() {
        match value {
            Some(value) => probe.env(name, value),
            None => probe.env_remove(name),
        };
    }
    let output = probe.output().expect("env(1) runs");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[test]
fn a_hermetic_child_does_not_see_the_shell_it_was_started_from() {
    let seen = environment_a_hermetic_child_sees(&[
        (env::FORMAT, "table"),
        (env::AGENT, "1"),
        (env::CI, "1"),
        (env::NETWORK, "testnet"),
        (env::PRIVATE_KEY, "sentinel-key"),
        (env::API_BASE_URL, "http://sentinel.invalid"),
        (env::CONFIG_DIR, "/sentinel/config"),
        ("HOME", "/sentinel/home"),
        ("HTTPS_PROXY", "http://sentinel.invalid:3128"),
    ]);

    for name in [
        env::FORMAT,
        env::AGENT,
        env::CI,
        env::NETWORK,
        env::PRIVATE_KEY,
        env::API_BASE_URL,
        env::CONFIG_DIR,
    ] {
        assert_eq!(seen.get(name), None, "{name} reached the child");
    }
    assert_ne!(seen["HOME"], "/sentinel/home");
    assert_eq!(seen["HTTPS_PROXY"], CLOSED_PROXY_URL);
    assert_eq!(seen["NO_PROXY"], LOOPBACK_NO_PROXY);
}

/// A read no test mocked must fail on the closed proxy, not succeed against the
/// public API: the suite stays offline.
#[test]
fn an_unmocked_read_fails_on_the_closed_proxy() {
    let home = IsolatedHome::new();

    let error = run_json_failure(home.json_command().arg("mids"), 12);

    assert_eq!(error["reason"], "network_error", "{error}");
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("HTTPS_PROXY (127.0.0.1:1)"),
        "{error}"
    );
}

/// `reqwest` honors the proxy variables and does not bypass loopback on its
/// own, so without the harness's `NO_PROXY` the closed proxy would receive
/// every request meant for a mock server.
#[tokio::test]
async fn a_loopback_mock_server_is_reached_directly() {
    let server = ExchangeFixture::start().await;
    let home = IsolatedHome::new();

    let assert = home
        .command_with_server(&server)
        .args(["--format", "json", "mids"])
        .assert()
        .success();

    assert_eq!(json_stdout(&assert)["BTC"], "50000");
}

/// The CLI is pointed at its directories by `HYPERLIQUID_*_DIR` only. These
/// writes land in the overrides; the platform directories stay empty, which
/// `IsolatedHome` asserts when it drops.
#[test]
fn the_overrides_alone_decide_where_the_cli_writes() {
    let env = IsolatedHome::new();

    env.account_command(TEST_ACCOUNT_PASSPHRASE)
        .args(["wallet", "import", VALID_PRIVATE_KEY])
        .assert()
        .success()
        .stdout(predicates::str::contains(expected_address(
            VALID_PRIVATE_KEY,
        )));

    assert!(env.config_file().is_file(), "{:?}", env.config_file());
    assert!(env.ows_vault().is_dir(), "{:?}", env.ows_vault());
    assert!(
        fs::read_dir(env.platform_dir()).unwrap().next().is_none(),
        "the platform directories must stay empty"
    );
}

#[test]
fn a_write_outside_the_overrides_fails_the_test_that_made_it() {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let env = IsolatedHome::new();
        let stray = env.platform_dir().join("config").join("hyperliquid");
        fs::create_dir_all(&stray).unwrap();
        fs::write(stray.join("config.json"), "{}").unwrap();
    }));

    assert!(
        outcome.is_err(),
        "dropping a home the CLI wrote outside of must panic"
    );
}

#[test]
fn a_bare_hermetic_command_cannot_write_to_the_machines_home() {
    let command = hermetic_command();
    let home = command
        .get_envs()
        .find(|(key, _)| *key == "HOME")
        .and_then(|(_, value)| value)
        .expect("HOME is set");

    assert!(
        fs::create_dir_all(Path::new(home).join("probe")).is_err(),
        "{home:?} must not be writable"
    );
}
