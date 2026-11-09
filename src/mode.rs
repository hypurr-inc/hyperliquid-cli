//! The interaction context of one run: who, if anyone, is at the other end.
//!
//! [`Mode`] is built once at startup from the flags, the output format and the
//! probed [`OutputEnv`]. It is the only reader of agent, CI and terminal state
//! for interaction decisions: each question the CLI asks about its surroundings
//! has exactly one predicate here, so every command and every pipeline step
//! agrees. Nothing else reads `HYPERLIQUID_AGENT`, `CI` or a TTY to decide
//! whether to prompt, bound a stream, announce or redraw.

use crate::errors::CliError;
use crate::output::terminal::{EnvFlag, OutputEnv, env_flag};
use crate::settings::{EnvSource, env};

/// What decides whether this invocation may prompt, resolved once in `main`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mode {
    /// `--non-interactive` was passed.
    pub non_interactive_flag: bool,
    /// The effective output format is JSON (explicit, env, agent, or non-TTY default).
    pub machine_format: bool,
    /// Stdin carries a payload (`--payload-file -`), so it can't answer prompts.
    pub stdin_is_payload: bool,
    /// `HYPERLIQUID_AGENT` is on.
    pub agent: bool,
    /// The process runs under CI.
    pub ci: bool,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
}

impl Mode {
    #[must_use]
    pub fn new(
        non_interactive_flag: bool,
        machine_format: bool,
        stdin_is_payload: bool,
        env: &OutputEnv,
    ) -> Self {
        Self {
            non_interactive_flag,
            machine_format,
            stdin_is_payload,
            agent: env.agent,
            ci: env.ci,
            stdout_tty: env.stdout_tty,
            stderr_tty: env.stderr_tty,
        }
    }

    /// Whether this invocation may ask the user a question.
    ///
    /// Prompts are off for `--non-interactive`, JSON output, a detected agent, CI,
    /// and when stdin is a payload. Piped stdin may still answer prompts in human
    /// mode; end-of-input is treated as missing input by [`crate::prompt`].
    #[must_use]
    pub fn can_prompt(&self) -> bool {
        self.prompt_blocker().is_none()
    }

    /// Why prompts are off, for error copy. `None` when prompts are allowed.
    #[must_use]
    pub fn prompt_blocker(&self) -> Option<&'static str> {
        if self.non_interactive_flag {
            Some("--non-interactive")
        } else if self.stdin_is_payload {
            Some("stdin is used as the payload")
        } else if self.machine_format {
            Some("JSON output")
        } else if self.agent {
            Some(env::AGENT)
        } else if self.ci {
            Some(env::CI)
        } else {
            None
        }
    }

    /// A program, not a person, consumes this run's output: JSON format or an
    /// agent. Endless streams need an explicit bound and secrets may not arrive
    /// on the command line. CI alone does not make a context machine-readable.
    #[must_use]
    pub fn machine_context(&self) -> bool {
        self.machine_format || self.agent
    }

    /// Notices addressed to a person on stderr, such as retry announcements:
    /// stderr is a terminal and the run could prompt that person.
    #[must_use]
    pub fn person_on_stderr(&self) -> bool {
        self.stderr_tty && self.can_prompt()
    }

    /// An unprompted notice may interrupt the person at this terminal, for
    /// example the passive update notice: human formats, no agent or CI, and
    /// both output streams are terminals.
    #[must_use]
    pub fn person_at_terminal(&self) -> bool {
        !self.machine_format && !self.agent && !self.ci && self.stdout_tty && self.stderr_tty
    }

    /// The run may redraw the terminal in place (a live view): stdout is a
    /// terminal and neither an agent, CI nor `--non-interactive` is driving.
    #[must_use]
    pub fn live_display(&self) -> bool {
        self.stdout_tty && !self.agent && !self.ci && !self.non_interactive_flag
    }
}

/// Parse a boolean environment variable strictly.
///
/// Unset or empty means `None`. Any value outside `1|true|yes|on|0|false|no|off`
/// (case-insensitive) is a configuration error naming the variable, so a typo
/// never silently falls back to the default.
fn env_bool(env: &EnvSource, name: &str) -> Result<Option<bool>, CliError> {
    flag_value(name, env_flag(env, name))
}

fn flag_value(name: &str, flag: EnvFlag) -> Result<Option<bool>, CliError> {
    match flag {
        EnvFlag::Unset => Ok(None),
        EnvFlag::On => Ok(Some(true)),
        EnvFlag::Off => Ok(Some(false)),
        EnvFlag::Invalid => Err(CliError::Configuration(format!(
            "{name} must be one of 1, true, yes, on, 0, false, no, off"
        ))),
    }
}

/// Validate every boolean `HYPERLIQUID_*` mode variable once at startup.
pub fn validate_env(source: &EnvSource) -> Result<(), CliError> {
    env_bool(source, env::AGENT)?;
    env_bool(source, env::NO_UPDATE_CHECK)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::terminal::parse_flag;

    #[test]
    fn strict_bool_accepts_known_values_and_treats_empty_as_unset() {
        assert_eq!(flag_value("X", parse_flag("TRUE")).unwrap(), Some(true));
        assert_eq!(flag_value("X", parse_flag(" off ")).unwrap(), Some(false));
        assert_eq!(flag_value("X", parse_flag("")).unwrap(), None);
    }

    #[test]
    fn strict_bool_rejects_unknown_values_by_name() {
        let err = flag_value("HYPERLIQUID_AGENT", parse_flag("banana")).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("HYPERLIQUID_AGENT"));
    }

    #[test]
    fn prompts_are_blocked_by_the_first_matching_reason() {
        assert!(Mode::default().can_prompt());
        assert_eq!(Mode::default().prompt_blocker(), None);

        let flagged = Mode {
            non_interactive_flag: true,
            machine_format: true,
            ..Mode::default()
        };
        assert_eq!(flagged.prompt_blocker(), Some("--non-interactive"));

        let payload = Mode {
            stdin_is_payload: true,
            machine_format: true,
            ..Mode::default()
        };
        assert_eq!(
            payload.prompt_blocker(),
            Some("stdin is used as the payload")
        );

        let json = Mode {
            machine_format: true,
            agent: true,
            ..Mode::default()
        };
        assert_eq!(json.prompt_blocker(), Some("JSON output"));

        let agent = Mode {
            agent: true,
            ci: true,
            ..Mode::default()
        };
        assert_eq!(agent.prompt_blocker(), Some("HYPERLIQUID_AGENT"));

        let ci = Mode {
            ci: true,
            ..Mode::default()
        };
        assert!(!ci.can_prompt());
        assert_eq!(ci.prompt_blocker(), Some(env::CI));
    }

    fn terminal() -> Mode {
        Mode {
            stdout_tty: true,
            stderr_tty: true,
            ..Mode::default()
        }
    }

    #[test]
    fn machine_context_is_json_or_agent_but_not_ci() {
        assert!(!Mode::default().machine_context());
        assert!(
            Mode {
                machine_format: true,
                ..Mode::default()
            }
            .machine_context()
        );
        assert!(
            Mode {
                agent: true,
                ..Mode::default()
            }
            .machine_context()
        );
        assert!(
            !Mode {
                ci: true,
                non_interactive_flag: true,
                ..Mode::default()
            }
            .machine_context()
        );
    }

    #[test]
    fn stderr_notices_are_for_a_person_at_a_terminal_who_may_be_prompted() {
        assert!(terminal().person_on_stderr());
        for (name, mode) in [
            (
                "stderr piped",
                Mode {
                    stderr_tty: false,
                    ..terminal()
                },
            ),
            (
                "json",
                Mode {
                    machine_format: true,
                    ..terminal()
                },
            ),
            (
                "agent",
                Mode {
                    agent: true,
                    ..terminal()
                },
            ),
            (
                "ci",
                Mode {
                    ci: true,
                    ..terminal()
                },
            ),
            (
                "non-interactive",
                Mode {
                    non_interactive_flag: true,
                    ..terminal()
                },
            ),
            (
                "payload on stdin",
                Mode {
                    stdin_is_payload: true,
                    ..terminal()
                },
            ),
        ] {
            assert!(!mode.person_on_stderr(), "{name}");
        }
    }

    #[test]
    fn unprompted_notices_need_both_streams_on_a_terminal() {
        assert!(terminal().person_at_terminal());
        assert!(
            Mode {
                non_interactive_flag: true,
                ..terminal()
            }
            .person_at_terminal(),
            "--non-interactive only forbids prompts"
        );
        for (name, mode) in [
            (
                "stdout piped",
                Mode {
                    stdout_tty: false,
                    ..terminal()
                },
            ),
            (
                "stderr piped",
                Mode {
                    stderr_tty: false,
                    ..terminal()
                },
            ),
            (
                "json",
                Mode {
                    machine_format: true,
                    ..terminal()
                },
            ),
            (
                "agent",
                Mode {
                    agent: true,
                    ..terminal()
                },
            ),
            (
                "ci",
                Mode {
                    ci: true,
                    ..terminal()
                },
            ),
        ] {
            assert!(!mode.person_at_terminal(), "{name}");
        }
    }

    #[test]
    fn live_display_needs_a_terminal_stdout_and_no_driver() {
        assert!(terminal().live_display());
        for (name, mode) in [
            (
                "stdout piped",
                Mode {
                    stdout_tty: false,
                    ..terminal()
                },
            ),
            (
                "agent",
                Mode {
                    agent: true,
                    ..terminal()
                },
            ),
            (
                "ci",
                Mode {
                    ci: true,
                    ..terminal()
                },
            ),
            (
                "non-interactive",
                Mode {
                    non_interactive_flag: true,
                    ..terminal()
                },
            ),
        ] {
            assert!(!mode.live_display(), "{name}");
        }
    }
}
