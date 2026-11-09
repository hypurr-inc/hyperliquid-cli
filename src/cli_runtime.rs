//! The one pipeline every command runs through: validate, bound streams, check
//! the payload, dry-run and confirmation policy, resolve the API endpoint, run.
//! What each step does for a command comes from its `CommandSpec`.

use clap::ArgMatches;
use clap::parser::MatchesError;
use hyperliquid_cli::command_spec::{
    CommandSpec, ConfirmationPolicy, DryRunPolicy, RawPayloadPolicy, YES_ARG, check_args,
    supplied_secrets,
};
use hyperliquid_cli::errors::CliError;
use hyperliquid_cli::input_hardening::{validate_input_path, validate_resource_id};
use hyperliquid_cli::invocation::{Invocation, InvocationFlags};
use hyperliquid_cli::signing::ensure_supports_expires_after;
use hyperliquid_cli::stream::require_bound;

use crate::cli_shell::{Redactions, Selected};

pub(super) async fn run(selected: Selected, inv: Invocation) -> Result<(), anyhow::Error> {
    let spec = selected.entry.spec;
    let mode = inv.mode();

    validate_global_inputs(inv.flags())?;
    check_args(spec, &selected.matches)?;
    selected.bound.validate()?;
    if inv.flags().expires_after.is_some() {
        ensure_supports_expires_after(spec.signs)?;
    }
    if mode.machine_context()
        && let Some(bound) = spec.stream
    {
        require_bound(bound, &selected.matches, inv.settings().env())?;
    }

    if inv.flags().has_payload() {
        validate_raw_payload_supported(spec)?;
    }
    validate_prompt_policy(&inv, &selected)?;
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

    let api = inv.remote()?.api();
    if spec.uses_api() {
        api.probe_override().await?;
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
    /// from the options the command actually defines. A command without the
    /// option does not support it; any other lookup failure is a bug.
    pub(crate) fn from_matches(matches: &ArgMatches) -> Result<Self, CliError> {
        match matches.try_get_one::<bool>(YES_ARG) {
            Ok(given) => Ok(Self {
                supported: given.is_some(),
                given: given.copied().unwrap_or(false),
            }),
            Err(MatchesError::UnknownArgument { .. }) => Ok(Self::default()),
            Err(err) => Err(CliError::Internal(anyhow::anyhow!(
                "--{YES_ARG} is not a flag: {err}"
            ))),
        }
    }
}

fn validate_prompt_policy(inv: &Invocation, selected: &Selected) -> Result<(), CliError> {
    let spec = selected.entry.spec;
    let matches = &selected.matches;
    let mode = inv.mode();
    if mode.machine_context() && supplied_secrets(spec, matches).next().is_some() {
        return Err(CliError::Unsupported(
            "argv secret input is not supported in machine-readable contexts; use an interactive prompt or a future safe stdin/file secret input"
                .to_string(),
        ));
    }
    let confirmation = Confirmation::from_matches(matches)?;
    if inv.dry_run()
        || mode.can_prompt()
        || spec.confirmation != ConfirmationPolicy::Prompt
        || confirmation.given
        || !confirmation_applies(spec, matches)?
    {
        return Ok(());
    }
    // Whether a testnet run is exempt depends on the resolved network, not on
    // which flag or variable selected it, so it is asked last.
    if spec.testnet_unprompted && !spec.prompts_on(inv.remote()?.network()) {
        return Ok(());
    }

    Err(confirmation_required_error(selected, confirmation))
}

/// `account remove` with no selector has nothing to confirm yet: its run fails
/// on the missing selector (missing input) instead of prompting for one.
fn confirmation_applies(spec: &CommandSpec, matches: &ArgMatches) -> Result<bool, CliError> {
    let Some(id) = spec.confirms_with_arg else {
        return Ok(true);
    };
    matches.try_contains_id(id).map_err(|err| {
        CliError::Internal(anyhow::anyhow!(
            "{} confirms with `{id}`, which it does not define: {err}",
            spec.key()
        ))
    })
}

/// Name the blocked command, then give the safe next steps: a runnable
/// `--dry-run` preview when supported, and the `--yes` rerun only for use
/// after a human approves.
fn confirmation_required_error(selected: &Selected, confirmation: Confirmation) -> CliError {
    let spec = selected.entry.spec;
    let args = replay_args(selected.args.iter().cloned(), &selected.redactions);
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
    use crate::{Cli, SECRET_GLOBALS};
    use clap::CommandFactory;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn redactions_for(argv: &[&str]) -> Redactions {
        try_parse_cli_from(argv.iter().copied())
            .unwrap()
            .selected
            .unwrap()
            .redactions
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
        let args = replay_args(strings(&argv[1..]), &redactions);
        assert_eq!(args, strings(&["--format", "pretty", "wallet", "import"]));
    }

    #[test]
    fn replay_drops_secret_options_declared_by_the_spec() {
        let argv = ["hyperliquid", "wallet", "login", "--api-key", "bk_secret"];
        let redactions = redactions_for(&argv);
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
            .unwrap()
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
