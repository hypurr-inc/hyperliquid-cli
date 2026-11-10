//! When a stream stops: the bound a command declares, the flags and
//! environment variables that set it, and the check that a machine-context run
//! has one.
//!
//! A command declares its bound once with [`StreamBound`] on its spec; the
//! schema, the pipeline's check ([`require_bound`]) and the environment
//! fallback all read that one value. This module is below both `command_spec`
//! (which stores the declaration) and `stream` (which runs to the bound).

use std::time::Duration;

use clap::{ArgMatches, Args};

use crate::env::{self, EnvSource};
use crate::errors::CliError;
use crate::quantity::parse_positive_usize;

/// How a command bounds its output in machine contexts. One definition: the
/// schema, the pipeline's check and the environment fallback all read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamBound {
    /// `--watch` needs `--max-ticks` (or `HYPERLIQUID_WATCH_MAX_TICKS`).
    SnapshotWatch,
    /// Needs `--max-events`, `--idle-timeout-ms` or `HYPERLIQUID_SUBSCRIBE_MAX_EVENTS`.
    Subscribe,
    /// A polling alert stream. Needs `--max-events` or `--idle-timeout-ms`; no environment fallback.
    Polled,
}

/// What `0` means in a bound's environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvZero {
    /// Not a bound: the variable is ignored, as if unset.
    Unbounded,
    /// A real bound of zero items: the stream stops right after it starts.
    StopImmediately,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamEnv {
    pub name: &'static str,
    pub zero: EnvZero,
}

impl StreamBound {
    #[must_use]
    pub const fn cli_args(self) -> &'static [&'static str] {
        match self {
            Self::SnapshotWatch => &["max_ticks"],
            Self::Subscribe | Self::Polled => &["max_events", "idle_timeout_ms"],
        }
    }

    /// The boolean arg that makes the bound apply; the bound always applies when `None`.
    #[must_use]
    pub const fn required_when(self) -> Option<&'static str> {
        match self {
            Self::SnapshotWatch => Some("watch"),
            Self::Subscribe | Self::Polled => None,
        }
    }

    #[must_use]
    pub(crate) const fn env(self) -> Option<StreamEnv> {
        match self {
            Self::SnapshotWatch => Some(StreamEnv {
                name: env::WATCH_MAX_TICKS,
                zero: EnvZero::Unbounded,
            }),
            Self::Subscribe => Some(StreamEnv {
                name: env::SUBSCRIBE_MAX_EVENTS,
                zero: EnvZero::StopImmediately,
            }),
            Self::Polled => None,
        }
    }

    /// What a machine-context run is told when it declares no stop condition:
    /// the flags and the environment variable that would bound it. `command`
    /// names the run for a stream that is a command of its own (`Polled`),
    /// whose message also carries a runnable example.
    #[must_use]
    fn unbounded_message(self, command: &str) -> String {
        let mut ways: Vec<String> = self
            .cli_args()
            .iter()
            .map(|id| format!("--{} <N>", id.replace('_', "-")))
            .collect();
        ways.extend(self.env().map(|env| env.name.to_string()));
        match self {
            Self::SnapshotWatch => format!(
                "JSON watch output must be bounded with {}",
                join_alternatives(ways)
            ),
            Self::Subscribe => format!(
                "JSON subscribe output must be bounded with {}",
                join_alternatives(ways)
            ),
            Self::Polled => {
                let flags = self
                    .cli_args()
                    .iter()
                    .map(|id| format!("--{}", id.replace('_', "-")))
                    .collect::<Vec<_>>()
                    .join(" or ");
                format!(
                    "{command} requires {flags} in agent mode.\n  \
                     hyperliquid --format json {command} {POLLED_EXAMPLE_ARGS} --max-events 20"
                )
            }
        }
    }
}

/// The address a polled stream's example names: every polled stream watches an
/// address.
const POLLED_EXAMPLE_ARGS: &str = "--user 0x0000000000000000000000000000000000000001";

fn join_alternatives(mut ways: Vec<String>) -> String {
    let last = ways.pop().unwrap_or_default();
    match ways.len() {
        0 => last,
        1 => format!("{} or {last}", ways[0]),
        _ => format!("{}, or {last}", ways.join(", ")),
    }
}

/// A polling stream with no explicit idle bound ends after this long without output.
const POLL_DEFAULT_IDLE: Duration = crate::clock::HOUR.of(1);

/// `--watch` / `--max-ticks`, shared by every snapshot command.
#[derive(Args, Debug, Clone, Default)]
pub struct SnapshotWatchArgs {
    /// Watch live updates in-place
    #[arg(short = 'w', long)]
    pub watch: bool,
    /// Stop after rendering this many watch snapshots (useful for automation)
    #[arg(long, value_name = "TICKS", requires = "watch", value_parser = parse_positive_usize)]
    pub max_ticks: Option<usize>,
}

/// `--max-events` / `--idle-timeout-ms`, shared by `subscribe` and `watch risk`.
#[derive(Args, Debug, Clone, Default)]
pub struct StreamBoundArgs {
    /// Stop after emitting this many matching events (useful for automation)
    #[arg(long)]
    pub max_events: Option<usize>,
    /// Stop if no matching events are emitted within this many milliseconds
    #[arg(long, value_name = "MILLISECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    pub idle_timeout_ms: Option<u64>,
}

impl StreamBoundArgs {
    fn idle(&self) -> Option<Duration> {
        self.idle_timeout_ms.map(Duration::from_millis)
    }
}

/// The environment variable behind a bound, read as a count. A value that does
/// not parse is ignored here and rejected by [`validate_env_bound`].
fn read_env_bound(env: StreamEnv, source: &EnvSource) -> Option<usize> {
    source
        .get(env.name)
        .and_then(|raw| parse_env_count(&raw, env.zero))
}

/// `0` is a bound only when the variable's [`EnvZero`] says so.
fn parse_env_count(raw: &str, zero: EnvZero) -> Option<usize> {
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|value| zero == EnvZero::StopImmediately || *value > 0)
}

fn validate_env_bound(env: StreamEnv, source: &EnvSource) -> Result<(), CliError> {
    let Some(raw) = source.get(env.name) else {
        return Ok(());
    };
    raw.trim().parse::<usize>().map(|_| ()).map_err(|err| {
        CliError::Configuration(format!(
            "{} must be a non-negative integer: {err}",
            env.name
        ))
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamBounds {
    /// Ticks, events or alerts, depending on the stream.
    pub max_items: Option<usize>,
    /// Stop after this long without output.
    pub idle: Option<Duration>,
}

impl StreamBounds {
    /// `--max-ticks`, falling back to `HYPERLIQUID_WATCH_MAX_TICKS`.
    pub fn snapshot_watch(max_ticks: Option<usize>, source: &EnvSource) -> Self {
        Self {
            max_items: max_ticks.or_else(|| {
                StreamBound::SnapshotWatch
                    .env()
                    .and_then(|env| read_env_bound(env, source))
            }),
            idle: None,
        }
    }

    /// `--max-events`, falling back to `HYPERLIQUID_SUBSCRIBE_MAX_EVENTS`, plus
    /// `--idle-timeout-ms`. A bound of `0` is valid: the stream ends after its
    /// `subscribed` line.
    pub fn subscribe(args: &StreamBoundArgs, source: &EnvSource) -> Self {
        Self {
            max_items: args.max_events.or_else(|| {
                StreamBound::Subscribe
                    .env()
                    .and_then(|env| read_env_bound(env, source))
            }),
            idle: args.idle(),
        }
    }

    /// `watch risk` takes its bounds from flags only, and a polling stream
    /// with no explicit idle bound ends after [`POLL_DEFAULT_IDLE`] without output.
    pub fn watch_risk(args: &StreamBoundArgs) -> Self {
        Self {
            max_items: args.max_events,
            idle: args.idle().or(Some(POLL_DEFAULT_IDLE)),
        }
    }

    pub(crate) fn reached(&self, emitted: usize) -> bool {
        self.max_items.is_some_and(|max| emitted >= max)
    }
}

/// Machine contexts must not start an endless stream: reject a run of `bound`'s
/// command, given the command's own matches, that declares no stop condition.
/// The environment variable is consulted, and checked for validity, only when
/// no flag bounds the run.
pub fn require_bound(
    command: &str,
    bound: StreamBound,
    matches: &ArgMatches,
    source: &EnvSource,
) -> Result<(), CliError> {
    let internal = |err: clap::parser::MatchesError| CliError::Internal(anyhow::anyhow!(err));
    if let Some(guard) = bound.required_when()
        && !matches
            .try_get_one::<bool>(guard)
            .map_err(internal)?
            .copied()
            .unwrap_or(false)
    {
        return Ok(());
    }
    for id in bound.cli_args() {
        if matches.try_contains_id(id).map_err(internal)? {
            return Ok(());
        }
    }
    if let Some(env) = bound.env() {
        validate_env_bound(env, source)?;
        if read_env_bound(env, source).is_some() {
            return Ok(());
        }
    }
    Err(CliError::Unsupported(bound.unbounded_message(command)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_risk_bounds_default_the_idle_end_and_keep_explicit_ones() {
        let defaulted = StreamBounds::watch_risk(&StreamBoundArgs::default());
        assert_eq!(defaulted.max_items, None);
        assert_eq!(defaulted.idle, Some(POLL_DEFAULT_IDLE));
        assert_eq!(
            StreamBounds::watch_risk(&StreamBoundArgs {
                max_events: Some(3),
                idle_timeout_ms: Some(250),
            }),
            StreamBounds {
                max_items: Some(3),
                idle: Some(Duration::from_millis(250)),
            }
        );
    }

    #[test]
    fn zero_in_the_environment_follows_the_bound() {
        let ticks = StreamBound::SnapshotWatch.env().unwrap().zero;
        let events = StreamBound::Subscribe.env().unwrap().zero;
        assert_eq!(parse_env_count("0", ticks), None, "0 ticks is not a bound");
        assert_eq!(
            parse_env_count("0", events),
            Some(0),
            "0 events stops at once"
        );
        assert_eq!(parse_env_count(" 3 ", ticks), Some(3));
        assert_eq!(parse_env_count("3", events), Some(3));
        assert_eq!(parse_env_count("many", events), None);
    }

    #[test]
    fn every_bound_names_args_its_command_flattens() {
        use clap::{Args, Command};
        let watch = SnapshotWatchArgs::augment_args(Command::new("c"));
        let events = StreamBoundArgs::augment_args(Command::new("c"));
        for (bound, command) in [
            (StreamBound::SnapshotWatch, &watch),
            (StreamBound::Subscribe, &events),
            (StreamBound::Polled, &events),
        ] {
            let ids = |id: &str| command.get_arguments().any(|arg| arg.get_id() == id);
            for id in bound
                .cli_args()
                .iter()
                .copied()
                .chain(bound.required_when())
            {
                assert!(
                    ids(id),
                    "{bound:?} names `{id}`, which its args do not define"
                );
            }
        }
    }

    #[test]
    fn the_environment_help_documents_every_stream_bound_variable() {
        let help = crate::env::help_section();
        for bound in [
            StreamBound::SnapshotWatch,
            StreamBound::Subscribe,
            StreamBound::Polled,
        ] {
            if let Some(env) = bound.env() {
                assert!(
                    help.contains(env.name),
                    "{} is missing from the environment help",
                    env.name
                );
            }
        }
    }

    fn matches(args: &[&str]) -> ArgMatches {
        use clap::{Args, Command};
        let command =
            StreamBoundArgs::augment_args(SnapshotWatchArgs::augment_args(Command::new("c")));
        command.try_get_matches_from(args).unwrap()
    }

    fn env(pairs: &[(&'static str, &'static str)]) -> EnvSource {
        EnvSource::fixed(pairs.iter().copied())
    }

    #[test]
    fn polled_streams_must_declare_a_bound_and_name_their_own_command() {
        let none = env(&[]);
        let err =
            require_bound("watch risk", StreamBound::Polled, &matches(&["c"]), &none).unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert_eq!(
            err.to_string(),
            "Unsupported input: watch risk requires --max-events or --idle-timeout-ms in agent mode.\n  hyperliquid --format json watch risk --user 0x0000000000000000000000000000000000000001 --max-events 20"
        );
        let other =
            require_bound("alerts feed", StreamBound::Polled, &matches(&["c"]), &none).unwrap_err();
        assert!(
            other
                .to_string()
                .starts_with("Unsupported input: alerts feed requires")
                && other
                    .to_string()
                    .contains("hyperliquid --format json alerts feed --user"),
            "{other}"
        );
        for bounded in [["c", "--max-events", "3"], ["c", "--idle-timeout-ms", "50"]] {
            assert!(require_bound("c", StreamBound::Polled, &matches(&bounded), &none).is_ok());
        }
    }

    #[test]
    fn snapshot_watch_is_bounded_only_when_watching() {
        let none = env(&[]);
        assert!(require_bound("c", StreamBound::SnapshotWatch, &matches(&["c"]), &none).is_ok());
        assert!(
            require_bound(
                "c",
                StreamBound::SnapshotWatch,
                &matches(&["c", "--watch", "--max-ticks", "2"]),
                &none
            )
            .is_ok()
        );
        let err = require_bound(
            "c",
            StreamBound::SnapshotWatch,
            &matches(&["c", "--watch"]),
            &none,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert_eq!(
            err.to_string(),
            "Unsupported input: JSON watch output must be bounded with --max-ticks <N> or HYPERLIQUID_WATCH_MAX_TICKS"
        );
    }

    #[test]
    fn subscribe_is_bounded_by_events_idle_or_the_environment() {
        let none = env(&[]);
        let err = require_bound("c", StreamBound::Subscribe, &matches(&["c"]), &none).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unsupported input: JSON subscribe output must be bounded with --max-events <N>, --idle-timeout-ms <N>, or HYPERLIQUID_SUBSCRIBE_MAX_EVENTS"
        );
        for bounded in [["c", "--max-events", "0"], ["c", "--idle-timeout-ms", "10"]] {
            assert!(require_bound("c", StreamBound::Subscribe, &matches(&bounded), &none).is_ok());
        }
        let zero = env(&[("HYPERLIQUID_SUBSCRIBE_MAX_EVENTS", "0")]);
        assert!(require_bound("c", StreamBound::Subscribe, &matches(&["c"]), &zero).is_ok());
    }

    #[test]
    fn the_environment_bound_is_read_from_the_given_source_and_checked_only_when_consulted() {
        let ticks = env(&[("HYPERLIQUID_WATCH_MAX_TICKS", "4")]);
        assert_eq!(
            StreamBounds::snapshot_watch(None, &ticks).max_items,
            Some(4)
        );
        assert_eq!(
            StreamBounds::snapshot_watch(Some(2), &ticks).max_items,
            Some(2),
            "a flag beats the variable"
        );
        assert_eq!(
            StreamBounds::snapshot_watch(None, &env(&[])).max_items,
            None
        );

        let garbage = env(&[("HYPERLIQUID_SUBSCRIBE_MAX_EVENTS", "many")]);
        let err =
            require_bound("c", StreamBound::Subscribe, &matches(&["c"]), &garbage).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .contains("HYPERLIQUID_SUBSCRIBE_MAX_EVENTS must be")
        );
        assert!(
            require_bound(
                "c",
                StreamBound::Subscribe,
                &matches(&["c", "--max-events", "3"]),
                &garbage
            )
            .is_ok(),
            "a flag bounds the run without consulting the variable"
        );
    }
}
