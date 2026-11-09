//! The interaction context of one run: who, if anyone, is at the other end.
//!
//! [`Mode`] is built once at startup by [`Mode::resolve`] from the probed
//! [`Terminal`], the environment, the flags and the output format. It is the
//! only owner of agent, CI and terminal state, and of the rule that an agent or
//! a non-terminal stdout defaults to JSON: each question the CLI asks about its
//! surroundings has exactly one predicate here, so every command and every
//! pipeline step agrees. Nothing else reads `HYPERLIQUID_AGENT`, `CI` or a TTY
//! to decide whether to prompt, bound a stream, announce or redraw.

use clap::ValueEnum;

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

/// Why a run is not a person at a terminal, which a live view, the watch TUI
/// and a human-format stream each need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayBlocker {
    /// Neither stdin nor stdout is a terminal.
    NoTerminal,
    StdoutNotTerminal,
    StdinNotTerminal,
    /// An agent, CI or `--non-interactive` is driving the run.
    Driven,
}

impl Mode {
    /// The mode of one run, from what the command line asked for and what the
    /// environment says, read in the one order that makes errors reportable:
    /// the output format first, because it names the format every later error
    /// is reported in, then the strict boolean variables. The format is
    /// `requested` (`--json` or `--format`), else `HYPERLIQUID_FORMAT`, else
    /// JSON for an agent or a non-terminal stdout, else the default.
    ///
    /// An unreadable switch counts as off while the format is resolved and is
    /// then rejected in that format. An unreadable `HYPERLIQUID_FORMAT` cannot
    /// be reported in the format it names, so it is reported as JSON. The
    /// error comes with the format to report it in.
    pub fn resolve(
        requested: Option<OutputFormat>,
        non_interactive_flag: bool,
        stdin_is_payload: bool,
        terminal: Terminal,
        source: &EnvSource,
    ) -> Result<Self, (CliError, OutputFormat)> {
        let surroundings = Self::surroundings(terminal, source);
        let format = surroundings
            .output_format(requested, source)
            .map_err(|err| (err, OutputFormat::Json))?;
        validate_env(source).map_err(|err| (err, format))?;
        Ok(surroundings.running(format, non_interactive_flag, stdin_is_payload))
    }

    /// The format to report a command line that did not parse in, with
    /// whatever `requested` it still showed. An unreadable `HYPERLIQUID_FORMAT`
    /// is reported as JSON.
    #[must_use]
    pub fn error_format(
        requested: Option<OutputFormat>,
        terminal: Terminal,
        source: &EnvSource,
    ) -> OutputFormat {
        Self::surroundings(terminal, source)
            .output_format(requested, source)
            .unwrap_or(OutputFormat::Json)
    }

    /// The surroundings: the probed `terminal` and the agent and CI variables
    /// of `env`. The format and flags join later through [`Self::running`],
    /// because the format itself depends on these surroundings.
    fn surroundings(terminal: Terminal, env: &EnvSource) -> Self {
        Self {
            terminal,
            // An invalid `HYPERLIQUID_AGENT` counts as off here;
            // [`validate_env`] rejects it.
            agent: env::flag(env, env::AGENT) == EnvFlag::On,
            ci: env::flag(env, env::CI).on_unless_off(),
            ..Self::default()
        }
    }

    fn output_format(
        &self,
        requested: Option<OutputFormat>,
        source: &EnvSource,
    ) -> Result<OutputFormat, CliError> {
        if let Some(format) = requested {
            return Ok(format);
        }
        if let Some(format) = env_output_format(source) {
            return format;
        }
        if self.agent || !self.terminal.stdout {
            return Ok(OutputFormat::Json);
        }
        Ok(OutputFormat::default())
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

    /// An agent, CI or `--non-interactive` is driving the run.
    fn driven(&self) -> bool {
        self.agent || self.ci || self.non_interactive_flag
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

    /// Why the run may not redraw the terminal in place (a live view), in the
    /// order they are reported. `None` when it may.
    #[must_use]
    pub fn live_display_blocker(&self) -> Option<DisplayBlocker> {
        if !self.terminal.stdout {
            Some(DisplayBlocker::StdoutNotTerminal)
        } else if self.driven() {
            Some(DisplayBlocker::Driven)
        } else {
            None
        }
    }

    /// The run may redraw the terminal in place (a live view): stdout is a
    /// terminal and neither an agent, CI nor `--non-interactive` is driving.
    #[must_use]
    pub fn live_display(&self) -> bool {
        self.live_display_blocker().is_none()
    }

    /// Why the run may not take over the whole terminal (the watch TUI): a
    /// live display that also reads keys from a terminal stdin. `None` when it
    /// may.
    #[must_use]
    pub fn full_screen_blocker(&self) -> Option<DisplayBlocker> {
        match (self.terminal.stdin, self.terminal.stdout) {
            (false, false) => Some(DisplayBlocker::NoTerminal),
            (false, true) => Some(DisplayBlocker::StdinNotTerminal),
            (true, _) => self.live_display_blocker(),
        }
    }

    /// The run may take over the whole terminal (the watch TUI).
    #[must_use]
    pub fn full_screen(&self) -> bool {
        self.full_screen_blocker().is_none()
    }
}

fn env_output_format(source: &EnvSource) -> Option<Result<OutputFormat, CliError>> {
    let raw = source.get(env::FORMAT)?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(parse_env_output_format(&raw))
}

fn parse_env_output_format(raw: &str) -> Result<OutputFormat, CliError> {
    <OutputFormat as ValueEnum>::from_str(raw.trim(), true).map_err(|_| {
        CliError::Configuration(format!(
            "{} must be one of pretty, table, or json; got '{}'",
            env::FORMAT,
            raw.trim().to_ascii_lowercase()
        ))
    })
}

/// Parse a boolean environment variable strictly.
///
/// Unset or empty means `None`. Any value outside `1|true|yes|on|0|false|no|off`
/// (case-insensitive) is a configuration error naming the variable, so a typo
/// never silently falls back to the default.
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
fn validate_env(source: &EnvSource) -> Result<(), CliError> {
    for name in env::strict_flags() {
        flag_value(name, env::flag(source, name))?;
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
    fn the_surroundings_read_agent_and_ci_from_the_environment_and_keep_the_terminal() {
        let terminal = Terminal {
            stdin: true,
            stdout: false,
            stderr: true,
        };
        let env = |pairs: &[(&'static str, &'static str)]| {
            Mode::surroundings(terminal, &EnvSource::fixed(pairs.iter().copied()))
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

    #[test]
    fn the_display_blockers_name_the_first_reason_in_the_order_they_are_reported() {
        use DisplayBlocker::{Driven, NoTerminal, StdinNotTerminal, StdoutNotTerminal};
        assert_eq!(terminal().live_display_blocker(), None);
        assert_eq!(terminal().full_screen_blocker(), None);
        assert_eq!(
            tty(true, false, true).live_display_blocker(),
            Some(StdoutNotTerminal)
        );
        assert_eq!(tty(false, true, true).live_display_blocker(), None);
        assert_eq!(
            tty(false, false, true).full_screen_blocker(),
            Some(NoTerminal)
        );
        assert_eq!(
            tty(false, true, true).full_screen_blocker(),
            Some(StdinNotTerminal)
        );
        assert_eq!(
            tty(true, false, true).full_screen_blocker(),
            Some(StdoutNotTerminal)
        );
        for driver in [
            Mode {
                agent: true,
                ..terminal()
            },
            Mode {
                ci: true,
                ..terminal()
            },
            Mode {
                non_interactive_flag: true,
                ..terminal()
            },
        ] {
            assert_eq!(driver.live_display_blocker(), Some(Driven));
            assert_eq!(driver.full_screen_blocker(), Some(Driven));
        }
        let piped_driver = Mode {
            agent: true,
            ..tty(false, true, true)
        };
        assert_eq!(
            piped_driver.full_screen_blocker(),
            Some(StdinNotTerminal),
            "a missing terminal is reported before a driver"
        );
    }

    fn quiet() -> (Terminal, EnvSource) {
        (Terminal::default(), EnvSource::fixed::<&str, &str>([]))
    }

    #[test]
    fn the_format_is_the_flag_then_the_environment_then_what_the_surroundings_suggest() {
        let on_a_terminal = Terminal {
            stdin: true,
            stdout: true,
            stderr: true,
        };
        let none = EnvSource::fixed::<&str, &str>([]);
        let agent = EnvSource::fixed([(env::AGENT, "1")]);
        let json_and_agent = EnvSource::fixed([(env::FORMAT, "json"), (env::AGENT, "1")]);
        let table = EnvSource::fixed([(env::FORMAT, "table")]);
        let cases = [
            (
                Some(OutputFormat::Table),
                &json_and_agent,
                on_a_terminal,
                OutputFormat::Table,
            ),
            (
                Some(OutputFormat::Pretty),
                &table,
                on_a_terminal,
                OutputFormat::Pretty,
            ),
            (None, &table, on_a_terminal, OutputFormat::Table),
            (None, &none, on_a_terminal, OutputFormat::default()),
            (None, &agent, on_a_terminal, OutputFormat::Json),
            (None, &none, Terminal::default(), OutputFormat::Json),
        ];
        for (requested, source, terminal, expected) in cases {
            let mode = Mode::resolve(requested, false, false, terminal, source).unwrap();
            assert_eq!(mode.format, expected, "{requested:?} {terminal:?}");
        }
    }

    #[test]
    fn an_unreadable_variable_is_reported_in_the_requested_format_or_json() {
        let (terminal, _) = quiet();
        let unreadable_format = EnvSource::fixed([(env::FORMAT, "yaml"), (env::AGENT, "banana")]);
        let (err, format) =
            Mode::resolve(None, false, false, terminal, &unreadable_format).unwrap_err();
        assert!(err.to_string().contains(env::FORMAT), "{err}");
        assert_eq!(format, OutputFormat::Json);

        for name in [env::AGENT, env::NO_UPDATE_CHECK] {
            let source = EnvSource::fixed([(name, "banana")]);
            let (err, format) =
                Mode::resolve(Some(OutputFormat::Table), false, false, terminal, &source)
                    .unwrap_err();
            assert!(err.to_string().contains(name), "{err}");
            assert_eq!(format, OutputFormat::Table);
        }
    }

    #[test]
    fn resolve_builds_the_one_mode_of_the_run() {
        let terminal = Terminal {
            stdin: true,
            stdout: true,
            stderr: true,
        };
        let source = EnvSource::fixed([(env::AGENT, "1"), (env::CI, "1")]);
        let mode = Mode::resolve(None, true, true, terminal, &source).unwrap();
        assert_eq!(mode.terminal, terminal);
        assert!(mode.agent && mode.ci && mode.non_interactive_flag && mode.stdin_is_payload);
        assert_eq!(mode.format, OutputFormat::Json);
    }

    #[test]
    fn the_environment_format_is_case_insensitive_and_names_the_variable_when_unreadable() {
        assert_eq!(
            parse_env_output_format(" Table ").unwrap(),
            OutputFormat::Table
        );
        let err = parse_env_output_format("YAML").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Configuration error: HYPERLIQUID_FORMAT must be one of pretty, table, or json; got 'yaml'"
        );
    }

    #[test]
    fn a_failed_parse_reports_in_the_requested_format_or_json_when_the_variable_is_unreadable() {
        let (terminal, source) = quiet();
        assert_eq!(
            Mode::error_format(Some(OutputFormat::Pretty), terminal, &source),
            OutputFormat::Pretty
        );
        let unreadable = EnvSource::fixed([(env::FORMAT, "yaml")]);
        assert_eq!(
            Mode::error_format(None, terminal, &unreadable),
            OutputFormat::Json
        );
    }
}
