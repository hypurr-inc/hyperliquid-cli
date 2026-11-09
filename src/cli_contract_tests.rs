//! Policy invariants over the generated command registry.
//!
//! The registry is built from the clap tree and each command's `CommandSpec`, so
//! names, flags and defaults cannot drift. What can still go wrong is policy that
//! contradicts itself or names args the command does not have.

use hyperliquid_cli::command_registry::CommandRegistry;
use hyperliquid_cli::command_spec::{ConfirmationPolicy, Mutability, YES_ARG};
use hyperliquid_cli::commands;

fn problems_or_pass(title: &str, problems: Vec<String>) {
    assert!(
        problems.is_empty(),
        "{title}: {} problem(s)\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

#[test]
fn every_schema_input_has_a_description() {
    let problems = CommandRegistry::get()
        .commands()
        .iter()
        .flat_map(|contract| {
            contract
                .inputs
                .iter()
                .filter(|input| {
                    input
                        .description
                        .as_deref()
                        .is_none_or(|description| description.trim().is_empty())
                })
                .map(|input| {
                    format!(
                        "{}: arg `{}` has no description",
                        contract.command_key(),
                        input.id
                    )
                })
        })
        .collect();
    problems_or_pass("args without descriptions", problems);
}

#[test]
fn testnet_unprompted_commands_prompt_on_mainnet() {
    let problems = CommandRegistry::get()
        .commands()
        .iter()
        .filter(|contract| {
            contract.spec.testnet_unprompted
                && (contract.spec.confirmation != ConfirmationPolicy::Prompt
                    || contract.confirmation_bypass_arg().is_none())
        })
        .map(|contract| {
            format!(
                "{}: skips the testnet prompt but has no mainnet prompt or --yes",
                contract.command_key()
            )
        })
        .collect();
    problems_or_pass("testnet policy", problems);
}

#[test]
fn bounded_commands_define_the_args_that_bound_them() {
    let mut problems = Vec::new();
    for contract in CommandRegistry::get().commands() {
        let Some(bound) = contract.spec.stream else {
            continue;
        };
        for id in bound
            .cli_args()
            .iter()
            .copied()
            .chain(bound.required_when())
        {
            if contract.input(id).is_none() {
                problems.push(format!("{}: missing `{id}`", contract.command_key()));
            }
        }
    }
    problems_or_pass("stream bounds", problems);
}

#[test]
fn stream_bounds_are_reported_for_every_bounded_command() {
    let problems = CommandRegistry::get()
        .commands()
        .iter()
        .filter(|contract| {
            contract.spec.stream.is_some() != contract.policy.stream_bounds.is_some()
        })
        .map(|contract| {
            format!(
                "{}: stream_bounds disagrees with its spec",
                contract.command_key()
            )
        })
        .collect();
    problems_or_pass("stream bounds in the schema", problems);
}

#[test]
fn policy_args_are_args_of_their_command() {
    let mut problems = Vec::new();
    for contract in CommandRegistry::get().commands() {
        if let Some(id) = contract.spec.confirms_with_arg
            && contract.input(id).is_none()
        {
            problems.push(format!(
                "{}: confirms_with_arg `{id}`",
                contract.command_key()
            ));
        }
        for group in contract.spec.one_of_required {
            for id in *group {
                if contract.input(id).is_none() {
                    problems.push(format!(
                        "{}: one_of_required `{id}`",
                        contract.command_key()
                    ));
                }
            }
        }
        for secret in contract.spec.args.iter().filter(|arg| arg.secret) {
            if contract.input(secret.id).is_none() {
                problems.push(format!(
                    "{}: secret arg `{}`",
                    contract.command_key(),
                    secret.id
                ));
            }
        }
    }
    problems_or_pass("policy args", problems);
}

#[test]
fn local_commands_never_use_the_api() {
    let problems = commands::entries()
        .iter()
        .map(|entry| entry.spec)
        .filter(|spec| spec.local && spec.uses_api())
        .map(|spec| format!("{}: local but uses the API", spec.key()))
        .collect();
    problems_or_pass("local commands", problems);
}

#[test]
fn state_changing_commands_are_never_read_only() {
    let problems = commands::entries()
        .iter()
        .map(|entry| entry.spec)
        .filter(|spec| {
            spec.changes_state()
                && matches!(
                    spec.mutability(),
                    Mutability::ReadOnly | Mutability::Streaming
                )
        })
        .map(|spec| format!("{}: changes state but reports read-only", spec.key()))
        .collect();
    problems_or_pass("mutability", problems);
}

/// A `.prompt()` spec promises its own `run` asks before acting, and the
/// pipeline refuses to run it where it cannot ask. A command that accepts
/// `--yes` answers that prompt with the flag. One without it can only be
/// prompting for a secret it reads at the prompt (`wallet import`), so it must
/// declare that secret.
#[test]
fn prompting_commands_without_yes_prompt_for_a_secret() {
    let problems = CommandRegistry::get()
        .commands()
        .iter()
        .filter(|contract| {
            contract.spec.confirmation == ConfirmationPolicy::Prompt
                && contract.input(YES_ARG).is_none()
                && !contract.spec.args.iter().any(|arg| arg.secret)
        })
        .map(|contract| {
            format!(
                "{}: prompts but has neither --{YES_ARG} nor a secret arg to prompt for",
                contract.command_key()
            )
        })
        .collect();
    problems_or_pass("prompt policy", problems);
}

/// A command nested under a prefix needs that prefix to be a group (help text,
/// `subcommand_required`) or itself a command. A new group is its module's
/// `GROUP` plus one line in `commands::groups`.
#[test]
fn every_nested_command_sits_under_a_declared_group() {
    let mut problems = Vec::new();
    for entry in commands::entries() {
        let path = entry.spec.path;
        for len in 1..path.len() {
            let prefix = &path[..len];
            let declared = commands::groups().iter().any(|group| group.path == prefix)
                || commands::entries()
                    .iter()
                    .any(|other| other.spec.path == prefix);
            if !declared {
                problems.push(format!(
                    "{}: `{}` is neither a group nor a command",
                    entry.spec.key(),
                    prefix.join(" ")
                ));
            }
        }
    }
    problems_or_pass("command groups", problems);
}
