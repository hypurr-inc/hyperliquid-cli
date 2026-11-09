//! Shared harness for the integration tests.
//!
//! Every test binary includes this module with `mod support;` and uses a part
//! of it, so unused-item warnings would be noise rather than signal.
//!
//! - [`hermetic_command`] is the only constructor of the CLI under test.
//! - [`IsolatedHome`] adds a private home, config and data directory to it.
//! - [`exchange::ExchangeFixture`] is a mock Hyperliquid API for commands that
//!   talk to the network.
#![allow(dead_code)]

use assert_cmd::Command;
use hyperliquid_cli::settings::env;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use wiremock::matchers::{body_json, body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub mod exchange;
pub mod legacy_accounts_fixture;

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

/// Variables the CLI's HTTP stack reads that are not in `settings::env`.
/// `reqwest` honors the proxy families and does not bypass loopback, so a proxy
/// in the shell would swallow every request to a mock server.
pub const PROCESS_ENV_OUTSIDE_SETTINGS: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "RUST_BACKTRACE",
];

/// Hosts the CLI must reach directly even when a proxy variable is set.
pub const LOOPBACK_NO_PROXY: &str = "127.0.0.1,localhost,::1";

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

/// The CLI under test, cut off from the machine it runs on: every setting the
/// CLI reads from the environment and every proxy variable is cleared,
/// loopback bypasses proxies, and the platform home points at a directory that
/// cannot be written. Behavior must come from each test, not from CI, an agent
/// harness or the developer's shell. This is the only place the binary is
/// spawned; `tests/hermetic_harness.rs` fails the suite when a test does it
/// by hand.
pub fn hermetic_command() -> Command {
    let mut command = Command::cargo_bin("hyperliquid").unwrap();
    for name in env::ALL.iter().chain(PROCESS_ENV_OUTSIDE_SETTINGS) {
        command.env_remove(name);
    }
    command.env("NO_PROXY", LOOPBACK_NO_PROXY);
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

    /// The directory the CLI keeps `config.json` and `last_error.json` in.
    pub fn config_dir(&self) -> PathBuf {
        self.config.join("hyperliquid")
    }

    /// The directory the CLI keeps its data files in.
    pub fn data_dir(&self) -> PathBuf {
        self.data.join("hyperliquid")
    }

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

    /// Where `config.json` lives. `config_file_candidates` and `config_file_path`
    /// predate the exact path and are kept for the tests that still call them.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.json")
    }

    pub fn config_file_candidates(&self) -> Vec<PathBuf> {
        vec![self.config_file()]
    }

    pub fn config_file_path(&self) -> PathBuf {
        let path = self.config_file();
        assert!(path.exists(), "config.json should exist");
        path
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

    pub fn accounts_db_candidates(&self) -> Vec<PathBuf> {
        vec![self.accounts_db()]
    }

    pub fn data_dir_path(&self) -> PathBuf {
        self.data_dir()
    }

    pub fn legacy_accounts_key_path(&self) -> PathBuf {
        self.data_dir().join("accounts.key")
    }

    /// The default OWS vault: `.hyperliquid` under the home.
    pub fn ows_vault(&self) -> PathBuf {
        self.home.join(".hyperliquid")
    }

    pub fn ows_vault_candidates(&self) -> Vec<PathBuf> {
        vec![self.ows_vault()]
    }

    pub fn ows_vault_path(&self) -> PathBuf {
        let path = self.ows_vault();
        assert!(path.exists(), "OWS vault should exist");
        path
    }

    /// The removed account-data key file the CLI must not recreate.
    pub fn deprecated_config_key_candidates(&self) -> Vec<PathBuf> {
        vec![self.config_dir().join("account-data.key")]
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

/// The JSON document a command printed on stdout.
pub fn json_stdout(assert: &assert_cmd::assert::Assert) -> serde_json::Value {
    stdout_json(&assert.get_output().stdout)
}

/// Parse captured stdout as one JSON document.
pub fn stdout_json(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).expect("stdout should be one JSON document")
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

/// The `action` of every `/exchange` request `server` received.
pub async fn exchange_actions(server: &MockServer) -> Vec<serde_json::Value> {
    exchange_bodies(server)
        .await
        .into_iter()
        .map(|body| body["action"].clone())
        .collect()
}

pub fn fixture_all_mids() -> serde_json::Value {
    serde_json::json!({
        "BTC": "50000",
        "ETH": "3000"
    })
}

/// One perpetual in a `meta` universe.
pub fn fixture_perp(name: &str, sz_decimals: u32, max_leverage: u32) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "szDecimals": sz_decimals,
        "maxLeverage": max_leverage,
        "onlyIsolated": false,
        "marginMode": null,
        "growthMode": "disabled"
    })
}

/// A `meta` reply for `universe`, collateralized in token 0 (USDC).
pub fn fixture_perp_meta(universe: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({ "universe": universe, "collateralToken": 0 })
}

pub fn fixture_perp_meta_btc_eth() -> serde_json::Value {
    fixture_perp_meta(vec![fixture_perp("BTC", 5, 50), fixture_perp("ETH", 4, 25)])
}

pub fn fixture_perp_meta_btc_only() -> serde_json::Value {
    fixture_perp_meta(vec![fixture_perp("BTC", 5, 50)])
}

pub fn fixture_spot_meta_usdc_only() -> serde_json::Value {
    serde_json::json!({
        "universe": [],
        "tokens": [
            {
                "name": "USDC",
                "index": 0,
                "tokenId": "0x00000000000000000000000000000000",
                "szDecimals": 6,
                "weiDecimals": 6,
                "evmContract": null
            }
        ]
    })
}

/// Spot metadata whose second pair references a missing token.
pub fn fixture_malformed_spot_meta() -> serde_json::Value {
    serde_json::json!({
        "universe": [
            {
                "name": "PURR/USDC",
                "index": 0,
                "tokens": [1, 0]
            },
            {
                "name": "BROKEN/USDC",
                "index": 1,
                "tokens": [9, 0]
            }
        ],
        "tokens": [
            {
                "name": "USDC",
                "index": 0,
                "tokenId": "0x00000000000000000000000000000000",
                "szDecimals": 6,
                "weiDecimals": 6,
                "evmContract": null
            },
            {
                "name": "PURR",
                "index": 1,
                "tokenId": "0x00000000000000000000000000000001",
                "szDecimals": 0,
                "weiDecimals": 0,
                "evmContract": null
            }
        ]
    })
}

pub fn fixture_basic_order(coin: &str, oid: u64) -> serde_json::Value {
    fixture_basic_order_with_cloid(coin, oid, None)
}

pub fn fixture_basic_order_with_cloid(
    coin: &str,
    oid: u64,
    cloid: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "timestamp": 1700000000000_u64,
        "coin": coin,
        "side": "B",
        "limitPx": "50000",
        "sz": "0.1",
        "oid": oid,
        "origSz": "0.1",
        "cloid": cloid,
        "orderType": "Limit",
        "tif": "Gtc",
        "reduceOnly": false
    })
}

pub fn fixture_fill(coin: &str, oid: u64) -> serde_json::Value {
    serde_json::json!({
        "coin": coin,
        "px": "50000",
        "sz": "0.1",
        "side": "B",
        "time": 1700000000000_u64,
        "startPosition": "0",
        "dir": "Open Long",
        "closedPnl": "0",
        "hash": "0xabc",
        "oid": oid,
        "crossed": true,
        "fee": "0.01",
        "tid": 43_u64,
        "cloid": null,
        "feeToken": "USDC",
        "liquidation": null
    })
}

/// A clearinghouse state with one BTC position.
pub fn fixture_clearinghouse_state(withdrawable: &str, leverage: u32) -> serde_json::Value {
    serde_json::json!({
        "marginSummary": {
            "accountValue": "10000",
            "totalNtlPos": "5000",
            "totalRawUsd": "10000",
            "totalMarginUsed": "1000"
        },
        "crossMarginSummary": {
            "accountValue": "10000",
            "totalNtlPos": "5000",
            "totalRawUsd": "10000",
            "totalMarginUsed": "1000"
        },
        "crossMaintenanceMarginUsed": "100",
        "withdrawable": withdrawable,
        "assetPositions": [
            {
                "type": "oneWay",
                "position": {
                    "coin": "BTC",
                    "szi": "0.1",
                    "leverage": {"type": "cross", "value": leverage},
                    "entryPx": "50000",
                    "positionValue": "5100",
                    "unrealizedPnl": "100",
                    "returnOnEquity": "0.1",
                    "liquidationPx": null,
                    "marginUsed": "1000",
                    "maxLeverage": 50,
                    "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                }
            }
        ],
        "time": 1700000000000_u64
    })
}

pub fn fixture_order_success_response(oid: u64) -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "response": {
            "type": "order",
            "data": {
                "statuses": [
                    {
                        "resting": {
                            "oid": oid,
                            "cloid": null
                        }
                    }
                ]
            }
        }
    })
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

pub async fn mount_market_meta(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_string_contains(r#""type":"meta""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub async fn mount_spot_meta(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_string_contains(r#""type":"spotMeta""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub async fn mount_perp_dexs(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_string_contains(r#""type":"perpDexs""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// Mount the public reads most read-only commands need.
pub async fn mount_common_public_endpoints(server: &MockServer) {
    mount_override_healthcheck(server).await;
    mount_market_meta(server, fixture_perp_meta_btc_eth()).await;
    mount_spot_meta(server, fixture_spot_meta_usdc_only()).await;
    mount_perp_dexs(server, serde_json::json!([])).await;
}

pub async fn mock_market_server() -> MockServer {
    let server = MockServer::start().await;
    mount_override_healthcheck(&server).await;
    mount_market_meta(&server, fixture_perp_meta_btc_eth()).await;
    mount_spot_meta(&server, fixture_spot_meta_usdc_only()).await;
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
                .set_body_json(fixture_clearinghouse_state(withdrawable, leverage)),
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
