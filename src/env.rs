//! The environment: every variable the CLI reads, what kind of value each is,
//! and where the values come from.
//!
//! Every variable is a constant here, with a [`Kind`] that says how its reader
//! interprets it. The root help text, the test harness's list of variables to
//! clear ([`ALL`]) and the strict-flag check (`mode::validate_env`) are
//! generated from the same table. [`EnvSource`] is the only way the CLI reads a
//! variable, so settings built over a fixed map never see the process's.
//!
//! This module imports nothing from the crate: errors, paths, output and every
//! other layer can name a variable without depending on settings.

use std::collections::HashMap;
use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A switch this CLI owns: `1|true|yes|on` or `0|false|no|off`, and
    /// anything else is a configuration error, so a typo never reads as off.
    StrictFlag,
    /// A switch another tool defines (`CI`, `FORCE_COLOR`): any value but an
    /// explicit off one means on.
    LenientFlag,
    /// Only whether the variable is set matters, whatever its value.
    Presence,
    /// Text where an empty value means unset.
    NonEmpty,
    /// A non-negative integer.
    Numeric,
    /// Text its reader parses or passes on as given.
    Value,
}

#[derive(Debug, Clone, Copy)]
pub struct Var {
    pub name: &'static str,
    pub kind: Kind,
    /// What the root help says; empty for a variable the CLI reads but does
    /// not advertise.
    pub help: &'static str,
}

/// One row per environment variable, in the order the root help lists them.
macro_rules! env_vars {
    ($($(#[$meta:meta])* $name:ident = $value:literal, $kind:ident => $help:literal $(, build $build:ident)?;)*) => {
        $($(#[$meta])* pub const $name: &str = $value;)*

        pub const TABLE: &[Var] = &[$(Var { name: $value, kind: Kind::$kind, help: $help }),*];

        /// The names of every variable, for a harness that must clear them all.
        pub const ALL: &[&str] = &[$($value),*];

        // `option_env!` needs the literal name, so the table is the one place
        // that spells a variable both as a runtime and as a build-time name.
        $($(
            pub(crate) const $build: &str = match option_env!($value) {
                Some(value) => value,
                None => "",
            };
        )?)*
    };
}

env_vars! {
    FORMAT = "HYPERLIQUID_FORMAT", Value => "Default output format: pretty, table, or json. --format overrides it.";
    AGENT = "HYPERLIQUID_AGENT", StrictFlag => "Set to 1 for JSON output and no prompts.";
    NETWORK = "HYPERLIQUID_NETWORK", Value => "Network to use: mainnet or testnet. --testnet overrides it.";
    HOME_DIR = "HYPERLIQUID_HOME_DIR", NonEmpty => "Home used for Bankr credentials and the default OWS vault, instead of platform discovery.";
    CONFIG_DIR = "HYPERLIQUID_CONFIG_DIR", NonEmpty => "Config directory, including the hyperliquid component, instead of platform discovery.";
    DATA_DIR = "HYPERLIQUID_DATA_DIR", NonEmpty => "Data directory, including the hyperliquid component, instead of platform discovery.";
    BANKR_API_KEY = "BANKR_API_KEY", NonEmpty => "Bankr API key for --bankr, shared with the Bankr CLI. `wallet login` stores one instead.";
    BANKR_API_URL = "BANKR_API_URL", NonEmpty => "Bankr API host, for a local or alternate endpoint.";
    PRIVATE_KEY = "HYPERLIQUID_PRIVATE_KEY", NonEmpty => "Private key for signing. Prefer it over --private-key.";
    KEYSTORE_PASSWORD = "HYPERLIQUID_KEYSTORE_PASSWORD", NonEmpty => "Password for --keystore. Prefer it over --keystore-password.";
    API_BASE_URL = "HYPERLIQUID_API_BASE_URL", NonEmpty => "Override the API endpoint. Loopback hosts only, for testing.";
    MAINNET_API_BASE_URL = "HYPERLIQUID_MAINNET_API_BASE_URL", NonEmpty => "Same override, mainnet only.";
    TESTNET_API_BASE_URL = "HYPERLIQUID_TESTNET_API_BASE_URL", NonEmpty => "Same override, testnet only.";
    OWS_VAULT_PATH = "HYPERLIQUID_OWS_VAULT_PATH", NonEmpty => "OWS vault directory. Defaults to ~/.hyperliquid.";
    OWS_PASSPHRASE = "OWS_PASSPHRASE", NonEmpty => "Passphrase for the OWS vault.";
    ACCOUNT_KEY_PASSPHRASE = "HYPERLIQUID_ACCOUNT_KEY_PASSPHRASE", Value => "Passphrase that unlocks pre-vault stored accounts, instead of the OS keychain.";
    ACCOUNT_KEYCHAIN_DISABLED = "HYPERLIQUID_ACCOUNT_KEYCHAIN_DISABLED", Presence => "Set to skip the OS keychain when unlocking pre-vault stored accounts.";
    /// Removed: it persisted raw account encryption keys. Setting it is an error.
    ACCOUNT_KEY_STORE_DIR = "HYPERLIQUID_ACCOUNT_KEY_STORE_DIR", Presence => "";
    DEFAULT_BUILDER_ADDRESS = "HYPERLIQUID_DEFAULT_BUILDER_ADDRESS", NonEmpty => "Default builder address for orders and setup suggestions.", build BUILD_DEFAULT_BUILDER_ADDRESS;
    DEFAULT_BUILDER_FEE_RATE = "HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE", NonEmpty => "Default builder fee rate, a percent string such as 0.001%.", build BUILD_DEFAULT_BUILDER_FEE_RATE;
    DEFAULT_REFERRAL_CODE = "HYPERLIQUID_DEFAULT_REFERRAL_CODE", NonEmpty => "Default referral code for `referral set` and setup.", build BUILD_DEFAULT_REFERRAL_CODE;
    NO_UPDATE_CHECK = "HYPERLIQUID_NO_UPDATE_CHECK", StrictFlag => "Set to 1 to skip release update checks.";
    NO_UPDATE_NOTIFIER = "NO_UPDATE_NOTIFIER", NonEmpty => "Set to a non-empty value to skip the update notice.";
    AUTO_UPDATE = "HYPERLIQUID_AUTO_UPDATE", StrictFlag => "Set to 1 to automatically update binary in place when a newer release is detected.";
    MCP_ALLOW_UNSAFE = "HYPERLIQUID_MCP_ALLOW_UNSAFE", StrictFlag => "Set to 1 to let `mcp` expose commands beyond the read-only set.";
    MCP_TIMEOUT_MS = "HYPERLIQUID_MCP_TIMEOUT_MS", Numeric => "Execution deadline for `mcp` calls, in milliseconds (default 30000).";
    WATCH_MAX_TICKS = "HYPERLIQUID_WATCH_MAX_TICKS", Numeric => "Tick bound for --watch when --max-ticks is not given.";
    SUBSCRIBE_MAX_EVENTS = "HYPERLIQUID_SUBSCRIBE_MAX_EVENTS", Numeric => "Event bound for subscribe when no bound option is given.";
    NO_COLOR = "NO_COLOR", NonEmpty => "Disable color when set to a non-empty value.";
    FORCE_COLOR = "FORCE_COLOR", LenientFlag => "Force color on, or off when set to 0 or false.";
    CI = "CI", LenientFlag => "Treat the run as non-interactive: no prompts.";
    TERM = "TERM", Value => "";
    CARGO_HOME = "CARGO_HOME", Value => "";
    // The HTTP stack honors these and names the one that applied to a failed
    // request; they are in the table so a harness clears them with the rest.
    HTTPS_PROXY = "HTTPS_PROXY", NonEmpty => "";
    HTTPS_PROXY_LOWER = "https_proxy", NonEmpty => "";
    HTTP_PROXY = "HTTP_PROXY", NonEmpty => "";
    HTTP_PROXY_LOWER = "http_proxy", NonEmpty => "";
    ALL_PROXY = "ALL_PROXY", NonEmpty => "";
    ALL_PROXY_LOWER = "all_proxy", NonEmpty => "";
    NO_PROXY = "NO_PROXY", NonEmpty => "";
    NO_PROXY_LOWER = "no_proxy", NonEmpty => "";
    RUST_BACKTRACE = "RUST_BACKTRACE", Value => "";
}

// Each `build` row above bakes the variable's value into the binary when it is
// set at build time (`HYPERLIQUID_DEFAULT_BUILDER_ADDRESS=... cargo build`); a
// runtime variable with the same name still overrides it. A variable unset at
// build time bakes in no default.

#[must_use]
pub fn kind(name: &str) -> Option<Kind> {
    TABLE
        .iter()
        .find(|var| var.name == name)
        .map(|var| var.kind)
}

pub fn strict_flags() -> impl Iterator<Item = &'static str> {
    TABLE
        .iter()
        .filter(|var| var.kind == Kind::StrictFlag)
        .map(|var| var.name)
}

#[must_use]
pub fn help_section() -> String {
    let advertised = || TABLE.iter().filter(|var| !var.help.is_empty());
    let width = advertised()
        .map(|var| var.name.len())
        .max()
        .unwrap_or_default()
        + 2;
    let mut section = String::from("Environment:\n");
    for var in advertised() {
        section.push_str(&format!("  {:<width$}{}\n", var.name, var.help));
    }
    section
}

/// The value trimmed, or `None` when it is absent or blank: what every setting
/// layer means by "not set".
pub(crate) fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
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

    #[must_use]
    pub fn get_non_empty(&self, name: &str) -> Option<String> {
        self.get(name).filter(|value| !value.is_empty())
    }

    #[must_use]
    pub fn get_trimmed(&self, name: &str) -> Option<String> {
        nonblank(self.get(name).as_deref()).map(str::to_string)
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

/// A boolean environment variable, parsed by [`parse_flag`]: the one place
/// that decides what `1|true|yes|on|0|false|no|off` (case-insensitive) mean.
///
/// A [`Kind::StrictFlag`] reads it strictly: only [`EnvFlag::On`] enables and
/// [`EnvFlag::Invalid`] is an error for the caller to report. A
/// [`Kind::LenientFlag`] reads it with [`EnvFlag::on_unless_off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvFlag {
    /// Unset or empty.
    Unset,
    On,
    Off,
    /// Set to something outside the accepted values.
    Invalid,
}

impl EnvFlag {
    /// The lenient reading for variables other tools define: set to anything
    /// but an explicit off value means on, so `CI=1`, `CI=true` and
    /// `FORCE_COLOR=2` all enable.
    #[must_use]
    pub fn on_unless_off(self) -> bool {
        matches!(self, Self::On | Self::Invalid)
    }
}

#[must_use]
pub fn parse_flag(raw: &str) -> EnvFlag {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => EnvFlag::Unset,
        "1" | "true" | "yes" | "on" => EnvFlag::On,
        "0" | "false" | "no" | "off" => EnvFlag::Off,
        _ => EnvFlag::Invalid,
    }
}

#[must_use]
pub fn flag(env: &EnvSource, name: &str) -> EnvFlag {
    env.get(name).map_or(EnvFlag::Unset, |raw| parse_flag(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_accept_known_values_and_treat_empty_as_unset() {
        assert_eq!(parse_flag("TRUE"), EnvFlag::On);
        assert_eq!(parse_flag(" off "), EnvFlag::Off);
        assert_eq!(parse_flag(""), EnvFlag::Unset);
        assert_eq!(parse_flag("banana"), EnvFlag::Invalid);
    }

    #[test]
    fn lenient_reading_turns_on_for_anything_but_an_explicit_off() {
        for on in ["1", "true", "yes", "on", "2", "banana"] {
            assert!(parse_flag(on).on_unless_off(), "{on}");
        }
        for off in ["", "0", "false", "no", "off", " OFF "] {
            assert!(!parse_flag(off).on_unless_off(), "{off:?}");
        }
    }

    #[test]
    fn every_variable_is_listed_once() {
        let mut names: Vec<&str> = ALL.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len(), "duplicate variable name");

        assert_eq!(kind(AGENT), Some(Kind::StrictFlag));
        assert_eq!(kind("NOT_A_VARIABLE"), None);
    }

    #[test]
    fn the_strict_flags_are_the_switches_this_cli_owns() {
        assert_eq!(
            strict_flags().collect::<Vec<_>>(),
            [AGENT, NO_UPDATE_CHECK, AUTO_UPDATE, MCP_ALLOW_UNSAFE]
        );
    }

    #[test]
    fn the_help_lists_advertised_variables_only() {
        let help = help_section();
        assert!(help.starts_with("Environment:\n"));
        assert!(help.contains(AGENT));
        for hidden in [
            TERM,
            CARGO_HOME,
            HTTPS_PROXY,
            RUST_BACKTRACE,
            ACCOUNT_KEY_STORE_DIR,
        ] {
            assert!(!help.contains(hidden), "{hidden}");
        }
    }
}
