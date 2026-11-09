//! Shared harness for the integration tests.
//!
//! Every test binary includes this module with `mod support;` and uses a part
//! of it, so unused-item warnings would be noise rather than signal.
//!
//! - [`hermetic_command`] is the only constructor of the CLI under test.
//! - [`IsolatedHome`] adds a private home, config and data directory to it.
//! - [`exchange::ExchangeFixture`] is a mock Hyperliquid API for commands that
//!   talk to the network.
//! - [`wire`] builds the API's reply shapes and [`signing`] reads back who
//!   signed a request the mock received.
#![allow(dead_code)]

use assert_cmd::Command;
use hyperliquid_cli::env;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use wiremock::matchers::{body_json, body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub mod exchange;
pub mod legacy_accounts_fixture;
pub mod orders;
pub mod signing;
pub mod wire;

pub const API_OVERRIDE_ENV: &str = env::API_BASE_URL;
pub const MAINNET_API_OVERRIDE_ENV: &str = env::MAINNET_API_BASE_URL;
pub const TESTNET_API_OVERRIDE_ENV: &str = env::TESTNET_API_BASE_URL;
pub const FORMAT_ENV: &str = env::FORMAT;
pub const PRIVATE_KEY_ENV: &str = env::PRIVATE_KEY;
pub const WATCH_MAX_TICKS_ENV: &str = env::WATCH_MAX_TICKS;
pub const ACCOUNT_KEY_PASSPHRASE_ENV: &str = env::ACCOUNT_KEY_PASSPHRASE;
pub const ACCOUNT_KEYCHAIN_DISABLED_ENV: &str = env::ACCOUNT_KEYCHAIN_DISABLED;
pub const ACCOUNT_KEY_STORE_DIR_ENV: &str = env::ACCOUNT_KEY_STORE_DIR;
pub const TEST_ACCOUNT_PASSPHRASE: &str = "deterministic integration account encryption passphrase";
pub const VALID_PRIVATE_KEY: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000009";

/// Hosts the CLI must reach directly even when a proxy variable is set.
pub const LOOPBACK_NO_PROXY: &str = "127.0.0.1,localhost,::1";

/// Where a hermetic command's proxy variables lead: a loopback port nothing
/// listens on. With loopback exempt ([`LOOPBACK_NO_PROXY`]), a mock server is
/// reached directly and any other host, the public API included, is refused at
/// once. A proxy, not an `HYPERLIQUID_API_BASE_URL` pointed at a dead port: an
/// API override makes the CLI probe it before every network command, which
/// would fail the tests that check validation happens before any request.
pub const CLOSED_PROXY_URL: &str = "http://127.0.0.1:1";

/// A directory nothing can be created under, so a command run without an
/// [`IsolatedHome`] fails to write instead of touching the developer's files.
const UNWRITABLE_ROOT: &str = "/dev/null/hyperliquid-test-no-home";

/// Point the platform lookups (`HOME`, `XDG_*`, `APPDATA`, ...) at `root`.
fn point_platform_dirs_at(command: &mut Command, root: &Path) {
    command
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("APPDATA", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("LOCALAPPDATA", root.join("data"));
}

/// The CLI's config directory as the platform lookup finds it for a process
/// whose `HOME` is `home` and `XDG_CONFIG_HOME` is `xdg_config_home`:
/// `~/Library/Application Support` on macOS, the XDG directory elsewhere.
pub fn platform_config_dir(home: &Path, xdg_config_home: &Path) -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        xdg_config_home.to_path_buf()
    };
    base.join("hyperliquid")
}

/// The CLI under test, cut off from the machine it runs on: every setting the
/// CLI reads from the environment and every proxy variable is cleared,
/// loopback bypasses proxies, the platform home points at a directory that
/// cannot be written, and every other host leads to a closed port
/// ([`CLOSED_PROXY_URL`]). Behavior must come from each test, not from CI, an
/// agent harness, the developer's shell or the network; a test that talks to an
/// API points `HYPERLIQUID_API_BASE_URL` at its mock server. This is the only
/// place the binary is spawned; `tests/hermetic_harness.rs` fails the suite
/// when a test does it by hand.
pub fn hermetic_command() -> Command {
    let mut command = Command::cargo_bin("hyperliquid").unwrap();
    for name in env::ALL {
        command.env_remove(name);
    }
    command
        .env("NO_PROXY", LOOPBACK_NO_PROXY)
        .env("HTTP_PROXY", CLOSED_PROXY_URL)
        .env("HTTPS_PROXY", CLOSED_PROXY_URL)
        .env("ALL_PROXY", CLOSED_PROXY_URL);
    point_platform_dirs_at(&mut command, Path::new(UNWRITABLE_ROOT));
    command
}

/// A `std` command with `command`'s program, arguments and environment, to
/// spawn with piped output and stop early. `assert_cmd` runs to completion.
pub fn std_command(command: &Command) -> std::process::Command {
    let mut std = std::process::Command::new(command.get_program());
    std.args(command.get_args());
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => std.env(key, value),
            None => std.env_remove(key),
        };
    }
    std
}

/// A private home, config and data directory for one test.
///
/// The CLI is pointed at them through its `HYPERLIQUID_*_DIR` overrides only.
/// The platform variables lead to a separate `platform` directory that must
/// stay empty: if an override stopped working the CLI would fall back to it
/// and the test would fail (here, on drop) instead of passing by accident.
pub struct IsolatedHome {
    _tmp: TempDir,
    pub home: PathBuf,
    /// Parent of the CLI's config directory, which is `config/hyperliquid`.
    pub config: PathBuf,
    /// Parent of the CLI's data directory, which is `data/hyperliquid`.
    pub data: PathBuf,
    platform: PathBuf,
}

impl IsolatedHome {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let config = tmp.path().join("config");
        let data = tmp.path().join("data");
        let platform = tmp.path().join("platform");
        for path in [&home, &config, &data, &platform] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self {
            _tmp: tmp,
            home,
            config,
            data,
            platform,
        }
    }

    pub fn tmp_path(&self) -> &Path {
        self._tmp.path()
    }

    /// Where the platform variables point. The CLI must never write here.
    pub fn platform_dir(&self) -> &Path {
        &self.platform
    }

    /// Where the platform's own config-directory lookup leads, which the CLI
    /// reaches only when no `HYPERLIQUID_CONFIG_DIR` override is set. It stays
    /// empty; a test that wants a config the CLI finds that way builds its own
    /// home and uses [`platform_config_dir`].
    pub fn platform_config_dir(&self) -> PathBuf {
        platform_config_dir(&self.platform.join("home"), &self.platform.join("config"))
    }

    /// The directory the CLI keeps `config.json` and `last_error.json` in.
    pub fn config_dir(&self) -> PathBuf {
        self.config.join("hyperliquid")
    }

    /// The directory the CLI keeps its data files in.
    pub fn data_dir(&self) -> PathBuf {
        self.data.join("hyperliquid")
    }

    /// The CLI with its directories in this home and human-readable output.
    ///
    /// The format is forced to `pretty` because stdout is piped under test and
    /// the CLI answers JSON whenever stdout is not a terminal: without it a
    /// test of the human output would silently read JSON. A JSON test passes
    /// `--format json`, which outranks the variable, or starts from
    /// [`Self::live_json`].
    pub fn command(&self) -> Command {
        let mut command = hermetic_command();
        self.point_at(&mut command, &self.home, &self.data_dir());
        command.env(FORMAT_ENV, "pretty");
        command
    }

    /// Point the CLI's directories at this home's, with `home` and `data_dir`
    /// replacing the default home and data directory.
    fn point_at(&self, command: &mut Command, home: &Path, data_dir: &Path) {
        point_platform_dirs_at(command, &self.platform);
        command
            .env(env::HOME_DIR, home)
            .env(env::CONFIG_DIR, self.config_dir())
            .env(env::DATA_DIR, data_dir);
    }

    pub fn account_command(&self, passphrase: &str) -> Command {
        let mut command = self.command();
        self.apply_account_env(&mut command, Some(passphrase));
        command
    }

    pub fn account_command_without_passphrase(&self) -> Command {
        let mut command = self.command();
        self.apply_account_env(&mut command, None);
        command
    }

    /// An account command with no passphrase whose home and data directory are
    /// not this home's.
    pub fn account_command_for_paths_without_passphrase(
        &self,
        home: &Path,
        data: &Path,
    ) -> Command {
        let mut command = hermetic_command();
        self.point_at(&mut command, home, &data.join("hyperliquid"));
        command
            .env(FORMAT_ENV, "pretty")
            .env(ACCOUNT_KEYCHAIN_DISABLED_ENV, "1");
        command
    }

    pub fn apply_account_env(&self, command: &mut Command, passphrase: Option<&str>) {
        command.env(ACCOUNT_KEYCHAIN_DISABLED_ENV, "1");
        match passphrase {
            Some(passphrase) => {
                command.env(ACCOUNT_KEY_PASSPHRASE_ENV, passphrase);
            }
            None => {
                command.env_remove(ACCOUNT_KEY_PASSPHRASE_ENV);
            }
        }
    }

    pub fn command_with_api_url(&self, url: impl AsRef<str>) -> Command {
        let mut command = self.command();
        command.env(API_OVERRIDE_ENV, url.as_ref());
        command
    }

    pub fn command_with_server(&self, server: &MockServer) -> Command {
        self.command_with_api_url(server.uri())
    }

    /// A command against `server` that can sign: the API override plus a valid
    /// private key.
    pub fn live_command(&self, server: &MockServer) -> Command {
        let mut command = self.command_with_server(server);
        command.env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY);
        command
    }

    /// [`Self::live_command`] on the testnet with JSON output: what a test of
    /// a signed request starts from. The test adds the subcommand and reads
    /// the receipt with [`run_json`].
    pub fn live_json(&self, server: &MockServer) -> Command {
        let mut command = self.live_command(server);
        command.args(["--format", "json", "--testnet"]);
        command
    }

    /// This home with `alias` stored as its default account, holding
    /// `private_key`: a plain wallet, or an API wallet of `master` when one is
    /// given. Run it with [`Self::account_command_without_passphrase`].
    pub fn with_stored_account(self, alias: &str, private_key: &str, master: Option<&str>) -> Self {
        let address = expected_address(private_key);
        let row = legacy_accounts_fixture::Row::new(alias, &address, private_key).default_account();
        let row = match master {
            Some(master) => row.agent_of(master),
            None => row,
        };
        legacy_accounts_fixture::create_accounts_db_with_key_file(
            &self.accounts_db(),
            &self.legacy_accounts_key(),
            &[row],
        );
        self
    }

    pub fn account_command_with_server(&self, passphrase: &str, server: &MockServer) -> Command {
        let mut command = self.account_command(passphrase);
        command.env(API_OVERRIDE_ENV, server.uri());
        command
    }

    pub fn account_command_with_mainnet_and_testnet(
        &self,
        passphrase: &str,
        mainnet_server: &MockServer,
        testnet_server: &MockServer,
    ) -> Command {
        let mut command = self.account_command(passphrase);
        command
            .env(MAINNET_API_OVERRIDE_ENV, mainnet_server.uri())
            .env(TESTNET_API_OVERRIDE_ENV, testnet_server.uri());
        command
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.json")
    }

    pub fn last_error_file(&self) -> PathBuf {
        self.config_dir().join("last_error.json")
    }

    pub fn write_last_error(&self, value: &serde_json::Value) {
        std::fs::create_dir_all(self.config_dir()).unwrap();
        std::fs::write(self.last_error_file(), value.to_string()).unwrap();
    }

    pub fn read_last_error(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.last_error_file())
            .expect("no last_error.json was recorded");
        serde_json::from_str(&text).unwrap()
    }

    /// The pre-vault accounts database.
    pub fn accounts_db(&self) -> PathBuf {
        self.data_dir().join("accounts.db")
    }

    /// The plain key file that releases before the OS keychain kept beside the
    /// accounts database.
    pub fn legacy_accounts_key(&self) -> PathBuf {
        self.data_dir().join("accounts.key")
    }

    /// The default OWS vault: `.hyperliquid` under the home.
    pub fn ows_vault(&self) -> PathBuf {
        self.home.join(".hyperliquid")
    }

    /// The removed account-data key file the CLI must not recreate.
    pub fn removed_account_data_key(&self) -> PathBuf {
        self.config_dir().join("account-data.key")
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let strays = files_under(&self.platform);
        assert!(
            strays.is_empty(),
            "the CLI wrote outside its HYPERLIQUID_*_DIR overrides: {strays:?}"
        );
    }
}

/// Every file under `dir`, recursively. Empty directories do not count.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

/// Every `.rs` file under `dir`, recursively, for the tests that scan source.
pub fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found
}

pub fn expected_address(private_key: &str) -> String {
    private_key
        .parse::<hypersdk::hypercore::PrivateKeySigner>()
        .unwrap()
        .address()
        .to_string()
}

pub fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// Milliseconds since the Unix epoch.
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Parse captured stdout as one JSON document. With [`json_stdout`], the only
/// way a test reads a command's JSON.
pub fn stdout_json(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).expect("stdout should be one JSON document")
}

/// The JSON document a finished command printed on stdout.
pub fn json_stdout(assert: &assert_cmd::assert::Assert) -> serde_json::Value {
    stdout_json(&assert.get_output().stdout)
}

/// What a finished command printed on stderr.
pub fn stderr_text(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).expect("stderr should be UTF-8")
}

/// Run `command`, require success, and return the JSON it printed.
pub fn run_json(command: &mut Command) -> serde_json::Value {
    json_stdout(&command.assert().success())
}

/// Every body `server` received on `path`, in arrival order. A body that is
/// not JSON is skipped.
pub async fn request_bodies(server: &MockServer, request_path: &str) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .expect("the mock server records requests")
        .iter()
        .filter(|request| request.url.path() == request_path)
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect()
}

/// The `/info` requests of one `type`, in arrival order.
pub async fn info_requests(server: &MockServer, request_type: &str) -> Vec<serde_json::Value> {
    request_bodies(server, "/info")
        .await
        .into_iter()
        .filter(|body| body["type"] == request_type)
        .collect()
}

/// The signed `/exchange` envelopes `server` received.
pub async fn exchange_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    request_bodies(server, "/exchange").await
}

/// The `action` of every `/exchange` request `server` received. For a server
/// that is not an [`exchange::ExchangeFixture`], which has the same accessor.
pub async fn exchange_actions(server: &MockServer) -> Vec<serde_json::Value> {
    exchange_bodies(server)
        .await
        .into_iter()
        .map(|body| body["action"].clone())
        .collect()
}

/// Answer the API-override probe, an unscoped `allMids` read, and nothing
/// else: a dex-scoped `allMids` does not match, so a test that wants one mounts
/// its own.
pub async fn mount_all_mids(server: &MockServer, btc: &str, eth: &str) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_json(serde_json::json!({"type": "allMids"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "BTC": btc,
            "ETH": eth
        })))
        .mount(server)
        .await;
}

/// Mount the reply the API-override probe expects.
pub async fn mount_override_healthcheck(server: &MockServer) {
    mount_all_mids(server, "50000", "3000").await;
}

pub async fn mock_all_mids_server() -> MockServer {
    mock_all_mids_server_with_prices("50000.0", "3000.0").await
}

pub async fn mock_all_mids_server_with_prices(btc: &str, eth: &str) -> MockServer {
    let server = MockServer::start().await;
    mount_all_mids(&server, btc, eth).await;
    server
}

pub async fn mount_account_state(
    server: &MockServer,
    open_orders: Vec<serde_json::Value>,
    fills: Vec<serde_json::Value>,
    withdrawable: &str,
    spot_usdc_total: &str,
    leverage: u32,
) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "frontendOpenOrders"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(open_orders))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(serde_json::json!({"type": "userFills"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fills))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "clearinghouseState"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(wire::clearinghouse_state(withdrawable, leverage)),
        )
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "spotClearinghouseState"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "balances": [
                {
                    "coin": "USDC",
                    "token": 0,
                    "hold": "0",
                    "total": spot_usdc_total,
                    "entryNtl": "0"
                }
            ]
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "userVaultEquities"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "userAbstraction"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!("disabled")))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(
            serde_json::json!({"type": "subAccounts"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
}

/// Mount successful leverage, USD class transfer, and order actions.
pub async fn mount_successful_exchange_actions(
    server: &MockServer,
    order_response: serde_json::Value,
) {
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .and(body_string_contains(r#""type":"updateLeverage""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": { "type": "default" }
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/exchange"))
        .and(body_string_contains(r#""type":"usdClassTransfer""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "response": { "type": "default" }
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/exchange"))
        .and(body_string_contains(r#""type":"order""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(order_response))
        .mount(server)
        .await;
}
