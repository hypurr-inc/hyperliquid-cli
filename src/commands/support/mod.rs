use hypersdk::Address;

use crate::dry_run::{DryRunEnvelope, DryRunSigning};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::signing::SelectedSigner;

/// Print the `--dry-run` preview of a command: its name from the invocation,
/// what it `would_execute`, and its `args`.
///
/// A signing command's preview also names the signer, once resolved here and
/// without prompting; a signer that cannot be resolved prints as `null` with a
/// `signer_warning`. A local command's preview names no network.
pub(crate) fn print_dry_run(
    inv: &Invocation,
    would_execute: &str,
    args: serde_json::Value,
) -> Result<(), anyhow::Error> {
    print_dry_run_acting_for(inv, would_execute, args, None)
}

/// [`print_dry_run`] for an action sent for `vault_address`, the account an
/// order's `--on-behalf-of` names.
pub(crate) fn print_dry_run_acting_for(
    inv: &Invocation,
    would_execute: &str,
    args: serde_json::Value,
    vault_address: Option<String>,
) -> Result<(), anyhow::Error> {
    let mut envelope = DryRunEnvelope::new(inv.command_key(), would_execute, args).with_payload(
        inv.payload()
            .map(|payload| crate::input_hardening::redact_sensitive_json(payload.clone())),
    );
    if inv.command().dry_run_names_signer() {
        envelope = envelope.with_signing(dry_run_signing(inv, vault_address));
    }
    if !inv.command().local {
        envelope = envelope.with_network(inv.remote().ok().map(|remote| remote.network()));
    }
    inv.print_untimed(&envelope);
    Ok(())
}

fn dry_run_signing(inv: &Invocation, vault_address: Option<String>) -> DryRunSigning {
    match inv.resolve_signer() {
        Ok(resolved) => DryRunSigning::new(
            Ok(resolved.address().to_string()),
            vault_address
                .clone()
                .or_else(|| Some(resolved.query_address().to_string())),
            vault_address,
        ),
        Err(err) => DryRunSigning::new(Err(err.to_string()), vault_address.clone(), vault_address),
    }
}

/// Refuse a stored API wallet for `action_type`, which only the master account
/// may sign. Runs on dry runs too, so a preview never succeeds where the live
/// run is rejected.
pub(crate) fn require_master_signer(
    signer: &SelectedSigner,
    action_type: &str,
) -> Result<(), CliError> {
    if signer.is_stored_agent() {
        return Err(CliError::Unsupported(format!(
            "{action_type} must be signed by the master account; select the master wallet, not a stored API wallet"
        )));
    }
    Ok(())
}

pub(crate) fn resolve_optional_acting_account_target(
    inv: &Invocation,
    selector: Option<&str>,
) -> Result<Option<Address>, anyhow::Error> {
    selector
        .map(|selector| inv.resolve_account(selector))
        .transpose()
}
