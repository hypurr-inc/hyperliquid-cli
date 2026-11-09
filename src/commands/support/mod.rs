use hypersdk::Address;

use crate::dry_run::{DryRunEnvelope, DryRunSigning};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::signing::SelectedSigner;

pub const REPORT_REPO: &str = "hypurr-inc/hyperliquid-cli";

/// Parse a raw address string strictly as an Ethereum 40-hex-character address.
pub fn parse_address(raw: &str) -> Result<Address, CliError> {
    raw.trim()
        .parse::<Address>()
        .map_err(|_| CliError::Unsupported(format!("Invalid address: {}", raw.trim())))
}

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

pub fn urlencoding(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

pub fn open_in_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).status().is_ok()
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .status()
            .is_ok()
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()
            .is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

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
