//! Every helper returns plain text unless styling is enabled for the stream it
//! writes to. Use the top-level helpers for stdout content and the [`stderr`]
//! set for notices, warnings, prompts, and errors.
//!
//! Precedence: `--no-color`, non-empty `NO_COLOR`, `FORCE_COLOR` (`0`/`false`
//! off), then per-stream TTY detection with `TERM=dumb` off.

use std::sync::OnceLock;

use super::terminal::{self, Terminal};
use crate::settings::{EnvSource, env as settings_env};

macro_rules! style_codes {
    ($($name:ident = $code:literal),+ $(,)?) => {
        $(pub const $name: &str = $code;)+

        /// Every style sequence the helpers here can emit. Table sanitising
        /// lets exactly these through, so a new style appears here by being
        /// declared.
        pub const ALL_STYLES: &[&str] = &[$($name),+];
    };
}

style_codes! {
    CYAN = "\x1b[36m",
    GREEN = "\x1b[32m",
    RED = "\x1b[31m",
    GRAY = "\x1b[90m",
    YELLOW = "\x1b[33m",
    BOLD = "\x1b[1m",
    RESET = "\x1b[0m",
}

/// Wrap `text` in `code` unconditionally. Styling decisions belong to the
/// helpers below; tests use this to assert styled output deterministically.
#[must_use]
pub fn paint(code: &str, text: &str) -> String {
    format!("{code}{text}{RESET}")
}

fn styled(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        paint(code, text)
    } else {
        text.to_string()
    }
}

#[must_use]
pub fn cyan(text: &str) -> String {
    styled(stdout_enabled(), CYAN, text)
}

#[must_use]
pub fn green(text: &str) -> String {
    styled(stdout_enabled(), GREEN, text)
}

#[must_use]
pub fn red(text: &str) -> String {
    styled(stdout_enabled(), RED, text)
}

#[must_use]
pub fn gray(text: &str) -> String {
    styled(stdout_enabled(), GRAY, text)
}

#[must_use]
pub fn yellow(text: &str) -> String {
    styled(stdout_enabled(), YELLOW, text)
}

pub mod stderr {
    use super::{GRAY, YELLOW, stderr_enabled, styled};

    #[must_use]
    pub fn gray(text: &str) -> String {
        styled(stderr_enabled(), GRAY, text)
    }

    #[must_use]
    pub fn yellow(text: &str) -> String {
        styled(stderr_enabled(), YELLOW, text)
    }
}

/// Per-stream color decision: `(stdout, stderr)`.
///
/// Process-wide because table cells are styled where they are built, far from
/// any session. [`init`] sets it once at startup; until then nothing is styled.
static STREAMS: OnceLock<(bool, bool)> = OnceLock::new();

/// Resolve color once per process from the run's terminal and environment.
/// Later calls are ignored.
pub fn init(no_color_flag: bool, env: &EnvSource, terminal: Terminal) {
    let _ = STREAMS.set(resolve(no_color_flag, env, terminal));
}

fn resolve(no_color_flag: bool, env: &EnvSource, terminal: Terminal) -> (bool, bool) {
    let no_color_env = env
        .get_os(settings_env::NO_COLOR)
        .is_some_and(|value| !value.is_empty());
    let dumb = env
        .get(settings_env::TERM)
        .is_some_and(|term| term == "dumb");
    decide(
        no_color_flag || no_color_env,
        terminal::env_flag(env, settings_env::FORCE_COLOR),
        dumb,
        (terminal.stdout, terminal.stderr),
    )
}

/// The `(stdout, stderr)` color decision. `disabled` (`--no-color` or
/// `NO_COLOR`) beats `force`; a set `FORCE_COLOR` beats terminal detection and
/// follows the convention for ecosystem variables: any value but an explicit
/// off one (`0`, `false`, `no`, `off`) forces color, so `FORCE_COLOR=2` works.
fn decide(
    disabled: bool,
    force: terminal::EnvFlag,
    dumb_terminal: bool,
    is_terminal: (bool, bool),
) -> (bool, bool) {
    if disabled {
        return (false, false);
    }
    if force != terminal::EnvFlag::Unset {
        let on = force.on_unless_off();
        return (on, on);
    }
    (
        !dumb_terminal && is_terminal.0,
        !dumb_terminal && is_terminal.1,
    )
}

#[cfg(test)]
thread_local! {
    static TEST_STREAMS: std::cell::Cell<Option<(bool, bool)>> = const { std::cell::Cell::new(None) };
}

/// Unit tests never inherit the developer's terminal or environment: until a
/// test opts in with [`with_streams_for_test`], nothing is styled.
fn streams() -> (bool, bool) {
    #[cfg(test)]
    if let Some(streams) = TEST_STREAMS.with(std::cell::Cell::get) {
        return streams;
    }
    STREAMS.get().copied().unwrap_or((false, false))
}

#[cfg(test)]
pub fn with_streams_for_test<T>(stdout: bool, stderr: bool, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<(bool, bool)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_STREAMS.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(TEST_STREAMS.with(|cell| cell.replace(Some((stdout, stderr)))));
    f()
}

#[must_use]
pub fn stdout_enabled() -> bool {
    streams().0
}

#[must_use]
pub fn stderr_enabled() -> bool {
    streams().1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_helper_follows_its_own_stream() {
        with_streams_for_test(true, false, || {
            assert_eq!(cyan("x"), paint(CYAN, "x"));
            assert_eq!(stderr::gray("x"), "x");
        });
        with_streams_for_test(false, true, || {
            assert_eq!(cyan("x"), "x");
            assert_eq!(stderr::gray("x"), paint(GRAY, "x"));
        });
    }

    #[test]
    fn resolution_reads_the_given_terminal_and_environment() {
        let tty = Terminal {
            stdin: true,
            stdout: true,
            stderr: false,
        };
        let none = EnvSource::fixed::<&str, &str>([]);
        let with = |name: &'static str| EnvSource::fixed([(name, "1")]);
        assert_eq!(resolve(false, &none, tty), (true, false));
        assert_eq!(resolve(true, &none, tty), (false, false));
        assert_eq!(resolve(false, &with("NO_COLOR"), tty), (false, false));
        assert_eq!(
            resolve(false, &with("FORCE_COLOR"), Terminal::default()),
            (true, true)
        );
        assert_eq!(
            resolve(false, &EnvSource::fixed([("TERM", "dumb")]), tty),
            (false, false)
        );
    }

    #[test]
    fn force_color_follows_the_shared_flag_parser() {
        let ttys = (false, false);
        for on in ["1", "true", "YES", "on", "2", "3"] {
            let force = terminal::parse_flag(on);
            assert_eq!(decide(false, force, false, ttys), (true, true), "{on}");
        }
        for off in ["0", "false", "no", "OFF"] {
            let force = terminal::parse_flag(off);
            assert_eq!(
                decide(false, force, false, (true, true)),
                (false, false),
                "{off}"
            );
        }
    }

    #[test]
    fn disabling_beats_forcing_and_terminals_decide_the_rest() {
        let force = terminal::parse_flag("1");
        assert_eq!(decide(true, force, false, (true, true)), (false, false));

        let unset = terminal::parse_flag("");
        assert_eq!(decide(false, unset, false, (true, false)), (true, false));
        assert_eq!(decide(false, unset, true, (true, true)), (false, false));
    }
}
