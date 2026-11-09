//! Environment facts that shape output: which standard streams are terminals
//! and whether an agent or CI drives the run.
//!
//! Human formats shape themselves by stream: bordered tables and indented JSON
//! on a terminal, tab-separated tables and compact JSON on a pipe. Colors and
//! prompts read the same probes, so every module agrees on what a terminal is.

use std::io::IsTerminal;

use crate::settings::{EnvSource, env as settings_env};

#[derive(Clone, Copy)]
enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

#[cfg(not(test))]
fn probe(stream: Stream) -> bool {
    match stream {
        Stream::Stdin => std::io::stdin().is_terminal(),
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
    }
}

#[cfg(test)]
thread_local! {
    static TEST_TTY: std::cell::Cell<Option<(bool, bool)>> = const { std::cell::Cell::new(None) };
}

/// Unit tests never see the developer's stdout or stderr as terminals unless a
/// test opts in with [`with_tty_for_test`].
#[cfg(test)]
fn probe(stream: Stream) -> bool {
    let tty = TEST_TTY.with(std::cell::Cell::get);
    match stream {
        Stream::Stdin => std::io::stdin().is_terminal(),
        Stream::Stdout => tty.is_some_and(|tty| tty.0),
        Stream::Stderr => tty.is_some_and(|tty| tty.1),
    }
}

#[must_use]
pub fn stdin_is_terminal() -> bool {
    probe(Stream::Stdin)
}

#[must_use]
pub fn stdout_is_terminal() -> bool {
    probe(Stream::Stdout)
}

#[must_use]
pub fn stderr_is_terminal() -> bool {
    probe(Stream::Stderr)
}

#[cfg(test)]
pub fn with_tty_for_test<T>(stdout: bool, stderr: bool, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<(bool, bool)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_TTY.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(TEST_TTY.with(|cell| cell.replace(Some((stdout, stderr)))));
    f()
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

/// Terminal and environment facts about this run, probed once at startup and
/// carried by the output session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputEnv {
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    pub agent: bool,
    pub ci: bool,
    /// `--format` was given on the command line, so no default may override it.
    pub explicit_format: bool,
}

impl OutputEnv {
    #[must_use]
    pub fn probe(explicit_format: bool, env: &EnvSource) -> Self {
        Self {
            stdout_tty: stdout_is_terminal(),
            stderr_tty: stderr_is_terminal(),
            // An invalid `HYPERLIQUID_AGENT` counts as off here;
            // [`crate::mode::validate_env`] rejects it.
            agent: env_flag(env, settings_env::AGENT) == EnvFlag::On,
            ci: env_flag(env, settings_env::CI).on_unless_off(),
            explicit_format,
        }
    }
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
