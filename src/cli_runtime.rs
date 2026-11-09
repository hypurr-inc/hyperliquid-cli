//! The one pipeline every command runs through: validate, bound streams, check
//! the payload, dry-run and confirmation policy, resolve the API endpoint, run.
//! What each step does for a command comes from its `CommandSpec`.

use clap::{ArgMatches, CommandFactory};
use hyperliquid_cli::command_registry::CommandRegistry;
use hyperliquid_cli::command_spec::{
    CommandSpec, ConfirmationPolicy, DryRunPolicy, RawPayloadPolicy, YES_ARG, check_args,
    supplied_secrets,
};
use hyperliquid_cli::errors::CliError;
use hyperliquid_cli::input_hardening::{validate_input_path, validate_resource_id};
use hyperliquid_cli::invocation::{Invocation, InvocationFlags};
use hyperliquid_cli::stream::require_bound;

use crate::cli_shell::Selected;
use crate::{Cli, SECRET_GLOBALS};

pub(super) async fn run(selected: Selected, inv: Invocation) -> Result<(), anyhow::Error> {
    let spec = selected.entry.spec;
    let mode = inv.mode();

    validate_global_inputs(inv.flags())?;
    check_args(spec, &selected.matches)?;
    selected.bound.validate()?;
    if mode.machine_context()
        && let Some(bound) = spec.stream
    {
        require_bound(bound, &selected.matches)?;
    }

    if inv.flags().has_payload() {
        validate_raw_payload_supported(spec)?;
    }
    validate_prompt_policy(&inv, spec, &selected.matches)?;
    if inv.dry_run() && spec.dry_run != DryRunPolicy::Optional {
        return Err(CliError::Unsupported(spec.dry_run_refusal.map_or_else(
            || "--dry-run is only supported for mutating commands with preview support".to_string(),
            str::to_string,
        ))
        .into());
    }
    if spec.local {
        return selected.bound.run(&inv).await;
    }

    inv.load_payload()?;
    if inv.payload().is_some() && !inv.dry_run() {
        return Err(CliError::Unsupported(
            "payload input currently requires --dry-run so raw payloads can be validated without side effects"
                .to_string(),
        )
        .into());
    }

    if inv.remote()?.api().has_override() && spec.uses_api() {
        inv.remote()?.api().probe_override().await?;
    }

    selected.bound.run(&inv).await
}

fn validate_global_inputs(flags: &InvocationFlags) -> Result<(), CliError> {
    if let Some(account) = flags.signer.account.as_deref() {
        validate_resource_id("account selector", account)?;
    }
    if let Some(path) = flags.payload_file.as_deref() {
        validate_input_path(path)?;
    }
    Ok(())
}

fn validate_raw_payload_supported(spec: &CommandSpec) -> Result<(), CliError> {
    if spec.raw_payload == RawPayloadPolicy::DryRunOnly {
        return Ok(());
    }
    Err(CliError::Unsupported(format!(
        "payload input is not supported for {}",
        spec.key()
    )))
}

/// What the command line said about `--yes` for the matched command.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Confirmation {
    /// The command has a `--yes` option.
    pub supported: bool,
    /// `--yes` was passed.
    pub given: bool,
}

impl Confirmation {
    /// Read `--yes` from the matched command itself, so the answer can't drift
    /// from the options the command actually defines.
    pub(crate) fn from_matches(matches: &ArgMatches) -> Self {
        match matches.try_get_one::<bool>(YES_ARG) {
            Ok(Some(given)) => Self {
                supported: true,
                given: *given,
            },
            _ => Self::default(),
        }
    }
}

fn validate_prompt_policy(
    inv: &Invocation,
    spec: &CommandSpec,
    matches: &ArgMatches,
) -> Result<(), CliError> {
    let mode = inv.mode();
    if mode.machine_context() && supplied_secrets(spec, matches).next().is_some() {
        return Err(CliError::Unsupported(
            "argv secret input is not supported in machine-readable contexts; use an interactive prompt or a future safe stdin/file secret input"
                .to_string(),
        ));
    }
    let confirmation = Confirmation::from_matches(matches);
    if inv.dry_run()
        || mode.can_prompt()
        || spec.confirmation != ConfirmationPolicy::Prompt
        || confirmation.given
        || !confirmation_applies(spec, matches)
    {
        return Ok(());
    }
    // Whether a testnet run is exempt depends on the resolved network, not on
    // which flag or variable selected it, so it is asked last.
    if spec.testnet_unprompted && !spec.prompts_on(inv.remote()?.network()) {
        return Ok(());
    }

    Err(confirmation_required_error(spec, matches, confirmation))
}

/// `account remove` with no selector has nothing to confirm yet: its run fails
/// on the missing selector (missing input) instead of prompting for one.
fn confirmation_applies(spec: &CommandSpec, matches: &ArgMatches) -> bool {
    spec.confirms_with_arg
        .is_none_or(|id| matches.try_contains_id(id).unwrap_or(false))
}

/// Name the blocked command, then give the safe next steps: a runnable
/// `--dry-run` preview when supported, and the `--yes` rerun only for use
/// after a human approves.
fn confirmation_required_error(
    spec: &CommandSpec,
    matches: &ArgMatches,
    confirmation: Confirmation,
) -> CliError {
    let args = replay_args(std::env::args().skip(1), &Redactions::of(spec, matches));
    let preview = (spec.dry_run == DryRunPolicy::Optional).then(|| {
        let mut argv = vec!["--dry-run".to_string()];
        argv.extend(args.iter().cloned());
        replay_command(&argv)
    });
    let approve = confirmation.supported.then(|| {
        let mut argv = args.clone();
        argv.push(format!("--{YES_ARG}"));
        replay_command(&argv)
    });
    CliError::ConfirmationRequired {
        headline: format!(
            "Can't run `{}` without confirmation in non-interactive mode.",
            spec.key()
        ),
        preview,
        approve,
    }
}

/// The secret inputs of one run: what a suggested command must not repeat.
/// They come from the global secret options and from the secret args the
/// command's spec declares, never from a second list.
#[derive(Debug, Default)]
struct Redactions {
    /// Long option names (`--private-key`) whose values are secrets.
    options: Vec<String>,
    /// Secret positional values typed on the command line.
    values: Vec<String>,
}

impl Redactions {
    fn of(spec: &CommandSpec, matches: &ArgMatches) -> Self {
        let mut redactions = Self::default();
        let cli = Cli::command();
        redactions.options.extend(
            cli.get_arguments()
                .filter(|arg| SECRET_GLOBALS.contains(&arg.get_id().as_str()))
                .filter_map(|arg| arg.get_long())
                .map(|long| format!("--{long}")),
        );
        let contract = CommandRegistry::get().find_path(spec.path);
        for secret in supplied_secrets(spec, matches) {
            let long = contract
                .and_then(|contract| contract.input(secret.id))
                .and_then(|input| input.long.as_deref());
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

/// Invocation arguments (without the program name) minus secret options and values.
fn replay_args(args: impl IntoIterator<Item = String>, redactions: &Redactions) -> Vec<String> {
    let mut args = args.into_iter();
    let mut kept = Vec::new();
    while let Some(arg) = args.next() {
        if redactions.options.contains(&arg) {
            args.next();
            continue;
        }
        if redactions.options.iter().any(|option| {
            arg.strip_prefix(option.as_str())
                .is_some_and(|rest| rest.starts_with('='))
        }) || redactions.values.contains(&arg)
        {
            continue;
        }
        kept.push(arg);
    }
    kept
}

fn replay_command(args: &[String]) -> String {
    let mut command = String::from("hyperliquid");
    for arg in args {
        command.push(' ');
        let plain = !arg.is_empty()
            && arg
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@%+,#".contains(ch));
        if plain {
            command.push_str(arg);
        } else {
            command.push('\'');
            command.push_str(&arg.replace('\'', r"'\''"));
            command.push('\'');
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_shell::try_parse_cli_from;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn redactions_for(argv: &[&str]) -> Redactions {
        let selected = try_parse_cli_from(argv.iter().copied())
            .unwrap()
            .selected
            .unwrap();
        Redactions::of(selected.entry.spec, &selected.matches)
    }

    #[test]
    fn replay_drops_secret_options_and_quotes_for_the_shell() {
        let argv = [
            "hyperliquid",
            "--private-key",
            "0xsecret",
            "--keystore-password=hunter2",
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--cloid",
            "it's here",
        ];
        let args = replay_args(strings(&argv[1..]), &redactions_for(&argv));
        assert_eq!(
            replay_command(&args),
            r"hyperliquid orders create --coin BTC --side buy --cloid 'it'\''s here'"
        );
    }

    #[test]
    fn global_secret_ids_are_global_options() {
        let cli = Cli::command();
        for id in SECRET_GLOBALS {
            assert!(
                cli.get_arguments()
                    .any(|arg| arg.get_id() == id && arg.get_long().is_some()),
                "{id} is not a global long option"
            );
            let redacted = hyperliquid_cli::input_hardening::redact_sensitive_json(
                serde_json::json!({ *id: "value" }),
            );
            assert_eq!(
                redacted[*id], "[redacted]",
                "{id} escapes payload redaction"
            );
        }
    }

    #[test]
    fn replay_drops_secret_positionals_declared_by_the_spec() {
        let argv = [
            "hyperliquid",
            "--format",
            "pretty",
            "wallet",
            "import",
            "0xSECRETKEY",
        ];
        let redactions = redactions_for(&argv);
        assert_eq!(redactions.values, strings(&["0xSECRETKEY"]));
        let args = replay_args(strings(&argv[1..]), &redactions);
        assert_eq!(args, strings(&["--format", "pretty", "wallet", "import"]));
    }

    #[test]
    fn replay_drops_secret_options_declared_by_the_spec() {
        let argv = ["hyperliquid", "wallet", "login", "--api-key", "bk_secret"];
        let redactions = redactions_for(&argv);
        assert!(redactions.options.contains(&"--api-key".to_string()));
        let args = replay_args(strings(&argv[1..]), &redactions);
        assert_eq!(args, strings(&["wallet", "login"]));
    }

    #[test]
    fn confirmation_comes_from_the_matched_command() {
        let parse = |argv: &[&str]| {
            Confirmation::from_matches(
                &try_parse_cli_from(argv.iter().copied())
                    .unwrap()
                    .selected
                    .unwrap()
                    .matches,
            )
        };
        let with_yes = parse(&["hyperliquid", "wallet", "export", "main", "--yes"]);
        assert!(with_yes.supported && with_yes.given);
        let without = parse(&["hyperliquid", "wallet", "export", "main"]);
        assert!(without.supported && !without.given);
        let none = parse(&["hyperliquid", "wallet", "show"]);
        assert!(!none.supported && !none.given);
        let setup = parse(&["hyperliquid", "setup", "--yes"]);
        assert!(setup.supported && setup.given);
    }
}
