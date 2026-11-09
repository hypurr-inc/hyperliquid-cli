use hypersdk::Address;

use crate::dry_run::{DryRunEnvelope, DryRunSigning};
use crate::errors::CliError;
use crate::invocation::Invocation;

/// A `--dex` value, or `None` for the default dex.
///
/// The exchange treats the empty dex name as the first perp dex, so a blank
/// flag must behave like no flag.
#[must_use]
pub(crate) fn normalize_dex(dex: Option<&str>) -> Option<&str> {
    dex.map(str::trim).filter(|dex| !dex.is_empty())
}

/// Validate a `--dex` value against the exchange and return its canonical name.
///
/// An unknown DEX is explicit invalid input: it fails here instead of reading an
/// empty account or an empty mid map, and the canonical spelling is what every
/// later request uses.
pub(crate) async fn canonical_dex(
    client: &hypersdk::hypercore::HttpClient,
    dex: Option<&str>,
) -> Result<Option<String>, CliError> {
    match normalize_dex(dex) {
        Some(dex) => Ok(Some(
            crate::assets::find_perp_dex(client, dex)
                .await?
                .name()
                .to_string(),
        )),
        None => Ok(None),
    }
}

/// Print the `--dry-run` preview of a command: its name from the invocation,
/// what it `would_execute`, and its `args`.
///
/// A signing command's preview also names the signer, once resolved here and
/// without prompting. A signer that cannot be resolved prints as `null` with a
/// `signer_warning`; one that resolves but doesn't fit the command fails the
/// preview, as the live run would. A local command's preview names no network.
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
        envelope = envelope.with_signing(dry_run_signing(inv, vault_address)?);
    }
    if !inv.command().local {
        envelope = envelope.with_network(inv.remote().ok().map(|remote| remote.network()));
    }
    inv.print_untimed(&envelope);
    Ok(())
}

fn dry_run_signing(
    inv: &Invocation,
    vault_address: Option<String>,
) -> Result<DryRunSigning, anyhow::Error> {
    Ok(match inv.preview_signer()? {
        Ok(resolved) => DryRunSigning::new(
            Ok(resolved.address().to_string()),
            vault_address
                .clone()
                .or_else(|| Some(resolved.query_address().to_string())),
            vault_address,
        ),
        Err(err) => DryRunSigning::new(Err(err.to_string()), vault_address.clone(), vault_address),
    })
}

pub(crate) fn resolve_optional_acting_account_target(
    inv: &Invocation,
    selector: Option<&str>,
) -> Result<Option<Address>, anyhow::Error> {
    selector
        .map(|selector| inv.resolve_account(selector))
        .transpose()
}
