//! Helpers every command's `run` shares: dry-run envelopes, address parsing,
//! GitHub issue link helpers, and acting-account lookup.

use hypersdk::Address;

use crate::dry_run::{
    ActionPlan, ActionReversibility, DryRunEnvelope, DryRunSigningContext, LiveSubmissionPolicy,
};
use crate::errors::CliError;
use crate::invocation::Invocation;

pub const REPORT_REPO: &str = "hypurr-inc/hyperliquid-cli";

/// Parse a raw address string strictly as an Ethereum 40-hex-character address.
pub fn parse_address(raw: &str) -> Result<Address, CliError> {
    raw.trim()
        .parse::<Address>()
        .map_err(|_| CliError::Unsupported(format!("Invalid address: {}", raw.trim())))
}

/// URL encode a UTF-8 string for query parameters.
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

/// Open a URL in the platform default web browser.
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
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .status()
            .is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

pub(crate) fn print_dry_run(
    inv: &Invocation,
    command: impl Into<String>,
    details: serde_json::Value,
) -> Result<(), anyhow::Error> {
    print_dry_run_envelope(inv, DryRunEnvelope::from_details(command, details))
}

pub(crate) fn print_dry_run_envelope(
    inv: &Invocation,
    envelope: DryRunEnvelope,
) -> Result<(), anyhow::Error> {
    inv.print_dry_run(envelope);
    Ok(())
}

pub(crate) fn dry_run_details(
    inv: &Invocation,
    would_execute: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let mut details = serde_json::Map::new();
    details.insert(
        "would_execute".to_string(),
        serde_json::json!(would_execute),
    );
    details.insert("args".to_string(), args);
    if let Some(payload) = inv.payload() {
        details.insert("payload".to_string(), redact_payload(payload.clone()));
    }
    serde_json::Value::Object(details)
}

pub(crate) fn dry_run_signed_details(
    inv: &Invocation,
    would_execute: &str,
    args: serde_json::Value,
    signer: Option<String>,
    acting_as: Option<String>,
    vault_address: Option<String>,
) -> serde_json::Value {
    let mut details = match dry_run_details(inv, would_execute, args) {
        serde_json::Value::Object(details) => details,
        _ => serde_json::Map::new(),
    };
    details.insert("signer".to_string(), serde_json::json!(signer));
    details.insert("acting_as".to_string(), serde_json::json!(acting_as));
    details.insert(
        "vault_address".to_string(),
        serde_json::json!(vault_address),
    );
    serde_json::Value::Object(details)
}

pub(crate) fn transfer_dry_run_envelope(
    inv: &Invocation,
    command: &str,
    action: crate::commands::transfers::TransferActionKind,
    would_execute: &str,
    args: serde_json::Value,
) -> DryRunEnvelope {
    let plan = ActionPlan::signed_exchange_action(
        would_execute,
        action.reversibility(),
        LiveSubmissionPolicy::ValidateConfirmSignSubmit,
    );
    DryRunEnvelope::from_action_plan(command, &plan, args)
        .with_payload(inv.payload().map(|payload| redact_payload(payload.clone())))
}

pub(crate) fn signed_action_dry_run_envelope(
    inv: &Invocation,
    command: &str,
    would_execute: &str,
    reversibility: ActionReversibility,
    args: serde_json::Value,
    signing_context: DryRunSigningContext,
) -> DryRunEnvelope {
    let plan = ActionPlan::signed_exchange_action(
        would_execute,
        reversibility,
        LiveSubmissionPolicy::ValidateConfirmSignSubmit,
    );
    DryRunEnvelope::from_action_plan(command, &plan, args)
        .with_payload(inv.payload().map(|payload| redact_payload(payload.clone())))
        .with_signing_context(signing_context)
}

pub(crate) fn print_signed_dry_run(
    inv: &Invocation,
    command: &str,
    would_execute: &str,
    reversibility: ActionReversibility,
    args: serde_json::Value,
) -> Result<(), anyhow::Error> {
    print_dry_run_envelope(
        inv,
        signed_action_dry_run_envelope(
            inv,
            command,
            would_execute,
            reversibility,
            args,
            default_dry_run_signing_context(inv),
        ),
    )
}

pub(crate) fn print_transfer_dry_run(
    inv: &Invocation,
    command: &str,
    action: crate::commands::transfers::TransferActionKind,
    would_execute: &str,
    args: serde_json::Value,
) -> Result<(), anyhow::Error> {
    print_transfer_dry_run_as(inv, command, action, would_execute, args, None)
}

pub(crate) fn print_transfer_dry_run_as(
    inv: &Invocation,
    command: &str,
    action: crate::commands::transfers::TransferActionKind,
    would_execute: &str,
    args: serde_json::Value,
    signing: Option<DryRunSigningContext>,
) -> Result<(), anyhow::Error> {
    let mut envelope = transfer_dry_run_envelope(inv, command, action, would_execute, args);
    if let Some(signing) = signing {
        envelope = envelope.with_signing_context(signing);
    }
    print_dry_run_envelope(inv, envelope)
}

pub(crate) fn dry_run_signer_address(inv: &Invocation) -> Option<String> {
    dry_run_signing_addresses(inv).0
}

pub(crate) fn dry_run_signing_addresses(inv: &Invocation) -> (Option<String>, Option<String>) {
    inv.resolve_signer()
        .ok()
        .map(|resolved| {
            (
                Some(resolved.address().to_string()),
                Some(resolved.query_address().to_string()),
            )
        })
        .unwrap_or((None, None))
}

pub(crate) fn dry_run_vault_signing_addresses(
    inv: &Invocation,
    vault_address: Option<String>,
) -> (Option<String>, Option<String>, Option<String>) {
    let (signer, query_address) = dry_run_signing_addresses(inv);
    let acting_as = vault_address.clone().or(query_address);
    (signer, acting_as, vault_address)
}

pub(crate) fn default_dry_run_signing_context(inv: &Invocation) -> DryRunSigningContext {
    let (signer, acting_as) = dry_run_signing_addresses(inv);
    DryRunSigningContext::new(signer, acting_as, None)
}

pub(crate) fn redact_payload(value: serde_json::Value) -> serde_json::Value {
    crate::input_hardening::redact_sensitive_json(value)
}

pub(crate) fn resolve_optional_acting_account_target(
    inv: &Invocation,
    selector: Option<&str>,
) -> Result<Option<Address>, anyhow::Error> {
    selector
        .map(|selector| inv.resolve_acting_account(selector))
        .transpose()
}
