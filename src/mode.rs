//! The interaction context of one run: who, if anyone, is at the other end.
//!
//! [`Mode`] is built once at startup from the probed [`Terminal`], the
//! environment, the flags and the output format. It is the only owner of
//! agent, CI and terminal state: each question the CLI asks about its
//! surroundings has exactly one predicate here, so every command and every
//! pipeline step agrees. Nothing else reads `HYPERLIQUID_AGENT`, `CI` or a TTY
//! to decide whether to prompt, bound a stream, announce or redraw.

use crate::env::{self, EnvFlag, EnvSource};
use crate::errors::CliError;
use crate::terminal::{OutputFormat, Terminal};

/// `--non-interactive`, as named in the error that says why a prompt is off.
pub const NON_INTERACTIVE_FLAG: &str = "--non-interactive";
const JSON_OUTPUT: &str = "JSON output";
const STDIN_PAYLOAD: &str = "stdin is used as the payload";

/// What decides whether this invocation may prompt, announce or redraw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mode {
    pub terminal: Terminal,
    /// `HYPERLIQUID_AGENT` is on.
    pub agent: bool,
    pub ci: bool,
    /// The effective output format (explicit, env, agent, or non-TTY default).
    pub format: OutputFormat,
    pub non_interactive_flag: bool,
    /// Stdin carries a payload (`--payload-file -`), so it can't answer prompts.
    pub stdin_is_payload: bool,
}

impl Mode {
    /// The surroundings: the probed `terminal` and the agent and CI variables
    /// of `env`. The format and flags join later through [`Self::running`],
    /// because the format itself depends on these surroundings.
    #[must_use]
    pub fn new(terminal: Terminal, env: &EnvSource) -> Self {
        Self {
            terminal,
            // An invalid `HYPERLIQUID_AGENT` counts as off here;
            // [`validate_env`] rejects it.
            agent: env::flag(env, env::AGENT) == EnvFlag::On,
            ci: env::flag(env, env::CI).on_unless_off(),
            ..Self::default()
        }
    }

    /// The same surroundings for a run that prints `format` and was given the
    /// flags `--non-interactive` and `--payload-file -`.
    #[must_use]
    pub fn running(
        self,
        format: OutputFormat,
        non_interactive_flag: bool,
        stdin_is_payload: bool,
    ) -> Self {
        Self {
            format,
            non_interactive_flag,
            stdin_is_payload,
            ..self
        }
    }

    #[must_use]
    pub fn machine_format(&self) -> bool {
        self.format == OutputFormat::Json
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
            Some(NON_INTERACTIVE_FLAG)
        } else if self.stdin_is_payload {
            Some(STDIN_PAYLOAD)
        } else if self.machine_format() {
            Some(JSON_OUTPUT)
        } else if self.agent {
            Some(env::AGENT)
        } else if self.ci {
            Some(env::CI)
        } else {
            None
        }
    }

    /// The prompt blockers a person or agent can switch on, for help text.
    #[must_use]
    pub fn switchable_blockers() -> String {
        format!(
            "{JSON_OUTPUT}, {}, {}, {NON_INTERACTIVE_FLAG}",
            env::AGENT,
            env::CI
        )
    }

    /// A program, not a person, consumes this run's output: JSON format or an
    /// agent. Endless streams need an explicit bound and secrets may not arrive
    /// on the command line. CI alone does not make a context machine-readable.
    #[must_use]
    pub fn machine_context(&self) -> bool {
        self.machine_format() || self.agent
    }

    /// No person reads this run: a JSON format, an agent or CI.
    fn unattended(&self) -> bool {
        self.machine_format() || self.agent || self.ci
    }

    /// Notices addressed to a person on stderr, such as retry announcements:
    /// stderr is a terminal and the run could prompt that person.
    #[must_use]
    pub fn person_on_stderr(&self) -> bool {
        self.terminal.stderr && self.can_prompt()
    }

    /// The "Completed in" footer may print on stderr: a human format, stderr is
    /// a terminal, and no agent or CI reads the run.
    #[must_use]
    pub fn timing_footer(&self) -> bool {
        !self.unattended() && self.terminal.stderr
    }

    /// An unprompted notice may interrupt the person at this terminal, for
    /// example the passive update notice or `--open`: human formats, no agent
    /// or CI, and both output streams are terminals.
    #[must_use]
    pub fn person_at_terminal(&self) -> bool {
        !self.unattended() && self.terminal.stdout && self.terminal.stderr
    }

    /// The run may redraw the terminal in place (a live view): stdout is a
    /// terminal and neither an agent, CI nor `--non-interactive` is driving.
    #[must_use]
    pub fn live_display(&self) -> bool {
        self.terminal.stdout && !self.agent && !self.ci && !self.non_interactive_flag
    }

    /// The run may take over the whole terminal (the watch TUI): a live
    /// display that also reads keys from a terminal stdin.
    #[must_use]
    pub fn full_screen(&self) -> bool {
        self.live_display() && self.terminal.stdin
    }
}

/// Parse a boolean environment variable strictly.
///
/// Unset or empty means `None`. Any value outside `1|true|yes|on|0|false|no|off`
/// (case-insensitive) is a configuration error naming the variable, so a typo
/// never silently falls back to the default.
fn env_bool(env: &EnvSource, name: &str) -> Result<Option<bool>, CliError> {
    flag_value(name, env::flag(env, name))
}

/// [`env_bool`] for a switch that is off unless the variable says otherwise.
pub fn env_enabled(env: &EnvSource, name: &str) -> Result<bool, CliError> {
    Ok(env_bool(env, name)?.unwrap_or(false))
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

/// Reject an invalid value of every strict flag in the [`env`] table, so a
/// typo in `HYPERLIQUID_AUTO_UPDATE` fails the run like one in `HYPERLIQUID_AGENT`
/// instead of reading as off.
pub fn validate_env(source: &EnvSource) -> Result<(), CliError> {
    for name in env::strict_flags() {
        env_bool(source, name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::parse_flag;

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
    fn every_strict_flag_in_the_table_is_validated_by_name() {
        for name in env::strict_flags() {
            let err = validate_env(&EnvSource::fixed([(name, "banana")])).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{name}");
            assert!(err.to_string().contains(name), "{name}: {err}");
            for fine in ["1", "TRUE", " off ", ""] {
                assert!(
                    validate_env(&EnvSource::fixed([(name, fine)])).is_ok(),
                    "{name}={fine:?}"
                );
            }
        }
    }

    #[test]
    fn a_typo_in_auto_update_or_mcp_allow_unsafe_fails_every_run_like_agent_does() {
        for name in [env::AUTO_UPDATE, env::MCP_ALLOW_UNSAFE] {
            let err = validate_env(&EnvSource::fixed([(name, "yes please")])).unwrap_err();
            assert!(matches!(err, CliError::Configuration(_)), "{name}");
            assert_eq!(
                err.to_string(),
                format!(
                    "Configuration error: {name} must be one of 1, true, yes, on, 0, false, no, off"
                )
            );
        }
    }

    fn json() -> Mode {
        Mode {
            format: OutputFormat::Json,
            ..Mode::default()
        }
    }

    #[test]
    fn new_reads_agent_and_ci_from_the_environment_and_keeps_the_terminal() {
        let terminal = Terminal {
            stdin: true,
            stdout: false,
            stderr: true,
        };
        let env = |pairs: &[(&'static str, &'static str)]| {
            Mode::new(terminal, &EnvSource::fixed(pairs.iter().copied()))
        };
        let plain = env(&[]);
        assert_eq!(plain.terminal, terminal);
        assert!(!plain.agent && !plain.ci);
        assert!(env(&[("HYPERLIQUID_AGENT", "1")]).agent);
        assert!(
            !env(&[("HYPERLIQUID_AGENT", "banana")]).agent,
            "an invalid value is rejected by validate_env, not read as on"
        );
        assert!(env(&[("CI", "true")]).ci);
        assert!(!env(&[("CI", "false")]).ci);
    }

    #[test]
    fn running_adds_the_format_and_flags_without_touching_the_surroundings() {
        let surroundings = Mode {
            agent: true,
            terminal: Terminal {
                stdin: true,
                ..Terminal::default()
            },
            ..Mode::default()
        };
        let run = surroundings.running(OutputFormat::Json, true, true);
        assert_eq!(run.format, OutputFormat::Json);
        assert!(run.machine_format() && run.non_interactive_flag && run.stdin_is_payload);
        assert!(run.agent && run.terminal.stdin);
        assert!(
            !surroundings
                .running(OutputFormat::Table, false, false)
                .machine_format()
        );
    }

    #[test]
    fn prompts_are_blocked_by_the_first_matching_reason() {
        assert!(Mode::default().can_prompt());
        assert_eq!(Mode::default().prompt_blocker(), None);

        let flagged = Mode {
            non_interactive_flag: true,
            ..json()
        };
        assert_eq!(flagged.prompt_blocker(), Some("--non-interactive"));

        let payload = Mode {
            stdin_is_payload: true,
            ..json()
        };
        assert_eq!(
            payload.prompt_blocker(),
            Some("stdin is used as the payload")
        );

        let json = Mode {
            agent: true,
            ..json()
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

    #[test]
    fn help_names_every_switchable_blocker_the_same_way_errors_do() {
        let help = Mode::switchable_blockers();
        for reason in [
            json().prompt_blocker(),
            Mode {
                agent: true,
                ..Mode::default()
            }
            .prompt_blocker(),
            Mode {
                ci: true,
                ..Mode::default()
            }
            .prompt_blocker(),
            Mode {
                non_interactive_flag: true,
                ..Mode::default()
            }
            .prompt_blocker(),
        ] {
            let reason = reason.unwrap();
            assert!(help.contains(reason), "{help} lacks {reason}");
        }
    }

    fn terminal() -> Mode {
        tty(true, true, true)
    }

    fn tty(stdin: bool, stdout: bool, stderr: bool) -> Mode {
        Mode {
            terminal: Terminal {
                stdin,
                stdout,
                stderr,
            },
            ..Mode::default()
        }
    }

    #[test]
    fn machine_context_is_json_or_agent_but_not_ci() {
        assert!(!Mode::default().machine_context());
        assert!(json().machine_context());
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
            ("stderr piped", tty(true, true, false)),
            (
                "json",
                Mode {
                    format: OutputFormat::Json,
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
    fn timing_footer_is_for_human_formats_on_a_stderr_terminal() {
        assert!(terminal().timing_footer());
        assert!(
            Mode {
                non_interactive_flag: true,
                ..tty(true, false, true)
            }
            .timing_footer(),
            "only a stderr terminal and a human format matter"
        );
        for (name, mode) in [
            ("stderr piped", tty(true, true, false)),
            (
                "json",
                Mode {
                    format: OutputFormat::Json,
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
            assert!(!mode.timing_footer(), "{name}");
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
            ("stdout piped", tty(true, false, true)),
            ("stderr piped", tty(true, true, false)),
            (
                "json",
                Mode {
                    format: OutputFormat::Json,
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
            ("stdout piped", tty(true, false, true)),
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

    #[test]
    fn full_screen_is_a_live_display_that_can_also_read_keys() {
        assert!(terminal().full_screen());
        assert!(!tty(false, true, true).full_screen());
        assert!(!tty(true, false, true).full_screen());
        assert!(
            !Mode {
                agent: true,
                ..terminal()
            }
            .full_screen()
        );
    }
}
