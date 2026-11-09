//! Command-line shell around the clap tree: parsing, help and usage-error
//! rendering, panic handling, and output-format resolution.
//!
//! `main.rs` declares the global options and boots the process; the commands
//! come from `commands::entries`; everything that decides how the command line
//! is presented to people and agents lives here.

use std::ffi::OsString;
use std::io::Write;

use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches};
use hyperliquid_cli::command_spec::{
    CommandSpec, ConfirmationPolicy, DryRunPolicy, Entry, Risk, YES_ARG,
};
use hyperliquid_cli::commands;
use hyperliquid_cli::errors;
use hyperliquid_cli::output;
use hyperliquid_cli::settings::{EnvSource, env};

use crate::{Cli, GLOBAL_OPTIONS};

const REPORT_URL: &str = "https://github.com/hypurr-inc/hyperliquid-cli/issues/new";

/// The command the line selected, with its args parsed.
pub(super) struct Selected {
    pub(super) entry: &'static Entry,
    /// The matches of the selected command itself.
    pub(super) matches: ArgMatches,
    pub(super) bound: Box<dyn hyperliquid_cli::command_spec::Bound>,
}

pub(super) struct ParsedCli {
    pub(super) cli: Cli,
    /// `None` when the line names no command (help is shown).
    pub(super) selected: Option<Selected>,
    /// `--json` or `--format` as typed on the command line; env and defaults are not.
    pub(super) requested_format: Option<output::OutputFormat>,
}

pub(super) fn try_parse_cli_from<I, T>(args: I) -> Result<ParsedCli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut command = build_command();
    let matches = command.try_get_matches_from_mut(args)?;
    let requested_format = requested_format(&matches);
    let cli = Cli::from_arg_matches(&matches)?;
    let selected = match command_path_from_matches(&matches) {
        Some(path) => {
            let entry = commands::entries()
                .iter()
                .find(|entry| entry.spec.path == path)
                .ok_or_else(|| {
                    clap::Error::raw(
                        clap::error::ErrorKind::InvalidSubcommand,
                        format!("command '{}' is not registered", path.join(" ")),
                    )
                })?;
            let leaf = leaf_matches(&matches);
            Some(Selected {
                entry,
                bound: entry.bind(leaf)?,
                matches: leaf.clone(),
            })
        }
        None => None,
    };
    Ok(ParsedCli {
        cli,
        selected,
        requested_format,
    })
}

fn leaf_matches(matches: &ArgMatches) -> &ArgMatches {
    let mut leaf = matches;
    while let Some((_, subcommand)) = leaf.subcommand() {
        leaf = subcommand;
    }
    leaf
}

fn command_path_from_matches(matches: &ArgMatches) -> Option<Vec<String>> {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, subcommand)) = current.subcommand() {
        path.push(name.to_string());
        current = subcommand;
    }
    (!path.is_empty()).then_some(path)
}

// ── Command tree ────────────────────────────────────────────────────────

/// The clap command tree: the global options, `-v`, and the registered commands
/// with help text filled from each command's spec and notes on every command
/// that changes state.
fn build_command() -> clap::Command {
    let mut tree = commands::command_tree().clone();
    for entry in commands::entries() {
        tree = decorate_at(tree, entry.spec.path, entry.spec);
    }
    Cli::command()
        .arg(
            clap::Arg::new("help")
                .short('h')
                .long("help")
                .global(true)
                .action(clap::ArgAction::HelpLong)
                .help("Print help")
                .help_heading(GLOBAL_OPTIONS),
        )
        .arg(
            clap::Arg::new("version")
                .short('v')
                .short_alias('V')
                .long("version")
                .global(true)
                .action(clap::ArgAction::Version)
                .help("Print version")
                .help_heading(GLOBAL_OPTIONS),
        )
        .subcommands(tree.get_subcommands().cloned())
}

/// Decorate the command at `path` below `command`.
fn decorate_at(command: clap::Command, path: &[&str], spec: &CommandSpec) -> clap::Command {
    match path.split_first() {
        Some((first, rest)) => command.mut_subcommand(*first, |sub| decorate_at(sub, rest, spec)),
        None => decorate_command(command, spec),
    }
}

/// Fill missing option help from the spec's descriptions and append the spec's
/// notes when the command changes state.
fn decorate_command(mut command: clap::Command, spec: &CommandSpec) -> clap::Command {
    let undocumented: Vec<String> = command
        .get_arguments()
        .filter(|arg| arg.get_help().is_none() && arg.get_long_help().is_none())
        .map(|arg| arg.get_id().to_string())
        .collect();
    for id in undocumented {
        if let Some(description) = spec.arg(&id).and_then(|arg| arg.description) {
            let help = description.trim_end_matches('.').to_string();
            command = command.mut_arg(&id, |arg| arg.help(help));
        }
    }

    let has_yes = command.get_arguments().any(|arg| arg.get_id() == YES_ARG);
    if let Some(notes) = command_notes(spec, has_yes) {
        let combined = match command.get_after_long_help() {
            Some(existing) => format!("{existing}\n\n{notes}"),
            None => notes,
        };
        command = command.after_long_help(combined);
    }
    command
}

/// `Notes:` block for commands that change state: risk, dry-run support, and
/// how confirmation behaves.
fn command_notes(spec: &CommandSpec, has_yes: bool) -> Option<String> {
    if !spec.changes_state() {
        return None;
    }
    let risk = match spec.risk {
        Risk::FundsMovement => "moves funds or changes live exchange state",
        Risk::AccountState => "changes account settings on the exchange",
        Risk::LocalSecret => "reads or writes secret key material on this machine",
        Risk::LocalState => "changes local configuration or wallet state",
        Risk::None => "changes live exchange state",
    };
    let mut lines = vec!["Notes:".to_string(), format!("  Risk: {risk}.")];
    lines.push(match spec.dry_run {
        DryRunPolicy::Optional => {
            "  Dry-run: supported. Add --dry-run to preview without side effects.".to_string()
        }
        DryRunPolicy::NotSupported => "  Dry-run: not supported.".to_string(),
    });
    lines.push(match (spec.confirmation, has_yes) {
        (ConfirmationPolicy::Prompt, true) => format!("  Confirmation: asks before it runs. Pass --{YES_ARG} to answer the prompt. Runs that can't prompt (JSON output, {}, {}, --non-interactive) fail without --{YES_ARG}. Use --{YES_ARG} there only after a human approves.", env::AGENT, env::CI),
        (ConfirmationPolicy::Prompt, false) => format!("  Confirmation: asks before it runs. Runs that can't prompt (JSON output, {}, {}, --non-interactive) fail instead.", env::AGENT, env::CI),
        (ConfirmationPolicy::None, _) => {
            "  Confirmation: none. It runs as soon as you invoke it.".to_string()
        }
    });
    if spec.testnet_unprompted {
        lines.push(format!(
            "  Testnet: on testnet (--testnet or {}=testnet) this command doesn't prompt and runs as soon as you invoke it.",
            env::NETWORK
        ));
    }
    Some(lines.join("\n"))
}

// ── Help, usage errors, and exit paths ──────────────────────────────────

fn styled_text(styled: &clap::builder::StyledStr, color: bool) -> String {
    if color {
        styled.ansi().to_string()
    } else {
        styled.to_string()
    }
}

/// Write to stdout. A closed pipe (`| head`) is a normal end, not an error.
fn write_stdout(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn exit_after_stdout(text: &str) -> ! {
    let code = match write_stdout(text) {
        Ok(()) => 0,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "Failed to write to stdout: {err}");
            1
        }
    };
    std::process::exit(code)
}

/// The deepest command along `path` that exists.
fn find_command<'a>(root: &'a clap::Command, path: &[String]) -> &'a clap::Command {
    let mut command = root;
    for name in path {
        match command.find_subcommand(name) {
            Some(sub) => command = sub,
            None => break,
        }
    }
    command
}

fn help_text_for_path(path: &[String]) -> String {
    let mut root = build_command();
    root.build();
    let mut target = find_command(&root, path).clone();
    styled_text(&target.render_long_help(), output::colors::stdout_enabled())
}

/// Print the long help for `path` (the root when empty) on stdout.
pub(super) fn print_help_to_stdout(path: &[String]) -> Result<(), anyhow::Error> {
    write_stdout(&help_text_for_path(path))
        .map_err(|err| anyhow::anyhow!("failed to write help to stdout: {err}"))
}

fn help_command(path: &[String]) -> String {
    if path.is_empty() {
        "hyperliquid --help".to_string()
    } else {
        format!("hyperliquid {} --help", path.join(" "))
    }
}

fn is_help_or_version_token(arg: &OsString) -> bool {
    matches!(
        arg.to_str(),
        Some("-h" | "--help" | "-v" | "-V" | "--version")
    )
}

/// Parse as much of the command line as possible, skipping what clap rejects.
///
/// Used only after a real parse failed, to learn which command was reached and
/// which global options were given, so errors can be rendered in the right
/// format and point at the right help page.
fn lenient_matches(raw_args: &[OsString]) -> Option<ArgMatches> {
    let mut args: Vec<OsString> = raw_args
        .iter()
        .filter(|arg| !is_help_or_version_token(arg))
        .cloned()
        .collect();
    let mut command = build_command().ignore_errors(true);
    for _ in 0..8 {
        match command.try_get_matches_from_mut(&args) {
            Ok(matches) => return Some(matches),
            Err(err) => {
                let mut indexes = offending_arg_indexes(&err, &args)?;
                indexes.sort_unstable_by(|a, b| b.cmp(a));
                for index in indexes {
                    args.remove(index);
                }
            }
        }
    }
    None
}

/// Positions in `args` of the token clap rejected (and its value, if any).
fn offending_arg_indexes(err: &clap::Error, args: &[OsString]) -> Option<Vec<usize>> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};

    let context_string = |kind: ContextKind| match err.get(kind) {
        Some(ContextValue::String(value)) => Some(value.clone()),
        _ => None,
    };
    let (name, has_value) = match err.kind() {
        ErrorKind::UnknownArgument => (context_string(ContextKind::InvalidArg)?, false),
        ErrorKind::InvalidSubcommand => (context_string(ContextKind::InvalidSubcommand)?, false),
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            (context_string(ContextKind::InvalidArg)?, true)
        }
        _ => return None,
    };
    let name = name.split([' ', '=']).next()?.to_string();
    let with_equals = format!("{name}=");
    let index = args.iter().position(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == name || arg.starts_with(&with_equals))
    })?;
    let mut indexes = vec![index];
    let attached = args[index].to_str().is_some_and(|arg| arg.contains('='));
    if has_value && !attached && index + 1 < args.len() {
        indexes.push(index + 1);
    }
    Some(indexes)
}

/// Output format for a failed parse. It cannot report an unreadable
/// `HYPERLIQUID_FORMAT` in the format that variable names, so it reports JSON.
fn error_format(matches: Option<&ArgMatches>, source: &EnvSource) -> output::OutputFormat {
    let requested = matches.and_then(requested_format);
    let probed = output::terminal::OutputEnv::probe(requested.is_some(), source);
    resolve_output_format(requested, &probed, source).unwrap_or(output::OutputFormat::Json)
}

/// Keep only clap's best suggestion. clap lists the closest match last.
fn narrow_suggestions(err: &mut clap::Error) {
    use clap::error::{ContextKind, ContextValue};

    for kind in [
        ContextKind::SuggestedSubcommand,
        ContextKind::SuggestedArg,
        ContextKind::SuggestedValue,
    ] {
        let best = match err.get(kind) {
            Some(ContextValue::Strings(values)) if values.len() > 1 => values.last().cloned(),
            _ => None,
        };
        if let Some(best) = best {
            err.insert(kind, ContextValue::Strings(vec![best]));
        }
    }
}

/// Render a usage error: one suggestion, the usage line of the command that
/// was actually reached, and a pointer to its help.
fn usage_error_text(mut err: clap::Error, path: &[String], color: bool) -> String {
    use clap::error::{ContextKind, ContextValue};

    narrow_suggestions(&mut err);
    let mut root = build_command();
    root.build();
    if err.get(ContextKind::Usage).is_some() {
        let usage = find_command(&root, path).clone().render_usage();
        err.insert(ContextKind::Usage, ContextValue::StyledStr(usage));
    }
    let text = styled_text(&err.render(), color);
    let body = match text.rfind("\n\nFor more information") {
        Some(index) => &text[..index],
        None => text.trim_end(),
    };
    format!("{body}\n\nSee all options:\n    {}\n", help_command(path))
}

/// Print what clap reported and exit. Help and version are successes on
/// stdout; everything else is a usage error (exit 2).
pub(super) fn exit_for_clap_error(
    err: clap::Error,
    raw_args: &[OsString],
    source: &EnvSource,
) -> ! {
    use clap::error::ErrorKind;

    let lenient = lenient_matches(raw_args);
    // A partial parse may not have applied flag defaults, so read the flag fallibly.
    output::colors::init(
        lenient.as_ref().is_some_and(|matches| {
            matches
                .try_get_one::<bool>("no_color")
                .ok()
                .flatten()
                .copied()
                .unwrap_or(false)
        }),
        source,
    );
    let path = lenient
        .as_ref()
        .and_then(command_path_from_matches)
        .unwrap_or_default();
    match err.kind() {
        ErrorKind::DisplayHelp => exit_after_stdout(&styled_text(
            &err.render(),
            output::colors::stdout_enabled(),
        )),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            exit_after_stdout(&help_text_for_path(&path))
        }
        ErrorKind::DisplayVersion => {
            let version = env!("CARGO_PKG_VERSION");
            let text = if error_format(lenient.as_ref(), source) == output::OutputFormat::Json {
                format!("{}\n", serde_json::json!({ "version": version }))
            } else {
                format!("hyperliquid {version}\n")
            };
            exit_after_stdout(&text)
        }
        _ => {
            let exit_code = err.exit_code();
            if error_format(lenient.as_ref(), source) == output::OutputFormat::Json {
                let text = usage_error_text(err, &path, false);
                exit_after_stdout_with_code(
                    &format!(
                        "{}\n",
                        errors::ErrorEnvelope::usage(text.trim_end()).to_json()
                    ),
                    exit_code,
                );
            }
            let text = usage_error_text(err, &path, output::colors::stderr_enabled());
            let _ = write!(std::io::stderr(), "{text}");
            std::process::exit(exit_code)
        }
    }
}

fn exit_after_stdout_with_code(text: &str, code: i32) -> ! {
    let _ = write_stdout(text);
    std::process::exit(code)
}

// ── Panics ──────────────────────────────────────────────────────────────

/// A panic from `println!`/`eprintln!` on a closed pipe.
fn is_broken_pipe_panic(message: &str) -> bool {
    message.contains("Broken pipe")
        && (message.contains("failed printing to stdout")
            || message.contains("failed printing to stderr"))
}

/// A closed pipe ends the process quietly. Any other panic is a bug: say so
/// with the command path (never argument values) and exit 1.
pub(super) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if is_broken_pipe_panic(&message) {
            std::process::exit(0);
        }
        let command = match errors::command_path() {
            Some(path) => format!("hyperliquid {path}"),
            None => "hyperliquid".to_string(),
        };
        let _ = write!(
            std::io::stderr(),
            "Unexpected error in {command}. This is a bug in hyperliquid-cli.\nVersion: {}\nReport it at:\n    {REPORT_URL}\n",
            env!("CARGO_PKG_VERSION"),
        );
        // Developers opt in to the panic message and location with RUST_BACKTRACE.
        if std::env::var("RUST_BACKTRACE").is_ok_and(|value| !value.is_empty() && value != "0") {
            let _ = writeln!(std::io::stderr(), "Panic: {info}");
        }
        std::process::exit(1);
    }));
}

// ── Output format resolution ────────────────────────────────────────────

/// `HYPERLIQUID_FORMAT`, when set to a non-empty value.
fn env_output_format(source: &EnvSource) -> Option<Result<output::OutputFormat, errors::CliError>> {
    let raw = source.get(env::FORMAT)?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(parse_env_output_format(&raw))
}

/// `--json` or `--format` as typed on the command line. `--json` wins; a
/// default or an environment variable is not a request. Reads a partial parse
/// too, so a failed parse resolves its format like a successful one.
pub(super) fn requested_format(matches: &ArgMatches) -> Option<output::OutputFormat> {
    if matches.get_one::<bool>("json").copied().unwrap_or(false) {
        return Some(output::OutputFormat::Json);
    }
    if matches.value_source("format") == Some(ValueSource::CommandLine) {
        return matches.get_one::<output::OutputFormat>("format").copied();
    }
    None
}

/// The one precedence rule for the output format: what the command line
/// requested, then `HYPERLIQUID_FORMAT`, then JSON for agents and non-terminal
/// stdout, then the default. An unreadable `HYPERLIQUID_FORMAT` is the only error.
fn resolve_output_format(
    requested: Option<output::OutputFormat>,
    probed: &output::terminal::OutputEnv,
    source: &EnvSource,
) -> Result<output::OutputFormat, errors::CliError> {
    if let Some(format) = requested {
        return Ok(format);
    }
    if let Some(format) = env_output_format(source) {
        return format;
    }
    if probed.agent || !probed.stdout_tty {
        return Ok(output::OutputFormat::Json);
    }
    Ok(output::OutputFormat::default())
}

/// What the environment decides before any command runs, and the one order it
/// is read in: the output format first, because it names the format every later
/// error is reported in, then the strict boolean variables, which are reported
/// in that format. An unreadable `HYPERLIQUID_FORMAT` cannot be reported in the
/// format it names, so it is reported as JSON. The error comes with the format
/// to report it in.
pub(super) fn startup(
    requested: Option<output::OutputFormat>,
    source: &EnvSource,
) -> Result<
    (output::terminal::OutputEnv, output::OutputFormat),
    (errors::CliError, output::OutputFormat),
> {
    let probed = output::terminal::OutputEnv::probe(requested.is_some(), source);
    let format = resolve_output_format(requested, &probed, source)
        .map_err(|err| (err, output::OutputFormat::Json))?;
    hyperliquid_cli::mode::validate_env(source).map_err(|err| (err, format))?;
    Ok((probed, format))
}

fn parse_env_output_format(raw: &str) -> Result<output::OutputFormat, errors::CliError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pretty" => Ok(output::OutputFormat::Pretty),
        "table" => Ok(output::OutputFormat::Table),
        "json" => Ok(output::OutputFormat::Json),
        other => Err(errors::CliError::Configuration(format!(
            "{} must be one of pretty, table, or json; got '{other}'",
            env::FORMAT
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperliquid_cli::command_registry::CommandRegistry;

    /// Split an example line into words, honoring single and double quotes.
    fn split_command_line(line: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut current = String::new();
        let mut quote: Option<char> = None;
        let mut started = false;
        for ch in line.chars() {
            match (quote, ch) {
                (Some(open), c) if c == open => quote = None,
                (Some(_), c) => current.push(c),
                (None, '\'' | '"') => {
                    quote = Some(ch);
                    started = true;
                }
                (None, c) if c.is_whitespace() => {
                    if started || !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                (None, c) => current.push(c),
            }
        }
        if started || !current.is_empty() {
            words.push(current);
        }
        words
    }

    fn collect_examples(command: &clap::Command, out: &mut Vec<String>) {
        for text in [
            command.get_about(),
            command.get_long_about(),
            command.get_after_help(),
            command.get_after_long_help(),
        ]
        .into_iter()
        .flatten()
        {
            let text = text.to_string();
            let mut in_examples = false;
            for line in text.lines() {
                let line = line.trim();
                if line == "Examples:" {
                    in_examples = true;
                } else if in_examples {
                    if line.starts_with("hyperliquid ") {
                        out.push(line.to_string());
                    } else {
                        in_examples = false;
                    }
                }
            }
        }
        for sub in command.get_subcommands() {
            collect_examples(sub, out);
        }
    }

    #[test]
    fn every_help_example_parses() {
        let mut examples = Vec::new();
        collect_examples(&build_command(), &mut examples);
        assert!(
            examples.len() >= 25,
            "expected help examples, found {}",
            examples.len()
        );
        for example in examples {
            let words = split_command_line(&example);
            if let Err(err) = try_parse_cli_from(&words) {
                panic!("help example doesn't parse: {example}\n{err}");
            }
        }
    }

    fn visit_commands(
        command: &clap::Command,
        path: &mut Vec<String>,
        f: &mut dyn FnMut(&clap::Command, &[String]),
    ) {
        f(command, path);
        for sub in command.get_subcommands() {
            path.push(sub.get_name().to_string());
            visit_commands(sub, path, f);
            path.pop();
        }
    }

    #[test]
    fn every_state_changing_command_documents_its_risk() {
        let root = build_command();
        let mut checked = 0;
        visit_commands(&root, &mut Vec::new(), &mut |command, path| {
            let Some(entry) = commands::entries()
                .iter()
                .find(|entry| entry.spec.path.iter().eq(path.iter()))
            else {
                return;
            };
            if !entry.spec.changes_state() {
                return;
            }
            checked += 1;
            let help = command
                .get_after_long_help()
                .map(|help| help.to_string())
                .unwrap_or_default();
            assert!(
                help.contains("Notes:") && help.contains("Risk:") && help.contains("Dry-run:"),
                "{} is missing notes",
                path.join(" ")
            );
        });
        assert!(
            checked >= 40,
            "only {checked} state-changing commands checked"
        );
    }

    #[test]
    fn orders_create_notes_state_that_testnet_does_not_prompt() {
        let root = build_command();
        let create = find_command(&root, &["orders".to_string(), "create".to_string()]);
        let help = create.get_after_long_help().unwrap().to_string();
        assert!(
            help.contains(
                "on testnet (--testnet or HYPERLIQUID_NETWORK=testnet) this command doesn't prompt"
            ),
            "{help}"
        );
        assert!(help.contains("Pass --yes"), "{help}");
    }

    #[test]
    fn commands_that_only_print_or_serve_carry_no_state_notes() {
        let root = build_command();
        for path in [["feedback"], ["mcp"], ["report-incident"]] {
            let command = find_command(&root, &path.map(str::to_string));
            let help = command
                .get_after_long_help()
                .map(|help| help.to_string())
                .unwrap_or_default();
            assert!(!help.contains("Risk:"), "{path:?}: {help}");
        }
    }

    #[test]
    fn every_option_has_help_text() {
        let root = build_command();
        let mut missing = Vec::new();
        visit_commands(&root, &mut Vec::new(), &mut |command, path| {
            for arg in command.get_arguments() {
                if arg.is_hide_set() {
                    continue;
                }
                if arg.get_help().is_none() && arg.get_long_help().is_none() {
                    missing.push(format!("{} {}", path.join(" "), arg.get_id()));
                }
            }
        });
        assert!(missing.is_empty(), "options without help: {missing:?}");
    }

    #[test]
    fn requested_format_is_read_from_parsed_matches() {
        for (argv, requested) in [
            (
                &["hyperliquid", "-fjson", "status"][..],
                Some(output::OutputFormat::Json),
            ),
            (
                &["hyperliquid", "-ftable", "status"][..],
                Some(output::OutputFormat::Table),
            ),
            (
                &["hyperliquid", "status", "--format=pretty"][..],
                Some(output::OutputFormat::Pretty),
            ),
            (
                &["hyperliquid", "--json", "--format=table", "status"][..],
                Some(output::OutputFormat::Json),
            ),
            (&["hyperliquid", "status"][..], None),
        ] {
            let parsed = try_parse_cli_from(argv.iter().copied()).unwrap();
            assert_eq!(parsed.requested_format, requested, "{argv:?}");
        }
    }

    #[test]
    fn requested_format_wins_over_the_environment_facts() {
        let piped_agent = output::terminal::OutputEnv {
            agent: true,
            ..output::terminal::OutputEnv::default()
        };
        assert_eq!(
            resolve_output_format(
                Some(output::OutputFormat::Table),
                &piped_agent,
                &EnvSource::fixed([(env::FORMAT, "json")])
            )
            .unwrap(),
            output::OutputFormat::Table
        );
    }

    #[test]
    fn startup_reports_an_unreadable_format_variable_as_json() {
        let source = EnvSource::fixed([(env::FORMAT, "yaml"), (env::AGENT, "banana")]);
        let (err, format) = startup(None, &source).unwrap_err();
        assert!(err.to_string().contains(env::FORMAT), "{err}");
        assert_eq!(format, output::OutputFormat::Json);
    }

    #[test]
    fn startup_reports_an_unreadable_mode_variable_in_the_resolved_format() {
        for name in [env::AGENT, env::NO_UPDATE_CHECK] {
            let source = EnvSource::fixed([(name, "banana")]);
            let (err, format) = startup(Some(output::OutputFormat::Table), &source).unwrap_err();
            assert!(err.to_string().contains(name), "{err}");
            assert_eq!(format, output::OutputFormat::Table);
        }
    }

    #[test]
    fn startup_resolves_the_format_from_the_environment_after_the_command_line() {
        let source = EnvSource::fixed([(env::FORMAT, "table")]);
        let (_, format) = startup(None, &source).unwrap();
        assert_eq!(format, output::OutputFormat::Table);
        let (_, format) = startup(Some(output::OutputFormat::Pretty), &source).unwrap();
        assert_eq!(format, output::OutputFormat::Pretty);
    }

    #[test]
    fn failed_parse_still_reads_explicit_format() {
        let source = &EnvSource::fixed::<&str, &str>([]);
        let args = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
        let lenient = lenient_matches(&args(&["hyperliquid", "--format=pretty", "--bogus"]));
        assert_eq!(
            error_format(lenient.as_ref(), source),
            output::OutputFormat::Pretty
        );
        let lenient = lenient_matches(&args(&["hyperliquid", "-f=table", "--bogus"]));
        assert_eq!(
            error_format(lenient.as_ref(), source),
            output::OutputFormat::Table
        );
        let lenient = lenient_matches(&args(&["hyperliquid", "-fjson", "--bogus"]));
        assert_eq!(
            error_format(lenient.as_ref(), source),
            output::OutputFormat::Json
        );
        let lenient = lenient_matches(&args(&["hyperliquid", "--format=json", "--bogus"]));
        assert_eq!(
            error_format(lenient.as_ref(), source),
            output::OutputFormat::Json
        );
    }

    #[test]
    fn lenient_parse_finds_the_command_that_was_reached() {
        let args = ["hyperliquid", "orders", "crete"].map(OsString::from);
        let lenient = lenient_matches(&args).unwrap();
        assert_eq!(
            command_path_from_matches(&lenient),
            Some(vec!["orders".to_string()])
        );
    }

    #[test]
    fn unknown_subcommand_shows_one_suggestion_and_its_group_help() {
        let err = try_parse_cli_from(["hyperliquid", "--no-update-check", "orders", "cancl"])
            .err()
            .unwrap();
        let text = usage_error_text(err, &["orders".to_string()], false);
        assert_eq!(text.matches("tip:").count(), 1, "{text}");
        assert!(text.contains("'cancel'"), "{text}");
        assert!(!text.contains("--no-update-check"), "{text}");
        assert!(text.contains("Usage: hyperliquid orders"), "{text}");
        assert!(
            text.trim_end()
                .ends_with("See all options:\n    hyperliquid orders --help"),
            "{text}"
        );
        assert!(!text.contains("For more information"), "{text}");
    }

    #[test]
    fn usage_line_shows_the_command_path_not_consumed_globals() {
        let err =
            try_parse_cli_from(["hyperliquid", "--no-update-check", "--form", "json", "mids"])
                .err()
                .unwrap();
        let text = usage_error_text(err, &[], false);
        assert!(!text.contains("--no-update-check"), "{text}");
        assert!(text.contains("Usage: hyperliquid [OPTIONS]"), "{text}");
        assert!(text.trim_end().ends_with("hyperliquid --help"), "{text}");
    }

    #[test]
    fn short_and_long_help_are_identical() {
        let render = |flag: &str| {
            try_parse_cli_from(["hyperliquid", "orders", "create", flag])
                .err()
                .unwrap()
                .render()
                .to_string()
        };
        assert_eq!(render("-h"), render("--help"));
    }

    #[test]
    fn version_flags_work_at_every_depth() {
        use clap::error::ErrorKind;
        for argv in [
            &["hyperliquid", "-v"][..],
            &["hyperliquid", "-V"][..],
            &["hyperliquid", "--version"][..],
            &["hyperliquid", "orders", "--version"][..],
            &["hyperliquid", "orders", "create", "--version"][..],
        ] {
            let err = try_parse_cli_from(argv.iter().copied()).err().unwrap();
            assert_eq!(err.kind(), ErrorKind::DisplayVersion, "{argv:?}");
        }
    }

    #[test]
    fn group_without_action_asks_for_help() {
        use clap::error::ErrorKind;
        let err = try_parse_cli_from(["hyperliquid", "orders"]).err().unwrap();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn list_has_ls_alias_and_ls_has_list_alias() {
        for argv in [
            &["hyperliquid", "perps", "ls"][..],
            &["hyperliquid", "positions", "ls"][..],
            &["hyperliquid", "account", "list"][..],
            &["hyperliquid", "account", "ls"][..],
            &["hyperliquid", "order", "open"][..],
            &["hyperliquid", "position", "list"][..],
            &["hyperliquid", "perp", "list"][..],
            &["hyperliquid", "outcome", "list"][..],
            &["hyperliquid", "wallets", "list"][..],
            &["hyperliquid", "accounts", "ls"][..],
        ] {
            try_parse_cli_from(argv.iter().copied())
                .unwrap_or_else(|err| panic!("{argv:?} should parse: {err}"));
        }
    }

    #[test]
    fn broken_pipe_panics_are_recognized() {
        assert!(is_broken_pipe_panic(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
        assert!(is_broken_pipe_panic(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
        assert!(!is_broken_pipe_panic("index out of bounds"));
        assert!(!is_broken_pipe_panic(
            "failed printing to stdout: Interrupted"
        ));
    }

    #[test]
    fn root_help_lists_environment_and_no_hype() {
        let help = help_text_for_path(&[]);
        assert!(help.contains("Environment:"), "{help}");
        for name in [
            "HYPERLIQUID_FORMAT",
            "HYPERLIQUID_KEYSTORE_PASSWORD",
            "OWS_PASSPHRASE",
            "CI",
        ] {
            assert!(help.contains(name), "{name} missing from root help");
        }
        assert!(!help.to_lowercase().contains("production-grade"), "{help}");
        assert!(help.contains("Global Options:"), "{help}");
    }

    #[test]
    fn cli_command_paths_resolve_to_registered_commands() {
        for (argv, expected_path) in [
            (
                &["hyperliquid", "api-wallet", "list"][..],
                "api-wallet list",
            ),
            (&["hyperliquid", "orders", "history"][..], "orders history"),
            (
                &["hyperliquid", "orders", "cancel", "123"][..],
                "orders cancel",
            ),
            (&["hyperliquid", "wallet", "show"][..], "wallet show"),
            (
                &["hyperliquid", "report-incident", "--command", "mids"][..],
                "report-incident",
            ),
            (
                &["hyperliquid", "subscribe", "fills"][..],
                "subscribe fills",
            ),
            (
                &["hyperliquid", "account", "abstraction"][..],
                "account abstraction",
            ),
            (
                &[
                    "hyperliquid",
                    "account",
                    "abstraction",
                    "set",
                    "--mode",
                    "disabled",
                ][..],
                "account abstraction set",
            ),
        ] {
            let parsed = try_parse_cli_from(argv.iter().copied()).unwrap();
            let selected = parsed
                .selected
                .unwrap_or_else(|| panic!("no command selected for {argv:?}"));
            assert_eq!(selected.entry.spec.key(), expected_path);
            assert!(
                CommandRegistry::get()
                    .find_path(&expected_path.split(' ').collect::<Vec<_>>())
                    .is_some(),
                "{expected_path} is not in the registry"
            );
        }
    }

    #[test]
    fn dry_run_and_api_policy_come_from_the_spec() {
        let spec = |argv: &[&str]| {
            try_parse_cli_from(argv.iter().copied())
                .unwrap()
                .selected
                .unwrap()
                .entry
                .spec
        };
        let api_wallet_list = spec(&["hyperliquid", "api-wallet", "list"]);
        assert_ne!(api_wallet_list.dry_run, DryRunPolicy::Optional);
        assert!(api_wallet_list.uses_api());

        let orders_cancel = spec(&["hyperliquid", "orders", "cancel", "123"]);
        assert_eq!(orders_cancel.dry_run, DryRunPolicy::Optional);
        assert!(orders_cancel.uses_api());

        let wallet_show = spec(&["hyperliquid", "wallet", "show"]);
        assert_ne!(wallet_show.dry_run, DryRunPolicy::Optional);
        assert!(!wallet_show.uses_api());
    }
}
