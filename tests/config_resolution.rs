// Priority chains exercised through the real binary and a real config file:
//   private key: --private-key > HYPERLIQUID_PRIVATE_KEY > config file
//   network:     --testnet > HYPERLIQUID_NETWORK > config file > mainnet

use assert_cmd::Command;
mod support;

use support::exchange::ExchangeFixture;

use std::fs;
use tempfile::TempDir;

const CLI_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
const ENV_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000002";
const FILE_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000003";

/// A home whose config is found through the platform config-dir lookup
/// (XDG_CONFIG_HOME on Linux, ~/Library/Application Support on macOS), not
/// through a `HYPERLIQUID_CONFIG_DIR` override.
struct ConfigHome {
    _tmp: TempDir,
    home_dir: std::path::PathBuf,
    xdg_config_home: std::path::PathBuf,
}

impl ConfigHome {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home_dir = tmp.path().join("home");
        let xdg_config_home = tmp.path().join("xdg-config");
        let home = Self {
            _tmp: tmp,
            home_dir,
            xdg_config_home,
        };
        for dir in home.config_dirs() {
            fs::create_dir_all(dir).unwrap();
        }
        home
    }

    fn config_dirs(&self) -> [std::path::PathBuf; 2] {
        [
            self.xdg_config_home.join("hyperliquid"),
            self.home_dir
                .join("Library")
                .join("Application Support")
                .join("hyperliquid"),
        ]
    }

    fn write_config_text(&self, text: &str) {
        for dir in self.config_dirs() {
            fs::write(dir.join("config.json"), text).unwrap();
        }
    }

    fn write_config(&self, config: &serde_json::Value) {
        self.write_config_text(&serde_json::to_string_pretty(config).unwrap());
    }

    fn command(&self) -> Command {
        let mut command = support::hermetic_command();
        command
            .env("HOME", &self.home_dir)
            .env("XDG_CONFIG_HOME", &self.xdg_config_home);
        command
    }

    async fn resolved_network(&self, command: &mut Command) -> String {
        let server = ExchangeFixture::start().await;
        let output = command
            .env("HYPERLIQUID_API_BASE_URL", server.uri())
            .args(["--no-update-check", "--format", "json", "status"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        support::stdout_json(&output)["network"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

#[test]
fn private_key_comes_from_flag_then_env_then_config_file() {
    let home = ConfigHome::new();
    home.write_config(&serde_json::json!({ "private_key": FILE_KEY, "network": "mainnet" }));

    let mut flag = home.command();
    flag.env("HYPERLIQUID_PRIVATE_KEY", ENV_KEY).args([
        "--private-key",
        CLI_KEY,
        "wallet",
        "address",
    ]);
    let mut env = home.command();
    env.env("HYPERLIQUID_PRIVATE_KEY", ENV_KEY)
        .args(["wallet", "address"]);
    let mut file = home.command();
    file.args(["wallet", "address"]);

    for (mut command, key) in [(flag, CLI_KEY), (env, ENV_KEY), (file, FILE_KEY)] {
        command
            .assert()
            .success()
            .stdout(predicates::str::contains(support::expected_address(key)));
    }
}

#[tokio::test]
async fn network_comes_from_flag_then_env_then_config_file_then_mainnet() {
    struct Case {
        config: Option<&'static str>,
        env: Option<&'static str>,
        testnet_flag: bool,
        expected: &'static str,
    }
    let cases = [
        Case {
            config: Some("mainnet"),
            env: Some("mainnet"),
            testnet_flag: true,
            expected: "testnet",
        },
        Case {
            config: Some("bogus"),
            env: Some("bogus"),
            testnet_flag: true,
            expected: "testnet",
        },
        Case {
            config: Some("mainnet"),
            env: Some("testnet"),
            testnet_flag: false,
            expected: "testnet",
        },
        Case {
            config: Some("testnet"),
            env: Some("mainnet"),
            testnet_flag: false,
            expected: "mainnet",
        },
        Case {
            config: Some("testnet"),
            env: None,
            testnet_flag: false,
            expected: "testnet",
        },
        Case {
            config: None,
            env: None,
            testnet_flag: false,
            expected: "mainnet",
        },
    ];

    for case in cases {
        let home = ConfigHome::new();
        if let Some(network) = case.config {
            home.write_config(&serde_json::json!({ "network": network }));
        }
        let mut command = home.command();
        if let Some(network) = case.env {
            command.env("HYPERLIQUID_NETWORK", network);
        }
        if case.testnet_flag {
            command.arg("--testnet");
        }
        assert_eq!(
            home.resolved_network(&mut command).await,
            case.expected,
            "config {:?}, env {:?}, --testnet {}",
            case.config,
            case.env,
            case.testnet_flag
        );
    }
}

#[test]
fn invalid_network_env_returns_configuration_error_naming_the_variable() {
    let home = ConfigHome::new();

    home.command()
        .env("HYPERLIQUID_NETWORK", "bogus")
        .arg("status")
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("Configuration error"))
        .stdout(predicates::str::contains("HYPERLIQUID_NETWORK"))
        .stdout(predicates::str::contains("bogus"));
}

#[test]
fn malformed_config_file_returns_configuration_error() {
    let home = ConfigHome::new();
    home.write_config_text("{not valid json");

    home.command()
        .arg("status")
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("Configuration error"))
        .stdout(predicates::str::contains("invalid config file"));
}

#[test]
fn invalid_network_in_config_file_returns_configuration_error() {
    let home = ConfigHome::new();
    home.write_config(&serde_json::json!({ "network": "bogus" }));

    home.command()
        .arg("status")
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("Configuration error"))
        .stdout(predicates::str::contains("invalid config file"))
        .stdout(predicates::str::contains("bogus"));
}

/// A proxy that refuses every connection and remembers what it was asked to
/// connect to, to see where a command with no API override sends its reads.
struct RecordingProxy {
    address: std::net::SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecordingProxy {
    fn start() -> Self {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::Ordering;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread = {
            let (stop, seen) = (stop.clone(), seen.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut request_line = String::new();
                    BufReader::new(&stream)
                        .read_line(&mut request_line)
                        .unwrap();
                    seen.lock().unwrap().push(request_line.trim().to_string());
                    let _ =
                        stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
                }
            })
        };
        Self {
            address,
            stop,
            seen,
            thread: Some(thread),
        }
    }

    /// The request lines the proxy received, after stopping it.
    fn finish(mut self) -> Vec<String> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        self.seen.lock().unwrap().clone()
    }
}

#[test]
fn public_reads_go_to_each_networks_documented_api_host() {
    for (args, host) in [
        (
            &["--no-update-check", "mids"][..],
            "api.hyperliquid.xyz:443",
        ),
        (
            &["--no-update-check", "--testnet", "mids"],
            "api.hyperliquid-testnet.xyz:443",
        ),
    ] {
        let home = ConfigHome::new();
        let proxy = RecordingProxy::start();
        let url = format!("http://{}", proxy.address);

        home.command()
            .env("HTTPS_PROXY", &url)
            .env("HTTP_PROXY", &url)
            .env("ALL_PROXY", &url)
            .args(args)
            .assert()
            .failure();

        let seen = proxy.finish();
        assert!(!seen.is_empty(), "{args:?} reached no host");
        for line in &seen {
            assert_eq!(line, &format!("CONNECT {host} HTTP/1.1"), "{args:?}");
        }
    }
}
