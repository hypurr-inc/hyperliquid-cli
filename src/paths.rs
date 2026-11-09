//! Where the CLI keeps its files. Directory overrides come from the caller's
//! [`EnvSource`], so settings built over a fixed environment never read the
//! process's.

use std::path::PathBuf;

use crate::settings::{EnvSource, env};

const APP_DIR: &str = "hyperliquid";

pub const DEFAULT_VAULT_DIR: &str = ".hyperliquid";

/// Config directory, overridden by `HYPERLIQUID_CONFIG_DIR` when nonempty:
/// - macOS: `~/Library/Application Support/hyperliquid`
/// - Linux: `~/.config/hyperliquid`
/// - Windows: `C:\Users\<user>\AppData\Roaming\hyperliquid`
#[must_use]
pub fn config_dir(env: &EnvSource) -> Option<PathBuf> {
    directory_override(env, env::CONFIG_DIR)
        .or_else(|| dirs::config_dir().map(|dir| dir.join(APP_DIR)))
}

#[must_use]
pub fn config_file(env: &EnvSource) -> Option<PathBuf> {
    config_dir(env).map(|dir| dir.join("config.json"))
}

#[must_use]
pub fn version_cache_file(env: &EnvSource) -> Option<PathBuf> {
    config_dir(env).map(|dir| dir.join("version.json"))
}

/// File where the most recent error envelope is recorded for `report-incident`.
/// Written while an error is being reported, with no settings to consult, so it
/// follows the process environment.
#[must_use]
pub fn last_error_file() -> Option<PathBuf> {
    config_dir(&EnvSource::Process).map(|dir| dir.join("last_error.json"))
}

/// A nonempty override bypasses platform directory discovery entirely.
fn directory_override(env: &EnvSource, name: &str) -> Option<PathBuf> {
    env.get_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn data_dir(env: &EnvSource) -> Option<PathBuf> {
    directory_override(env, env::DATA_DIR).or_else(|| dirs::data_dir().map(|dir| dir.join(APP_DIR)))
}

/// Encrypted accounts written by releases before the OWS vault.
#[must_use]
pub fn legacy_accounts_db(env: &EnvSource) -> Option<PathBuf> {
    data_dir(env).map(|dir| dir.join("accounts.db"))
}

/// Plain key file that releases before the OS keychain stored next to the database.
#[must_use]
pub fn legacy_accounts_key(env: &EnvSource) -> Option<PathBuf> {
    data_dir(env).map(|dir| dir.join("accounts.key"))
}

/// Default OWS vault directory when `HYPERLIQUID_OWS_VAULT_PATH` is unset.
#[must_use]
pub fn default_vault_dir(env: &EnvSource) -> Option<PathBuf> {
    home_dir(env).map(|home| home.join(DEFAULT_VAULT_DIR))
}

/// The Bankr CLI's own credentials file.
#[must_use]
pub fn bankr_cli_config(env: &EnvSource) -> Option<PathBuf> {
    home_dir(env).map(|home| home.join(".bankr").join("config.json"))
}

/// Home used for Bankr credentials and the default OWS vault.
/// `HYPERLIQUID_HOME_DIR` bypasses platform discovery when nonempty.
#[must_use]
pub fn home_dir(env: &EnvSource) -> Option<PathBuf> {
    directory_override(env, env::HOME_DIR).or_else(dirs::home_dir)
}
