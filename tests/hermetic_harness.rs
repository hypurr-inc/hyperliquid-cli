//! The harness itself: the CLI under test is spawned in one place, cut off from
//! the machine, and confined to its own directories.

mod support;

use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use hyperliquid_cli::env;
use support::exchange::ExchangeFixture;
use support::{
    CLOSED_PROXY_URL, IsolatedHome, LOOPBACK_NO_PROXY, TEST_ACCOUNT_PASSPHRASE, VALID_PRIVATE_KEY,
    expected_address, hermetic_command, json_stdout, rust_files,
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

#[test]
fn the_hermetic_command_clears_every_setting_and_proxy_variable() {
    let command = hermetic_command();
    let envs: Vec<_> = command.get_envs().collect();
    let value_of = |name: &str| {
        envs.iter()
            .find(|(key, _)| *key == name)
            .unwrap_or_else(|| panic!("{name} is neither set nor removed"))
            .1
    };

    let proxy_variables = ["NO_PROXY", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];
    for name in env::ALL
        .iter()
        .filter(|name| !proxy_variables.contains(name))
    {
        assert!(value_of(name).is_none(), "{name} must be removed");
    }
    assert_eq!(
        value_of("NO_PROXY").unwrap(),
        std::ffi::OsStr::new(LOOPBACK_NO_PROXY)
    );
    for name in &proxy_variables[1..] {
        assert_eq!(
            value_of(name).unwrap(),
            std::ffi::OsStr::new(CLOSED_PROXY_URL)
        );
    }
}

/// A read no test mocked must fail on the closed proxy, not succeed against the
/// public API: the suite stays offline.
#[test]
fn an_unmocked_read_fails_with_a_connection_error() {
    hermetic_command()
        .args(["--format", "json", "mids"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("\"error\""));
}

/// `reqwest` honors the proxy variables and does not bypass loopback on its
/// own, so without the harness's `NO_PROXY` a proxy in the shell would receive
/// every request meant for a mock server.
#[tokio::test]
async fn a_proxy_in_the_shell_does_not_swallow_requests_to_a_loopback_mock() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();

    let assert = env
        .command_with_server(&server)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
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
