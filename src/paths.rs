//! Where the CLI keeps its files. Environment overrides live in [`crate::settings`].

use std::path::PathBuf;

const APP_DIR: &str = "hyperliquid";

/// Config directory, overridden by `HYPERLIQUID_CONFIG_DIR` when nonempty:
/// - macOS: `~/Library/Application Support/hyperliquid`
/// - Linux: `~/.config/hyperliquid`
/// - Windows: `C:\Users\<user>\AppData\Roaming\hyperliquid`
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    directory_override(crate::settings::env::CONFIG_DIR)
        .or_else(|| dirs::config_dir().map(|dir| dir.join(APP_DIR)))
}

#[must_use]
pub fn config_file() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.json"))
}

/// Cached release info for the update notice. Lives beside the config file.
#[must_use]
pub fn version_cache_file() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("version.json"))
}

/// File where the most recent error envelope is recorded for `report-incident`.
#[must_use]
pub fn last_error_file() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("last_error.json"))
}

/// A nonempty override bypasses platform directory discovery entirely.
fn directory_override(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Data directory, overridden by `HYPERLIQUID_DATA_DIR` when nonempty.
fn data_dir() -> Option<PathBuf> {
    directory_override(crate::settings::env::DATA_DIR)
        .or_else(|| dirs::data_dir().map(|dir| dir.join(APP_DIR)))
}

/// Encrypted accounts written by releases before the OWS vault.
#[must_use]
pub fn legacy_accounts_db() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("accounts.db"))
}

/// Plain key file that releases before the OS keychain stored next to the database.
#[must_use]
pub fn legacy_accounts_key() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("accounts.key"))
}

/// Default OWS vault directory when `HYPERLIQUID_OWS_VAULT_PATH` is unset.
#[must_use]
pub fn default_vault_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".hyperliquid"))
}

/// Home used for Bankr credentials and the default OWS vault.
/// `HYPERLIQUID_HOME_DIR` bypasses platform discovery when nonempty.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    directory_override(crate::settings::env::HOME_DIR).or_else(dirs::home_dir)
}
