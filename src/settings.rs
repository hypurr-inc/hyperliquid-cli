//! Environment variable names and the layered settings built from flags,
//! environment and the config file.
//!
//! Every environment variable the CLI reads is a constant in [`env`]; the root
//! help text and the test harness's list of variables to clear are generated
//! from the same table. [`Settings`] reads the environment through an
//! [`EnvSource`] and parses the config file at most once, lazily, so a flag that
//! settles a question never touches a broken config file. Every write to the
//! config file goes through [`Settings::update_config`], which owns the
//! lock, fresh read, edit and save sequence.

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};

use crate::config::{BankrLogin, Config, ConfigFile};
use crate::errors::CliError;
use crate::network::Network;
use crate::private_fs::FileLock;

/// One row per environment variable, in the order the root help lists them. A
/// row with empty help is read by the CLI but not advertised.
macro_rules! env_vars {
    ($($(#[$meta:meta])* $name:ident = $value:literal => $help:literal;)*) => {
        pub mod env {
            $($(#[$meta])* pub const $name: &str = $value;)*

            /// Every variable the CLI reads.
            pub const ALL: &[&str] = &[$($value),*];
        }

        /// The `Environment:` section of the root help, in display order.
        const ENV_HELP: &[(&str, &str)] = &[$(($value, $help)),*];
    };
}

env_vars! {
    FORMAT = "HYPERLIQUID_FORMAT" => "Default output format: pretty, table, or json. --format overrides it.";
    AGENT = "HYPERLIQUID_AGENT" => "Set to 1 for JSON output and no prompts.";
    NETWORK = "HYPERLIQUID_NETWORK" => "Network to use: mainnet or testnet. --testnet overrides it.";
    /// Override the home used for Bankr credentials and the default OWS vault.
    HOME_DIR = "HYPERLIQUID_HOME_DIR" => "Home used for Bankr credentials and the default OWS vault, instead of platform discovery.";
    /// Override the CLI config directory, including the hyperliquid component.
    CONFIG_DIR = "HYPERLIQUID_CONFIG_DIR" => "Config directory, including the hyperliquid component, instead of platform discovery.";
    /// Override the CLI data directory, including the hyperliquid component.
    DATA_DIR = "HYPERLIQUID_DATA_DIR" => "Data directory, including the hyperliquid component, instead of platform discovery.";
    /// Shared with the Bankr CLI.
    BANKR_API_KEY = "BANKR_API_KEY" => "Bankr API key for --bankr, shared with the Bankr CLI. `wallet login` stores one instead.";
    BANKR_API_URL = "BANKR_API_URL" => "Bankr API host, for a local or alternate endpoint.";
    PRIVATE_KEY = "HYPERLIQUID_PRIVATE_KEY" => "Private key for signing. Prefer it over --private-key.";
    KEYSTORE_PASSWORD = "HYPERLIQUID_KEYSTORE_PASSWORD" => "Password for --keystore. Prefer it over --keystore-password.";
    API_BASE_URL = "HYPERLIQUID_API_BASE_URL" => "Override the API endpoint. Loopback hosts only, for testing.";
    MAINNET_API_BASE_URL = "HYPERLIQUID_MAINNET_API_BASE_URL" => "Same override, mainnet only.";
    TESTNET_API_BASE_URL = "HYPERLIQUID_TESTNET_API_BASE_URL" => "Same override, testnet only.";
    OWS_VAULT_PATH = "HYPERLIQUID_OWS_VAULT_PATH" => "OWS vault directory. Defaults to ~/.hyperliquid.";
    OWS_PASSPHRASE = "OWS_PASSPHRASE" => "Passphrase for the OWS vault.";
    ACCOUNT_KEY_PASSPHRASE = "HYPERLIQUID_ACCOUNT_KEY_PASSPHRASE" => "Passphrase that unlocks pre-vault stored accounts, instead of the OS keychain.";
    ACCOUNT_KEYCHAIN_DISABLED = "HYPERLIQUID_ACCOUNT_KEYCHAIN_DISABLED" => "Set to skip the OS keychain when unlocking pre-vault stored accounts.";
    /// Removed: it persisted raw account encryption keys. Setting it is an error.
    ACCOUNT_KEY_STORE_DIR = "HYPERLIQUID_ACCOUNT_KEY_STORE_DIR" => "";
    DEFAULT_BUILDER_ADDRESS = "HYPERLIQUID_DEFAULT_BUILDER_ADDRESS" => "Default builder address for orders and setup suggestions.";
    DEFAULT_BUILDER_FEE_RATE = "HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE" => "Default builder fee rate, a percent string such as 0.001%.";
    DEFAULT_REFERRAL_CODE = "HYPERLIQUID_DEFAULT_REFERRAL_CODE" => "Default referral code for `referral set` and setup.";
    NO_UPDATE_CHECK = "HYPERLIQUID_NO_UPDATE_CHECK" => "Set to 1 to skip release update checks.";
    NO_UPDATE_NOTIFIER = "NO_UPDATE_NOTIFIER" => "Set to a non-empty value to skip the update notice.";
    AUTO_UPDATE = "HYPERLIQUID_AUTO_UPDATE" => "Set to 1 to automatically update binary in place when a newer release is detected.";
    MCP_ALLOW_UNSAFE = "HYPERLIQUID_MCP_ALLOW_UNSAFE" => "Set to 1 to let `mcp` expose commands beyond the read-only set.";
    MCP_TIMEOUT_MS = "HYPERLIQUID_MCP_TIMEOUT_MS" => "Execution deadline for `mcp` calls, in milliseconds (default 30000).";
    WATCH_MAX_TICKS = "HYPERLIQUID_WATCH_MAX_TICKS" => "Tick bound for --watch when --max-ticks is not given.";
    SUBSCRIBE_MAX_EVENTS = "HYPERLIQUID_SUBSCRIBE_MAX_EVENTS" => "Event bound for subscribe when no bound option is given.";
    NO_COLOR = "NO_COLOR" => "Disable color when set to a non-empty value.";
    FORCE_COLOR = "FORCE_COLOR" => "Force color on, or off when set to 0 or false.";
    CI = "CI" => "Treat the run as non-interactive: no prompts.";
    CARGO_HOME = "CARGO_HOME" => "";
}

/// Compile-time builder default captured from the build environment.
///
/// Set `HYPERLIQUID_DEFAULT_BUILDER_ADDRESS` while building the binary to bake
/// in a default builder address. A runtime variable with the same name still
/// overrides it. Empty by default in the upstream repo. `option_env!` needs the
/// literal name, which must match [`env::DEFAULT_BUILDER_ADDRESS`].
pub const BUILD_DEFAULT_BUILDER_ADDRESS: &str =
    match option_env!("HYPERLIQUID_DEFAULT_BUILDER_ADDRESS") {
        Some(address) => address,
        None => "",
    };

/// Compile-time default builder fee rate, a percent string such as `0.001%`.
/// Pairs with [`BUILD_DEFAULT_BUILDER_ADDRESS`]; see [`env::DEFAULT_BUILDER_FEE_RATE`].
pub const BUILD_DEFAULT_BUILDER_FEE_RATE: &str =
    match option_env!("HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE") {
        Some(rate) => rate,
        None => "",
    };

/// Compile-time default referral code. A runtime variable with the same name
/// still overrides it. Empty by default in the upstream repo, meaning no mainnet
/// default. See [`env::DEFAULT_REFERRAL_CODE`].
pub const BUILD_DEFAULT_REFERRAL_CODE: &str = match option_env!("HYPERLIQUID_DEFAULT_REFERRAL_CODE")
{
    Some(code) => code,
    None => "",
};

const TESTNET_REFERRAL_FALLBACK: &str = "TESTNET";

/// The `Environment:` block of the root help text.
#[must_use]
pub fn env_help_section() -> String {
    let advertised = || ENV_HELP.iter().filter(|(_, description)| !description.is_empty());
    let width = advertised()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or_default()
        + 2;
    let mut section = String::from("Environment:\n");
    for (name, description) in advertised() {
        section.push_str(&format!("  {name:<width$}{description}\n"));
    }
    section
}

/// Where environment variables come from. Tests inject a fixed map instead of
/// mutating the process environment.
#[derive(Debug, Clone, Default)]
pub enum EnvSource {
    #[default]
    Process,
    Fixed(HashMap<String, OsString>),
}

impl EnvSource {
    pub fn fixed<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<OsString>,
    {
        Self::Fixed(
            pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
    }

    pub(crate) fn get_os(&self, name: &str) -> Option<OsString> {
        match self {
            Self::Process => std::env::var_os(name),
            Self::Fixed(map) => map.get(name).cloned(),
        }
    }

    /// The value when set and valid Unicode.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        self.get_os(name).and_then(|value| value.into_string().ok())
    }

    /// The value when set and non-empty.
    #[must_use]
    pub fn get_non_empty(&self, name: &str) -> Option<String> {
        self.get(name).filter(|value| !value.is_empty())
    }

    /// The trimmed value when set and non-blank.
    #[must_use]
    pub fn get_trimmed(&self, name: &str) -> Option<String> {
        self.get(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Whether the variable is set at all, even to an empty or non-Unicode value.
    #[must_use]
    pub fn is_set(&self, name: &str) -> bool {
        self.get_os(name).is_some()
    }

    /// The value with invalid Unicode replaced, when set.
    #[must_use]
    pub fn get_lossy(&self, name: &str) -> Option<String> {
        self.get_os(name)
            .map(|value| value.to_string_lossy().into_owned())
    }
}

/// A builder address/fee pair from one settings layer, not yet validated.
/// The labels name the layer's fields in validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderDefaultSetting {
    pub address: Option<String>,
    pub fee_rate: Option<String>,
    pub address_label: &'static str,
    pub fee_label: &'static str,
}

enum ConfigSource {
    File(ConfigFile),
    Preloaded(Option<Arc<Config>>),
    Absent,
}

/// What this run has read of the config file. Cleared by every write, so a
/// read after [`Settings::update_config`] sees the new contents.
#[derive(Default)]
struct ConfigCache {
    text: Option<Result<Option<Arc<str>>, String>>,
    config: Option<Result<Option<Arc<Config>>, String>>,
}

/// Hygiene warnings describe the file, not a reader, so a process prints them once.
static HYGIENE_ANNOUNCED: Once = Once::new();

/// Flags, environment and config file, resolved lazily.
pub struct Settings {
    env: EnvSource,
    source: ConfigSource,
    announce_hygiene: bool,
    cache: Mutex<ConfigCache>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings").finish_non_exhaustive()
    }
}

#[derive(serde::Deserialize)]
struct PrivateKeyOnly {
    private_key: Option<String>,
}

/// A locked read-modify-write of the config file, begun by
/// [`Settings::begin_config_update`]. Holding it holds the lock; the file is
/// written only by [`Self::commit`].
pub struct ConfigTransaction<'a> {
    settings: &'a Settings,
    file: &'a ConfigFile,
    lock: FileLock,
    before: Config,
}

impl ConfigTransaction<'_> {
    /// The config as the file held it when the lock was taken (the default for a
    /// missing file).
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.before
    }

    /// Apply `edit`, save when it changed anything, and return the config keys
    /// it changed (set, changed or cleared).
    pub fn commit(self, edit: impl FnOnce(&mut Config)) -> Result<Vec<String>, anyhow::Error> {
        let mut after = self.before.clone();
        edit(&mut after);
        let changed = changed_keys(&self.before, &after);
        if !changed.is_empty() {
            self.file.save(&after, &self.lock)?;
        }
        self.settings.forget_config();
        Ok(changed)
    }
}

/// The top-level config keys whose stored value differs between `before` and `after`.
fn changed_keys(before: &Config, after: &Config) -> Vec<String> {
    let fields = |config: &Config| match serde_json::to_value(config) {
        Ok(serde_json::Value::Object(fields)) => fields,
        _ => serde_json::Map::new(),
    };
    let (before, after) = (fields(before), fields(after));
    let mut keys: Vec<String> = after
        .iter()
        .filter(|(key, value)| before.get(*key) != Some(*value))
        .map(|(key, _)| key.clone())
        .chain(before.keys().filter(|key| !after.contains_key(*key)).cloned())
        .collect();
    keys.sort();
    keys
}

impl Settings {
    /// The real environment and config file. Config hygiene warnings (plaintext
    /// key, loose permissions) print to stderr the first time the file is parsed.
    #[must_use]
    pub fn from_process() -> Self {
        Self::build(
            EnvSource::Process,
            ConfigFile::default_location(&EnvSource::Process)
                .map_or(ConfigSource::Absent, ConfigSource::File),
            true,
        )
    }

    /// Settings over `env` and an optional config file. Never prints hygiene warnings.
    #[must_use]
    pub fn new(env: EnvSource, file: Option<ConfigFile>) -> Self {
        Self::build(
            env,
            file.map_or(ConfigSource::Absent, ConfigSource::File),
            false,
        )
    }

    /// Settings over `env` with an in-memory config and no file.
    #[must_use]
    pub fn detached(env: EnvSource, config: Option<Config>) -> Self {
        Self::build(env, ConfigSource::Preloaded(config.map(Arc::new)), false)
    }

    /// The same settings without hygiene warnings.
    #[must_use]
    pub fn quiet(mut self) -> Self {
        self.announce_hygiene = false;
        self
    }

    fn build(env: EnvSource, source: ConfigSource, announce_hygiene: bool) -> Self {
        Self {
            env,
            source,
            announce_hygiene,
            cache: Mutex::new(ConfigCache::default()),
        }
    }

    fn cache(&self) -> MutexGuard<'_, ConfigCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn forget_config(&self) {
        *self.cache() = ConfigCache::default();
    }

    #[must_use]
    pub fn env(&self) -> &EnvSource {
        &self.env
    }

    #[must_use]
    pub fn config_file(&self) -> Option<&ConfigFile> {
        match &self.source {
            ConfigSource::File(file) => Some(file),
            ConfigSource::Preloaded(_) | ConfigSource::Absent => None,
        }
    }

    fn require_config_file(&self) -> Result<&ConfigFile, anyhow::Error> {
        self.config_file()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine config directory"))
    }

    /// Lock the config file and read it fresh, for an edit that another process
    /// may race.
    pub fn begin_config_update(&self) -> Result<ConfigTransaction<'_>, anyhow::Error> {
        let file = self.require_config_file()?;
        let lock = file.lock()?;
        let before = file.read()?;
        if let Some(config) = &before {
            self.note_parsed(file, config.has_plaintext_key());
        }
        Ok(ConfigTransaction {
            settings: self,
            file,
            lock,
            before: before.unwrap_or_default(),
        })
    }

    /// Edit the config under its lock. Returns the keys the edit changed; the
    /// file is untouched when it changed none.
    pub fn update_config(
        &self,
        edit: impl FnOnce(&mut Config),
    ) -> Result<Vec<String>, anyhow::Error> {
        self.begin_config_update()?.commit(edit)
    }

    /// Delete the config file under its lock. A file that is already gone is success.
    pub fn remove_config(&self) -> Result<(), anyhow::Error> {
        let file = self.require_config_file()?;
        let lock = file.lock()?;
        file.remove(&lock)?;
        self.forget_config();
        Ok(())
    }

    fn note_parsed(&self, file: &ConfigFile, has_plaintext_key: bool) {
        if !self.announce_hygiene {
            return;
        }
        HYGIENE_ANNOUNCED.call_once(|| {
            for line in file.hygiene_warnings(has_plaintext_key) {
                eprintln!("{line}");
            }
        });
    }

    fn file_text(&self, file: &ConfigFile) -> Result<Option<Arc<str>>, CliError> {
        self.cache()
            .text
            .get_or_insert_with(|| {
                file.read_text()
                    .map(|text| text.map(Arc::from))
                    .map_err(configuration_message)
            })
            .clone()
            .map_err(CliError::Configuration)
    }

    /// The parsed config file. `Ok(None)` when there is no file.
    pub fn config(&self) -> Result<Option<Arc<Config>>, CliError> {
        let file = match &self.source {
            ConfigSource::File(file) => file,
            ConfigSource::Preloaded(config) => return Ok(config.clone()),
            ConfigSource::Absent => return Ok(None),
        };
        let cached = self.cache().config.clone();
        let parsed = match cached {
            Some(parsed) => parsed,
            None => {
                let parsed = self
                    .file_text(file)
                    .and_then(|text| text.map(|text| file.parse(&text)).transpose())
                    .map(|config| config.map(Arc::new))
                    .map_err(configuration_message);
                if let Ok(Some(config)) = &parsed {
                    self.note_parsed(file, config.has_plaintext_key());
                }
                self.cache().config = Some(parsed.clone());
                parsed
            }
        };
        parsed.map_err(CliError::Configuration)
    }

    /// The config as the defaults resolvers see it: parse failures read as
    /// "failed to load config".
    fn config_for_defaults(&self) -> Result<Option<Arc<Config>>, CliError> {
        self.config()
            .map_err(|err| CliError::Configuration(format!("failed to load config: {err}")))
    }

    fn file_private_key(&self) -> Result<Option<String>, CliError> {
        let file = match &self.source {
            ConfigSource::File(file) => file,
            ConfigSource::Preloaded(config) => {
                return Ok(config
                    .as_ref()
                    .and_then(|config| config.plaintext_key().map(str::to_string)));
            }
            ConfigSource::Absent => return Ok(None),
        };
        let Some(text) = self.file_text(file)? else {
            return Ok(None);
        };
        // Only this field is read, so an unrelated bad field such as `network`
        // doesn't block a run that signs with the key.
        let parsed: PrivateKeyOnly = file.parse_with(&text)?;
        let key = parsed.private_key.filter(|key| !key.is_empty());
        self.note_parsed(file, key.is_some());
        Ok(key)
    }

/// Whether to use testnet. Flag, then `HYPERLIQUID_NETWORK`, then the config file; mainnet by default.
    pub fn testnet(&self, cli_testnet: bool) -> Result<bool, CliError> {
        if cli_testnet {
            return Ok(true);
        }
        if let Some(network) = self.env.get(env::NETWORK) {
            return Ok(Network::parse_setting(env::NETWORK, &network)?.is_testnet());
        }
        Ok(self
            .config()?
            .map(|config| config.network)
            .unwrap_or_default()
            .is_testnet())
    }

    pub fn network(&self, cli_testnet: bool) -> Result<Network, CliError> {
        self.testnet(cli_testnet).map(Network::from_testnet)
    }

    /// The signing private key. Flag, then `HYPERLIQUID_PRIVATE_KEY`, then the
    /// legacy plaintext `private_key` in the config file. Empty values are skipped.
    pub fn private_key(&self, cli_key: Option<&str>) -> Result<Option<String>, CliError> {
        if let Some(key) = cli_key.filter(|key| !key.is_empty()) {
            return Ok(Some(key.to_string()));
        }
        if let Some(key) = self.env.get_non_empty(env::PRIVATE_KEY) {
            return Ok(Some(key));
        }
        self.file_private_key()
    }

    /// The loopback-only API endpoint override for `network`. The network-specific
    /// variable wins over `HYPERLIQUID_API_BASE_URL`.
    pub fn api_base_url_override(
        &self,
        network: Network,
    ) -> Result<Option<reqwest::Url>, CliError> {
        let network_var = match network {
            Network::Mainnet => env::MAINNET_API_BASE_URL,
            Network::Testnet => env::TESTNET_API_BASE_URL,
        };
        for name in [network_var, env::API_BASE_URL] {
            if let Some(url) = parse_api_override(name, self.env.get(name).as_deref())? {
                return Ok(Some(url));
            }
        }
        Ok(None)
    }

    pub fn default_wallet_id(&self) -> Result<Option<String>, CliError> {
        Ok(self
            .config()?
            .and_then(|config| config.default_wallet_id.clone()))
    }

    pub fn ows_vault_path(&self) -> Option<PathBuf> {
        self.env
            .get_non_empty(env::OWS_VAULT_PATH)
            .map(PathBuf::from)
            .or_else(|| crate::paths::default_vault_dir(&self.env))
    }

    pub fn ows_passphrase(&self) -> Option<String> {
        self.env.get_non_empty(env::OWS_PASSPHRASE)
    }

    pub fn keystore_password(&self) -> Option<String> {
        self.env.get_non_empty(env::KEYSTORE_PASSWORD)
    }

    /// What `wallet login` stored for Bankr; empty when nothing is stored.
    pub fn bankr_login(&self) -> Result<BankrLogin, CliError> {
        Ok(self
            .config()?
            .map(|config| config.bankr.clone())
            .unwrap_or_default())
    }

    fn env_builder_default(&self) -> Option<BuilderDefaultSetting> {
        let address = self.env.get_trimmed(env::DEFAULT_BUILDER_ADDRESS);
        let fee_rate = self.env.get_trimmed(env::DEFAULT_BUILDER_FEE_RATE);
        (address.is_some() || fee_rate.is_some()).then_some(BuilderDefaultSetting {
            address,
            fee_rate,
            address_label: env::DEFAULT_BUILDER_ADDRESS,
            fee_label: env::DEFAULT_BUILDER_FEE_RATE,
        })
    }

    fn layered_builder_default(&self, config: Option<&Config>) -> Option<BuilderDefaultSetting> {
        if let Some(setting) = self.env_builder_default() {
            return Some(setting);
        }
        if let Some(config) = config {
            let trimmed = |value: &Option<String>| {
                value
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let address = trimmed(&config.default_builder_address);
            let fee_rate = trimmed(&config.default_builder_fee_rate);
            if address.is_some() || fee_rate.is_some() {
                return Some(BuilderDefaultSetting {
                    address,
                    fee_rate,
                    address_label: "default_builder_address",
                    fee_label: "default_builder_fee_rate",
                });
            }
        }
        let non_empty = |value: &str| (!value.is_empty()).then(|| value.to_string());
        let address = non_empty(BUILD_DEFAULT_BUILDER_ADDRESS);
        let fee_rate = non_empty(BUILD_DEFAULT_BUILDER_FEE_RATE);
        (address.is_some() || fee_rate.is_some()).then_some(BuilderDefaultSetting {
            address,
            fee_rate,
            address_label: "DEFAULT_BUILDER_ADDRESS",
            fee_label: "DEFAULT_BUILDER_FEE_RATE",
        })
    }

    /// The default builder pair: environment, then config file, then the build-time default.
    /// The config file is only read when the environment sets neither value.
    pub fn default_builder(&self) -> Result<Option<BuilderDefaultSetting>, CliError> {
        if let Some(setting) = self.env_builder_default() {
            return Ok(Some(setting));
        }
        let config = self.config_for_defaults()?;
        Ok(self.layered_builder_default(config.as_deref()))
    }

    /// The builder pair setup and wallet creation would offer: environment, then
    /// the build-time default. The config file is the thing being filled in, so it is skipped.
    #[must_use]
    pub fn suggested_default_builder(&self) -> Option<BuilderDefaultSetting> {
        self.layered_builder_default(None)
    }

    fn layered_referral_code(&self, is_testnet: bool, config: Option<&Config>) -> Option<String> {
        if let Some(code) = self.env.get_trimmed(env::DEFAULT_REFERRAL_CODE) {
            return Some(code);
        }
        if let Some(code) = config
            .and_then(|config| config.default_referral_code.as_deref())
            .map(str::trim)
            .filter(|code| !code.is_empty())
        {
            return Some(code.to_string());
        }
        if !BUILD_DEFAULT_REFERRAL_CODE.is_empty() {
            return Some(BUILD_DEFAULT_REFERRAL_CODE.to_string());
        }
        is_testnet.then(|| TESTNET_REFERRAL_FALLBACK.to_string())
    }

    /// The default referral code: environment, config file, build-time default,
    /// then `TESTNET` on testnet. `None` on mainnet means the caller needs an explicit code.
    pub fn default_referral_code(&self, is_testnet: bool) -> Result<Option<String>, CliError> {
        let config = self.config_for_defaults()?;
        Ok(self.layered_referral_code(is_testnet, config.as_deref()))
    }

    /// The referral code setup and wallet creation would offer, without the config file.
    #[must_use]
    pub fn suggested_default_referral_code(&self, is_testnet: bool) -> Option<String> {
        self.layered_referral_code(is_testnet, None)
    }
}

fn configuration_message(err: CliError) -> String {
    match err {
        CliError::Configuration(message) => message,
        other => other.to_string(),
    }
}

/// Whether `url` names this machine: `localhost` or a loopback address, IPv4 or
/// IPv6 (URLs bracket IPv6 hosts).
#[must_use]
pub fn is_loopback_url(url: &reqwest::Url) -> bool {
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Parse a hidden API endpoint override. Overrides are limited to loopback hosts
/// so tests can point the CLI at a local mock without routing real trading or
/// market-data traffic through an arbitrary proxy. `name` is the variable that
/// supplied `value`.
fn parse_api_override(name: &str, value: Option<&str>) -> Result<Option<reqwest::Url>, CliError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    let url = reqwest::Url::parse(value)
        .map_err(|err| CliError::Configuration(format!("{name} must be a valid URL: {err}")))?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(CliError::Configuration(format!(
            "{name} must use http or https"
        )));
    }

    if url.host_str().is_none() {
        return Err(CliError::Configuration(format!(
            "{name} must include a host"
        )));
    }

    if !is_loopback_url(&url) {
        return Err(CliError::Configuration(format!(
            "{name} is only supported for localhost/loopback test endpoints"
        )));
    }

    Ok(Some(url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FILE_KEY: &str = "0x00000000000000000000000000000000000000000000000000000000000000f1";

    fn settings(env: &[(&str, &str)], config: Option<Config>) -> Settings {
        Settings::detached(EnvSource::fixed(env.iter().copied()), config)
    }

    fn testnet_config() -> Config {
        Config {
            network: Network::Testnet,
            ..Config::default()
        }
    }

    fn file_settings(env: &[(&str, &str)], contents: &str) -> (TempDir, Settings) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, contents).unwrap();
        let settings = Settings::new(
            EnvSource::fixed(env.iter().copied()),
            Some(ConfigFile::at(path)),
        );
        (tmp, settings)
    }

    #[test]
    fn network_precedence_is_flag_then_env_then_file_then_mainnet() {
        let from_file = || Some(testnet_config());
        assert!(settings(&[], None).testnet(true).unwrap());
        assert!(!settings(&[], None).testnet(false).unwrap());
        assert!(settings(&[], from_file()).testnet(false).unwrap());
        assert!(
            !settings(&[(env::NETWORK, "mainnet")], from_file())
                .testnet(false)
                .unwrap()
        );
        assert!(
            settings(&[(env::NETWORK, "TestNet")], None)
                .testnet(false)
                .unwrap()
        );
        assert!(
            settings(&[(env::NETWORK, "bogus")], from_file())
                .testnet(true)
                .unwrap()
        );
    }

    #[test]
    fn invalid_network_env_names_the_variable() {
        let err = settings(&[(env::NETWORK, "bogus")], None)
            .testnet(false)
            .unwrap_err();
        assert!(matches!(err, CliError::Configuration(_)));
        assert!(err.to_string().contains(env::NETWORK));
        assert!(err.to_string().contains("'bogus'"));
    }

    #[test]
    fn invalid_network_in_file_is_a_configuration_error() {
        let (_tmp, settings) = file_settings(&[], r#"{"network":"bogus"}"#);
        let err = settings.testnet(false).unwrap_err();
        assert!(err.to_string().contains("invalid config file"));
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn bankr_login_is_empty_without_a_config_and_read_whole_from_one() {
        assert_eq!(settings(&[], None).bankr_login().unwrap(), BankrLogin::default());
        let login = BankrLogin::new(
            "bk_key",
            None,
            "0x0000000000000000000000000000000000000009".parse().unwrap(),
        );
        let config = Config {
            bankr: login.clone(),
            ..Config::default()
        };
        assert_eq!(settings(&[], Some(config)).bankr_login().unwrap(), login);
    }

    #[test]
    fn private_key_precedence_skips_empty_values() {
        let from_file = || {
            Some(Config {
                private_key: Some(FILE_KEY.to_string()),
                ..Config::default()
            })
        };
        let resolve = |env: &[(&str, &str)], cli: Option<&str>| {
            settings(env, from_file()).private_key(cli).unwrap()
        };
        assert_eq!(
            resolve(&[(env::PRIVATE_KEY, "env")], Some("cli")).unwrap(),
            "cli"
        );
        assert_eq!(resolve(&[(env::PRIVATE_KEY, "env")], None).unwrap(), "env");
        assert_eq!(
            resolve(&[(env::PRIVATE_KEY, "env")], Some("")).unwrap(),
            "env"
        );
        assert_eq!(resolve(&[(env::PRIVATE_KEY, "")], None).unwrap(), FILE_KEY);
        assert_eq!(settings(&[], None).private_key(None).unwrap(), None);
    }

    #[test]
    fn private_key_ignores_unrelated_invalid_file_fields() {
        let (_tmp, settings) = file_settings(
            &[],
            &format!(r#"{{"private_key":"{FILE_KEY}","network":"bogus"}}"#),
        );
        assert_eq!(
            settings.private_key(None).unwrap().as_deref(),
            Some(FILE_KEY)
        );
        assert!(settings.testnet(false).is_err());
    }

    #[test]
    fn flags_settle_questions_without_reading_a_broken_file() {
        let (_tmp, settings) = file_settings(&[(env::PRIVATE_KEY, "env")], "{not json");
        assert!(settings.testnet(true).unwrap());
        assert_eq!(settings.private_key(None).unwrap().as_deref(), Some("env"));
        assert!(settings.testnet(false).is_err());
        assert!(settings.private_key(Some("")).is_ok());
    }

    #[test]
    fn config_is_parsed_once() {
        let (tmp, settings) = file_settings(&[], r#"{"network":"testnet"}"#);
        assert!(settings.testnet(false).unwrap());
        std::fs::write(tmp.path().join("config.json"), r#"{"network":"mainnet"}"#).unwrap();
        assert!(settings.testnet(false).unwrap());
    }

    #[test]
    fn update_config_is_visible_to_the_same_settings_and_reports_changed_keys() {
        let (tmp, settings) = file_settings(&[], r#"{"network":"testnet"}"#);
        assert!(settings.testnet(false).unwrap());

        let changed = settings
            .update_config(|config| {
                config.network = Network::Mainnet;
                config.default_wallet_id = Some("w".to_string());
            })
            .unwrap();

        assert_eq!(changed, ["default_wallet_id", "network"]);
        assert!(!settings.testnet(false).unwrap());
        assert_eq!(settings.default_wallet_id().unwrap().as_deref(), Some("w"));
        let on_disk = std::fs::read_to_string(tmp.path().join("config.json")).unwrap();
        assert!(on_disk.contains("\"default_wallet_id\""));
    }

    #[test]
    fn update_config_without_a_change_leaves_the_file_alone() {
        let (tmp, settings) = file_settings(&[], r#"{"network":"testnet"}"#);
        let changed = settings.update_config(|_| {}).unwrap();
        assert!(changed.is_empty());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("config.json")).unwrap(),
            r#"{"network":"testnet"}"#
        );

        let missing = Settings::new(
            EnvSource::fixed::<&str, &str>([]),
            Some(ConfigFile::at(tmp.path().join("absent.json"))),
        );
        assert!(missing.update_config(|_| {}).unwrap().is_empty());
        assert!(!tmp.path().join("absent.json").exists());
    }

    #[test]
    fn update_config_refuses_a_broken_file_instead_of_overwriting_it() {
        let (tmp, settings) = file_settings(&[], "{not json");
        let err = settings
            .update_config(|config| config.default_wallet_id = Some("w".to_string()))
            .unwrap_err();
        assert!(err.to_string().contains("invalid config file"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("config.json")).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn settings_without_a_file_cannot_be_updated() {
        let err = settings(&[], None)
            .update_config(|config| config.default_wallet_id = Some("w".to_string()))
            .unwrap_err();
        assert!(err.to_string().contains("config directory"));
    }

    #[test]
    fn remove_config_deletes_the_file_and_forgets_its_contents() {
        let (tmp, settings) = file_settings(&[], r#"{"network":"testnet"}"#);
        assert!(settings.testnet(false).unwrap());
        settings.remove_config().unwrap();
        assert!(!tmp.path().join("config.json").exists());
        assert!(!settings.testnet(false).unwrap());
        settings.remove_config().unwrap();
    }

    #[test]
    fn directory_overrides_follow_the_settings_environment_not_the_process() {
        let env = EnvSource::fixed([
            (env::CONFIG_DIR, "/cfg/hyperliquid"),
            (env::HOME_DIR, "/home/fixed"),
        ]);
        assert_eq!(
            ConfigFile::default_location(&env).unwrap().path(),
            std::path::Path::new("/cfg/hyperliquid/config.json")
        );
        assert_eq!(
            Settings::new(env, None).ows_vault_path().unwrap(),
            std::path::Path::new("/home/fixed").join(crate::paths::DEFAULT_VAULT_DIR)
        );
    }

    #[test]
    fn api_override_prefers_the_network_variable_and_allows_only_loopback() {
        let both = settings(
            &[
                (env::API_BASE_URL, "http://127.0.0.1:4011"),
                (env::TESTNET_API_BASE_URL, "http://localhost:4012"),
            ],
            None,
        );
        assert_eq!(
            both.api_base_url_override(Network::Testnet)
                .unwrap()
                .unwrap()
                .as_str(),
            "http://localhost:4012/"
        );
        assert_eq!(
            both.api_base_url_override(Network::Mainnet)
                .unwrap()
                .unwrap()
                .as_str(),
            "http://127.0.0.1:4011/"
        );
    }

    #[test]
    fn api_override_errors_name_the_variable_that_supplied_the_value() {
        let err = settings(&[(env::MAINNET_API_BASE_URL, "https://example.com")], None)
            .api_base_url_override(Network::Mainnet)
            .unwrap_err();
        assert!(err.to_string().contains(env::MAINNET_API_BASE_URL));
        assert!(err.to_string().contains("localhost/loopback"));

        let err = settings(&[(env::API_BASE_URL, "ftp://127.0.0.1")], None)
            .api_base_url_override(Network::Mainnet)
            .unwrap_err();
        assert!(err.to_string().contains(env::API_BASE_URL));
        assert!(err.to_string().contains("http or https"));
    }

    #[test]
    fn builder_default_layers_env_then_config() {
        let address = "0x0000000000000000000000000000000000000abc";
        let config = Config {
            default_builder_address: Some(address.to_string()),
            default_builder_fee_rate: Some("0.001%".to_string()),
            ..Config::default()
        };
        let from_config = settings(&[], Some(config.clone()))
            .default_builder()
            .unwrap()
            .unwrap();
        assert_eq!(from_config.address.as_deref(), Some(address));
        assert_eq!(from_config.address_label, "default_builder_address");

        let from_env = settings(
            &[
                (env::DEFAULT_BUILDER_ADDRESS, "  0xenv "),
                (env::DEFAULT_BUILDER_FEE_RATE, "0.002%"),
            ],
            Some(config),
        )
        .default_builder()
        .unwrap()
        .unwrap();
        assert_eq!(from_env.address.as_deref(), Some("0xenv"));
        assert_eq!(from_env.address_label, env::DEFAULT_BUILDER_ADDRESS);
    }

    #[test]
    fn builder_suggestion_skips_the_config_file() {
        let config = Config {
            default_builder_address: Some("0xabc".to_string()),
            default_builder_fee_rate: Some("0.001%".to_string()),
            ..Config::default()
        };
        let suggestion = settings(&[], Some(config)).suggested_default_builder();
        assert_eq!(
            suggestion.is_some(),
            !BUILD_DEFAULT_BUILDER_ADDRESS.is_empty() || !BUILD_DEFAULT_BUILDER_FEE_RATE.is_empty()
        );
    }

    #[test]
    fn referral_code_layers_env_config_build_then_testnet() {
        let configured = Config {
            default_referral_code: Some(" FROMFILE ".to_string()),
            ..Config::default()
        };
        assert_eq!(
            settings(
                &[(env::DEFAULT_REFERRAL_CODE, "  CODE  ")],
                Some(configured.clone())
            )
            .default_referral_code(false)
            .unwrap()
            .as_deref(),
            Some("CODE")
        );
        assert_eq!(
            settings(&[], Some(configured))
                .default_referral_code(false)
                .unwrap()
                .as_deref(),
            Some("FROMFILE")
        );
        if BUILD_DEFAULT_REFERRAL_CODE.is_empty() {
            assert_eq!(
                settings(&[], None).default_referral_code(false).unwrap(),
                None
            );
            assert_eq!(
                settings(&[], None)
                    .default_referral_code(true)
                    .unwrap()
                    .as_deref(),
                Some("TESTNET")
            );
        }
    }

    #[test]
    fn help_section_lists_every_documented_variable_aligned() {
        let section = env_help_section();
        assert!(section.starts_with("Environment:\n"));
        for (name, description) in ENV_HELP {
            assert_eq!(section.contains(name), !description.is_empty(), "{name}");
        }
        let column = section
            .lines()
            .nth(1)
            .unwrap()
            .find("Default output")
            .unwrap();
        assert!(section.lines().skip(1).all(|line| {
            line.get(..column)
                .is_some_and(|prefix| prefix.ends_with(' '))
        }));
    }

    #[test]
    fn every_variable_is_listed_once_and_the_vault_default_is_spelled_as_it_is_resolved() {
        let mut names = env::ALL.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), env::ALL.len(), "duplicate variable name");
        assert_eq!(env::ALL.len(), ENV_HELP.len());

        let vault_help = ENV_HELP
            .iter()
            .find(|(name, _)| *name == env::OWS_VAULT_PATH)
            .unwrap()
            .1;
        assert!(vault_help.contains(&format!("~/{}", crate::paths::DEFAULT_VAULT_DIR)));
    }

    #[test]
    fn api_override_accepts_ipv6_loopback_like_the_bankr_endpoint_does() {
        let url = settings(&[(env::API_BASE_URL, "http://[::1]:4011")], None)
            .api_base_url_override(Network::Mainnet)
            .unwrap()
            .unwrap();
        assert_eq!(url.as_str(), "http://[::1]:4011/");
        assert!(is_loopback_url(&reqwest::Url::parse("http://LOCALHOST:1").unwrap()));
        assert!(!is_loopback_url(&reqwest::Url::parse("http://[2001:db8::1]:1").unwrap()));
        assert!(!is_loopback_url(&reqwest::Url::parse("http://127.example.com").unwrap()));
    }
}
