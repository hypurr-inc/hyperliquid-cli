//! Command-line shell around the clap tree: parsing, help and usage-error
//! rendering, panic handling, and output-format resolution.
//!
//! `main.rs` declares the global options and boots the process; the commands
//! come from `commands::entries`; everything that decides how the command line
//! is presented to people and agents lives here.

use std::ffi::OsString;
use std::sync::OnceLock;

use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches};
use hyperliquid_cli::auth::SignerFlags;
use hyperliquid_cli::cli_command::Entry;
use hyperliquid_cli::command_spec::{
    CommandSpec, ConfirmationPolicy, DryRunPolicy, Risk, YES_ARG, YES_FLAG, supplied_secrets,
};
use hyperliquid_cli::commands;
use hyperliquid_cli::env::{self, EnvSource};
use hyperliquid_cli::errors;
use hyperliquid_cli::mode::Mode;
use hyperliquid_cli::output::{self, sink};
use hyperliquid_cli::terminal::{OutputFormat, Terminal};

use crate::{Cli, GLOBAL_OPTIONS};

pub(super) struct Selected {
    pub(super) entry: &'static Entry,
    pub(super) matches: ArgMatches,
    pub(super) bound: Box<dyn hyperliquid_cli::cli_command::Bound>,
    /// The command line after the program name, as typed.
    pub(super) args: Vec<String>,
    pub(super) redactions: Redactions,
}

/// The secret inputs of one run: what a suggested command must not repeat.
/// They come from the global secret options and from the secret args the
/// command's spec declares, never from a second list.
#[derive(Debug, Default)]
pub(super) struct Redactions {
    /// Long option names (`--private-key`) whose values are secrets.
    pub(super) options: Vec<String>,
    /// Secret positional values typed on the command line.
    pub(super) values: Vec<String>,
}

impl Redactions {
    /// Read from the clap tree the line was parsed with: `root` for the global
    /// options, `leaf` (the selected command) for the spec's secret args.
    fn of(
        spec: &CommandSpec,
        matches: &ArgMatches,
        root: &clap::Command,
        leaf: &clap::Command,
    ) -> Self {
        let mut redactions = Self::default();
        redactions.options.extend(
            root.get_arguments()
                .filter(|arg| SignerFlags::SECRET.contains(&arg.get_id().as_str()))
                .filter_map(|arg| arg.get_long())
                .map(|long| format!("--{long}")),
        );
        for secret in supplied_secrets(spec, matches) {
            let long = leaf
                .get_arguments()
                .find(|arg| arg.get_id() == secret.id)
                .and_then(|arg| arg.get_long());
            match long {
                Some(long) => redactions.options.push(format!("--{long}")),
                None => redactions.values.extend(
                    matches
                        .get_raw(secret.id)
                        .into_iter()
                        .flatten()
                        .map(|value| value.to_string_lossy().into_owned()),
                ),
            }
        }
        redactions
    }
}

pub(super) struct ParsedCli {
    pub(super) cli: Cli,
    /// `None` when the line names no command (help is shown).
    pub(super) selected: Option<Selected>,
    /// `--json` or `--format` as typed on the command line; env and defaults are not.
    pub(super) requested_format: Option<OutputFormat>,
}

pub(super) fn try_parse_cli_from<I, T>(args: I) -> Result<ParsedCli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let raw: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let mut command = build_command();
    let matches = command.try_get_matches_from_mut(&raw)?;
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
                args: raw
                    .iter()
                    .skip(1)
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
                redactions: Redactions::of(
                    entry.spec,
                    leaf,
                    &command,
                    find_command(&command, &path),
                ),
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
    let blockers = Mode::switchable_blockers();
    lines.push(match (spec.confirmation, has_yes) {
        (ConfirmationPolicy::Prompt, true) => format!("  Confirmation: asks before it runs. Pass {YES_FLAG} to answer the prompt. Runs that can't prompt ({blockers}) fail without {YES_FLAG}. Use {YES_FLAG} there only after a human approves."),
        (ConfirmationPolicy::Prompt, false) => format!("  Confirmation: asks before it runs. Runs that can't prompt ({blockers}) fail instead."),
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

fn styled_text(styled: &clap::builder::StyledStr, color: bool) -> String {
    if color {
        styled.ansi().to_string()
    } else {
        styled.to_string()
    }
}

fn exit_after_stdout(text: &str) -> ! {
    sink::out(text);
    std::process::exit(0)
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

pub(super) fn print_help_to_stdout(path: &[String]) -> Result<(), anyhow::Error> {
    sink::try_out(&help_text_for_path(path))
        .map_err(|err| anyhow::anyhow!("failed to write help to stdout: {err}"))
}

fn help_command(path: &[String]) -> String {
    if path.is_empty() {
        "hyperliquid --help".to_string()
    } else {
        format!("hyperliquid {} --help", path.join(" "))
    }
}

/// The tokens that make clap print help or the version and stop, read from
/// the arguments `command` defines for them rather than listed a second time.
fn help_or_version_tokens(command: &clap::Command) -> Vec<String> {
    use clap::ArgAction;

    command
        .get_arguments()
        .filter(|arg| {
            matches!(
                arg.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
        })
        .flat_map(|arg| {
            let longs = arg.get_long().map(|long| format!("--{long}"));
            let shorts = arg
                .get_short()
                .into_iter()
                .chain(arg.get_all_short_aliases().into_iter().flatten())
                .map(|short| format!("-{short}"));
            longs.into_iter().chain(shorts).collect::<Vec<_>>()
        })
        .collect()
}

/// Parse as much of the command line as possible, skipping what clap rejects.
///
/// Used only after a real parse failed, to learn which command was reached and
/// which global options were given, so errors can be rendered in the right
/// format and point at the right help page.
fn lenient_matches(raw_args: &[OsString]) -> Option<ArgMatches> {
    let mut command = build_command().ignore_errors(true);
    let help_or_version = help_or_version_tokens(&command);
    let mut args: Vec<OsString> = raw_args
        .iter()
        .filter(|arg| {
            !arg.to_str()
                .is_some_and(|arg| help_or_version.iter().any(|token| token == arg))
        })
        .cloned()
        .collect();
    loop {
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

/// Output format for a failed parse, from the `--json` or `--format` it still
/// showed.
fn error_format(
    matches: Option<&ArgMatches>,
    terminal: Terminal,
    source: &EnvSource,
) -> OutputFormat {
    Mode::error_format(matches.and_then(requested_format), terminal, source)
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

/// One suggestion, and the usage line of the command that was actually reached.
fn prepare_usage_error(mut err: clap::Error, path: &[String]) -> clap::Error {
    use clap::error::{ContextKind, ContextValue};

    narrow_suggestions(&mut err);
    let mut root = build_command();
    root.build();
    if err.get(ContextKind::Usage).is_some() {
        let usage = find_command(&root, path).clone().render_usage();
        err.insert(ContextKind::Usage, ContextValue::StyledStr(usage));
    }
    err
}

/// Render a usage error: one suggestion, the usage line of the command that
/// was actually reached, and a pointer to its help.
fn usage_error_text(err: clap::Error, path: &[String], color: bool) -> String {
    let err = prepare_usage_error(err, path);
    let body = clap_message(&err, color);
    format!("{body}\n\nSee all options:\n    {}\n", help_command(path))
}

/// What clap reported, without its trailing "For more information" pointer:
/// this shell ends every usage error with its own pointer to the right help
/// page. The one place clap's rendered text is cut.
fn clap_message(err: &clap::Error, color: bool) -> String {
    let text = styled_text(&err.render(), color);
    match text.rfind("\n\nFor more information") {
        Some(index) => text[..index].to_string(),
        None => text.trim_end().to_string(),
    }
}

/// The same usage error for the JSON envelope, read from what clap reported:
/// its kind and context say whether the command was unknown and what it
/// suggests; the usage line is the one this shell put there.
fn usage_error_parts(err: clap::Error, path: &[String]) -> errors::UsageParts {
    use clap::error::{ContextKind, ContextValue, ErrorKind};

    let mut err = prepare_usage_error(err, path);
    let usage = match err.remove(ContextKind::Usage) {
        Some(ContextValue::StyledStr(usage)) => Some(usage.to_string().trim().to_string()),
        _ => None,
    };
    let suggestion = [
        ContextKind::SuggestedSubcommand,
        ContextKind::SuggestedArg,
        ContextKind::SuggestedValue,
    ]
    .into_iter()
    .find_map(|kind| match err.get(kind) {
        Some(ContextValue::Strings(values)) => values.first().cloned(),
        Some(ContextValue::String(value)) => Some(value.clone()),
        _ => None,
    });
    let message = clap_message(&err, false);
    let message = message.trim();
    let message = message.strip_prefix("error: ").unwrap_or(message);
    errors::UsageParts {
        headline: message
            .split("\n\n")
            .map(str::trim)
            .filter(|paragraph| !paragraph.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        usage,
        suggestion,
        help_command: help_command(path),
        unknown_command: err.kind() == ErrorKind::InvalidSubcommand,
    }
}

/// Parse the command line, or print what clap reported and exit. This is the
/// one place the process's color decision is made: from `--no-color` as parsed,
/// or as far as a failed parse got.
pub(super) fn parse_command_line(
    raw_args: &[OsString],
    terminal: Terminal,
    source: &EnvSource,
) -> ParsedCli {
    let parsed = try_parse_cli_from(raw_args);
    let lenient = parsed
        .as_ref()
        .err()
        .and_then(|_| lenient_matches(raw_args));
    let no_color = match &parsed {
        Ok(parsed) => parsed.cli.no_color,
        // A partial parse may not have applied flag defaults, so read the flag fallibly.
        Err(_) => lenient.as_ref().is_some_and(|matches| {
            matches
                .try_get_one::<bool>("no_color")
                .ok()
                .flatten()
                .copied()
                .unwrap_or(false)
        }),
    };
    output::colors::init(no_color, source, terminal);
    match parsed {
        Ok(parsed) => parsed,
        Err(err) => exit_for_clap_error(err, lenient.as_ref(), terminal, source),
    }
}

/// Print what clap reported and exit. Help and version are successes on
/// stdout; everything else is a usage error (exit 2).
fn exit_for_clap_error(
    err: clap::Error,
    lenient: Option<&ArgMatches>,
    terminal: Terminal,
    source: &EnvSource,
) -> ! {
    use clap::error::ErrorKind;

    let path = lenient
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
            let text = if error_format(lenient, terminal, source) == OutputFormat::Json {
                format!("{}\n", serde_json::json!({ "version": version }))
            } else {
                format!("hyperliquid {version}\n")
            };
            exit_after_stdout(&text)
        }
        _ => {
            let exit_code = err.exit_code();
            if error_format(lenient, terminal, source) == OutputFormat::Json {
                exit_after_stdout_with_code(
                    &format!(
                        "{}\n",
                        errors::ErrorEnvelope::usage(usage_error_parts(err, &path)).to_json()
                    ),
                    exit_code,
                );
            }
            sink::err(&usage_error_text(
                err,
                &path,
                output::colors::stderr_enabled(),
            ));
            std::process::exit(exit_code)
        }
    }
}

fn exit_after_stdout_with_code(text: &str, code: i32) -> ! {
    let _ = sink::try_out(text);
    std::process::exit(code)
}

/// The command the line selected, for the panic message: a panic hook takes no
/// arguments, so this is the one value it reads.
static PANIC_COMMAND: OnceLock<String> = OnceLock::new();

/// Make a later panic name `command` (a space-joined path such as `orders create`).
pub(super) fn name_command_in_panics(command: &str) {
    let _ = PANIC_COMMAND.set(command.to_string());
}

/// A panic is a bug: say so with the command path (never argument values) and
/// exit 1. A closed pipe is not one, because nothing prints but through
/// [`output::sink`].
pub(super) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let command = match PANIC_COMMAND.get() {
            Some(path) => format!("hyperliquid {path}"),
            None => "hyperliquid".to_string(),
        };
        sink::err(&format!(
            "Unexpected error in {command}. This is a bug in hyperliquid-cli.\nVersion: {}\nReport it at:\n    https://github.com/{}/issues/new\n",
            env!("CARGO_PKG_VERSION"),
            hyperliquid_cli::update::REPO,
        ));
        // Developers opt in to the panic message and location with RUST_BACKTRACE.
        if EnvSource::Process
            .get(env::RUST_BACKTRACE)
            .is_some_and(|value| !value.is_empty() && value != "0")
        {
            sink::err_line(&format!("Panic: {info}"));
        }
        std::process::exit(1);
    }));
}

/// `--json` or `--format` as typed on the command line. `--json` wins; a
/// default or an environment variable is not a request. Reads a partial parse
/// too, so a failed parse resolves its format like a successful one.
pub(super) fn requested_format(matches: &ArgMatches) -> Option<OutputFormat> {
    if matches.get_one::<bool>("json").copied().unwrap_or(false) {
        return Some(OutputFormat::Json);
    }
    if matches.value_source("format") == Some(ValueSource::CommandLine) {
        return matches.get_one::<OutputFormat>("format").copied();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn read_only_local_commands_do_not_claim_to_change_state() {
        for path in [
            ["account", "ls"],
            ["wallet", "show"],
            ["wallet", "address"],
            ["wallet", "list"],
        ] {
            let path = path.map(String::from);
            let help = help_text_for_path(&path);
            assert!(
                !help.contains("Notes:") && !help.contains("runs as soon as you invoke it"),
                "{} is read-only but its help warns it changes state:\n{help}",
                path.join(" ")
            );
        }
    }

    #[test]
    fn the_secret_flags_are_the_global_options_that_warn_about_process_lists() {
        let root = build_command();
        let mut warned: Vec<&str> = root
            .get_arguments()
            .filter(|arg| {
                arg.get_help()
                    .is_some_and(|help| help.to_string().contains("appear in process lists"))
            })
            .map(|arg| arg.get_id().as_str())
            .collect();
        warned.sort_unstable();
        let mut secret = SignerFlags::SECRET.to_vec();
        secret.sort_unstable();
        assert_eq!(warned, secret);
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
                Some(OutputFormat::Json),
            ),
            (
                &["hyperliquid", "-ftable", "status"][..],
                Some(OutputFormat::Table),
            ),
            (
                &["hyperliquid", "status", "--format=pretty"][..],
                Some(OutputFormat::Pretty),
            ),
            (
                &["hyperliquid", "--json", "--format=table", "status"][..],
                Some(OutputFormat::Json),
            ),
            (&["hyperliquid", "status"][..], None),
        ] {
            let parsed = try_parse_cli_from(argv.iter().copied()).unwrap();
            assert_eq!(parsed.requested_format, requested, "{argv:?}");
        }
    }

    fn quiet() -> (Terminal, EnvSource) {
        (Terminal::default(), EnvSource::fixed::<&str, &str>([]))
    }

    #[test]
    fn failed_parse_still_reads_explicit_format() {
        let (terminal, source) = quiet();
        let args = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
        for (argv, expected) in [
            (
                ["hyperliquid", "--format=pretty", "--bogus"],
                OutputFormat::Pretty,
            ),
            (["hyperliquid", "-f=table", "--bogus"], OutputFormat::Table),
            (["hyperliquid", "-fjson", "--bogus"], OutputFormat::Json),
            (
                ["hyperliquid", "--format=json", "--bogus"],
                OutputFormat::Json,
            ),
        ] {
            let lenient = lenient_matches(&args(&argv));
            assert_eq!(
                error_format(lenient.as_ref(), terminal, &source),
                expected,
                "{argv:?}"
            );
        }
    }

    #[test]
    fn the_lenient_parse_survives_any_number_of_rejected_tokens() {
        let mut argv = vec!["hyperliquid".to_string(), "--format=table".to_string()];
        argv.extend((0..20).map(|n| format!("--bogus{n}")));
        let args: Vec<OsString> = argv.iter().map(OsString::from).collect();
        let lenient = lenient_matches(&args).expect("every rejected token is skipped");
        assert_eq!(requested_format(&lenient), Some(OutputFormat::Table));
    }

    #[test]
    fn the_usage_envelope_matches_the_pre_refactor_binary_byte_for_byte() {
        let cases: &[(&str, &[&str], &[&str], &str)] = &[
            (
                "unknown flag, no suggestion",
                &["hyperliquid", "wallet", "show", "--bogus"],
                &["wallet", "show"],
                r##"{"error":"unexpected argument '--bogus' found","category":"usage","exit_code":2,"reason":"invalid_arguments","next":[{"command":"hyperliquid wallet show --help"}],"data":{"usage":"Usage: hyperliquid wallet show [OPTIONS]"}}"##,
            ),
            (
                "invalid enum value with suggestion",
                &[
                    "hyperliquid",
                    "account",
                    "abstraction",
                    "set",
                    "--mode",
                    "unified",
                ],
                &["account", "abstraction", "set"],
                r##"{"error":"invalid value 'unified' for '--mode <MODE>'\n  [possible values: disabled, unified-account, portfolio-margin]\ntip: a similar value exists: 'unified-account'","category":"usage","exit_code":2,"reason":"invalid_arguments","next":[{"command":"hyperliquid account abstraction set --help"}],"data":{"suggestion":"unified-account"}}"##,
            ),
            (
                "unknown flag with suggestion",
                &[
                    "hyperliquid",
                    "orders",
                    "scale",
                    "--order-count",
                    "1",
                    "--coin",
                    "BTC",
                ],
                &["orders", "scale"],
                r##"{"error":"unexpected argument '--order-count' found\ntip: a similar argument exists: '--orders'","category":"usage","exit_code":2,"reason":"invalid_arguments","next":[{"command":"hyperliquid orders scale --help"}],"data":{"suggestion":"--orders","usage":"Usage: hyperliquid orders scale [OPTIONS] --coin <COIN> --side <SIDE> --start-price <START_PRICE> --end-price <END_PRICE> --total-size <TOTAL_SIZE> --orders <ORDER_COUNT>"}}"##,
            ),
            (
                "unknown subcommand with suggestion",
                &["hyperliquid", "orders", "cancl"],
                &["orders"],
                r##"{"error":"unrecognized subcommand 'cancl'\ntip: a similar subcommand exists: 'cancel'","category":"usage","exit_code":2,"reason":"unknown_command","next":[{"command":"hyperliquid orders --help"}],"data":{"suggestion":"cancel","usage":"Usage: hyperliquid orders [OPTIONS] <COMMAND>"}}"##,
            ),
            (
                "flag on a bare group",
                &["hyperliquid", "orders", "--bogus"],
                &["orders"],
                r##"{"error":"unexpected argument '--bogus' found","category":"usage","exit_code":2,"reason":"invalid_arguments","next":[{"command":"hyperliquid orders --help"}],"data":{"usage":"Usage: hyperliquid orders [OPTIONS] <COMMAND>"}}"##,
            ),
            (
                "missing required argument",
                &["hyperliquid", "perps", "get"],
                &["perps", "get"],
                r##"{"error":"the following required arguments were not provided:\n  <COIN>","category":"usage","exit_code":2,"reason":"invalid_arguments","next":[{"command":"hyperliquid perps get --help"}],"data":{"usage":"Usage: hyperliquid perps get [OPTIONS] <COIN>"}}"##,
            ),
            (
                "too many values",
                &["hyperliquid", "perps", "get", "BTC", "extra"],
                &["perps", "get"],
                r##"{"error":"unexpected argument 'extra' found","category":"usage","exit_code":2,"reason":"invalid_arguments","next":[{"command":"hyperliquid perps get --help"}],"data":{"usage":"Usage: hyperliquid perps get [OPTIONS] <COIN>"}}"##,
            ),
            (
                "value validation failure",
                &["hyperliquid", "--max-results", "0", "perps", "list"],
                &["perps", "list"],
                r##"{"error":"invalid value '0' for '--max-results <N>': value must be at least 1","category":"usage","exit_code":2,"reason":"invalid_arguments","next":[{"command":"hyperliquid perps list --help"}]}"##,
            ),
        ];
        for (name, argv, path, expected) in cases {
            let err = try_parse_cli_from(argv.iter().copied()).err().unwrap();
            let path: Vec<String> = path.iter().map(|part| part.to_string()).collect();
            let json = errors::ErrorEnvelope::usage(usage_error_parts(err, &path)).to_json();
            assert_eq!(&json, expected, "{name}");
        }
    }

    #[test]
    fn the_usage_envelope_comes_from_clap_context() {
        let parts = |argv: &[&str], path: &[&str]| {
            let err = try_parse_cli_from(argv.iter().copied()).err().unwrap();
            usage_error_parts(err, &path.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };

        let unknown = parts(&["hyperliquid", "orders", "cancl"], &["orders"]);
        assert!(unknown.unknown_command);
        assert_eq!(unknown.suggestion.as_deref(), Some("cancel"));
        assert_eq!(unknown.help_command, "hyperliquid orders --help");
        assert!(
            unknown
                .headline
                .starts_with("unrecognized subcommand 'cancl'"),
            "{}",
            unknown.headline
        );
        assert!(!unknown.headline.contains("Usage:"), "{}", unknown.headline);
        assert!(
            unknown
                .usage
                .as_deref()
                .is_some_and(|u| u.starts_with("Usage: hyperliquid orders"))
        );

        let bad_flag = parts(
            &["hyperliquid", "wallet", "show", "--bogus"],
            &["wallet", "show"],
        );
        assert!(!bad_flag.unknown_command);
        assert!(
            bad_flag
                .headline
                .starts_with("unexpected argument '--bogus' found"),
            "{}",
            bad_flag.headline
        );
        assert_eq!(
            bad_flag.usage.as_deref(),
            Some("Usage: hyperliquid wallet show [OPTIONS]")
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
    fn root_help_lists_environment_and_global_options() {
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
        assert!(help.contains("Global Options:"), "{help}");
    }

    #[test]
    fn argv_selects_the_command_with_that_path() {
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
        }
    }

    #[test]
    fn the_help_and_version_tokens_are_the_flags_the_command_defines() {
        let mut tokens = help_or_version_tokens(&build_command());
        tokens.sort();
        assert_eq!(tokens, ["--help", "--version", "-V", "-h", "-v"]);
    }

    #[test]
    fn a_failed_parse_still_reads_the_command_around_a_help_flag() {
        let raw: Vec<OsString> = ["hyperliquid", "orders", "create", "--help", "--nope"]
            .iter()
            .map(OsString::from)
            .collect();
        let matches = lenient_matches(&raw).expect("leniently parsed");
        assert_eq!(
            command_path_from_matches(&matches).unwrap(),
            ["orders", "create"]
        );
    }
}
