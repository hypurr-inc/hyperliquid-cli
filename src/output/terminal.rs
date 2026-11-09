//! Facts about the terminal and boolean environment variables.
//!
//! [`Terminal`] is probed once at startup and owned by [`crate::mode::Mode`],
//! so every module agrees on what a terminal is: bordered tables and indented
//! JSON on a terminal, tab-separated tables and compact JSON on a pipe.

use std::io::IsTerminal;

use crate::settings::EnvSource;

/// Which standard streams are terminals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Terminal {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

impl Terminal {
    /// The process's real standard streams. Only the entry point calls this.
    #[must_use]
    pub fn probe() -> Self {
        Self {
            stdin: std::io::stdin().is_terminal(),
            stdout: std::io::stdout().is_terminal(),
            stderr: std::io::stderr().is_terminal(),
        }
    }
}

/// A boolean environment variable, parsed by [`parse_flag`]: the one place
/// that decides what `1|true|yes|on|0|false|no|off` (case-insensitive) mean.
///
/// Variables this CLI owns read it strictly: only [`EnvFlag::On`] enables and
/// [`EnvFlag::Invalid`] is an error for the caller to report. Variables other
/// tools define (`CI`, `FORCE_COLOR`) read it with [`EnvFlag::on_unless_off`].
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
pub fn env_flag(env: &EnvSource, name: &str) -> EnvFlag {
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
}
