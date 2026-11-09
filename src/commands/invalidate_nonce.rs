//! `account invalidate-nonce`: the exchange's `noop` action, which does nothing
//! except mark its nonce as used. A signed request whose reply never arrived may
//! still be in flight; once its nonce is burnt the exchange rejects it
//! (<https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint#invalidate-pending-nonce-noop>).
//! It moves no funds, so like `prio bid` it asks for no confirmation.

use clap::Args;
use hypersdk::hypercore::types::Action;
use serde::Serialize;

use crate::clock;
use crate::command_spec::{CliCommand, CommandSpec, Risk, arg};
use crate::commands::actions::{EXCHANGE_ACTION, send_l1_action_with_nonce};
use crate::commands::rows::{One, Row};
use crate::commands::support::print_dry_run;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::signing::nonce::{WINDOW_FUTURE_MS, WINDOW_PAST_MS, window_phrase};

/// Arguments for `account invalidate-nonce`.
#[derive(Args, Debug, Clone)]
pub struct InvalidateNonceArgs {
    /// The nonce to burn: the millisecond timestamp the in-flight request was signed with. It must be signed by the same wallet as that request
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub nonce: u64,
}

#[derive(Debug, Clone, Serialize)]
struct InvalidatedNonce {
    action: &'static str,
    status: &'static str,
    nonce: u64,
}

impl Row for InvalidatedNonce {
    const HEADERS: &'static [&'static str] = &["Action", "Status", "Nonce"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.to_string(),
            self.status.to_string(),
            self.nonce.to_string(),
        ]
    }
}

/// The nonce, once it is one the exchange could hold.
fn validate_nonce(nonce: u64, now_ms: u64) -> Result<u64, CliError> {
    if nonce <= now_ms.saturating_sub(WINDOW_PAST_MS)
        || nonce >= now_ms.saturating_add(WINDOW_FUTURE_MS)
    {
        return Err(CliError::Unsupported(format!(
            concat!(
                "nonce {} is outside the window the exchange accepts (from ",
                window_phrase!(),
                " now, in milliseconds since the Unix epoch), so no request with it can be in flight"
            ),
            nonce
        )));
    }
    Ok(nonce)
}

pub struct AccountInvalidateNonce;

impl CliCommand for AccountInvalidateNonce {
    type Args = InvalidateNonceArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["account", "invalidate-nonce"],
        "account",
        "Burn a nonce to invalidate an in-flight request",
        "Send a noop action that marks a nonce as used, so a signed request still in flight with that nonce is rejected. Sign with the same wallet as the request; the nonce is printed when a request's outcome is unknown. Moves no funds.",
    )
    .risk(Risk::AccountState)
    .args(&[arg("nonce").describe(concat!(
        "Millisecond timestamp nonce of the request to invalidate; within ",
        window_phrase!(),
        " now."
    ))]);

    async fn run(inv: &Invocation, args: &InvalidateNonceArgs) -> Result<(), anyhow::Error> {
        let nonce = validate_nonce(args.nonce, clock::now_ms())?;
        if inv.dry_run() {
            return print_dry_run(inv, "invalidate_nonce", serde_json::json!({"nonce": nonce}));
        }
        let signer = inv.resolve_signer()?;
        send_l1_action_with_nonce(
            inv.remote()?.api(),
            &signer,
            Action::Noop,
            nonce,
            None,
            EXCHANGE_ACTION,
        )
        .await?;
        inv.print(&One::new(InvalidatedNonce {
            action: "invalidate_nonce",
            status: "confirmed",
            nonce,
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn only_nonces_inside_the_exchange_window_can_be_burnt() {
        assert!(validate_nonce(NOW, NOW).is_ok());
        assert!(validate_nonce(NOW - WINDOW_PAST_MS + 1, NOW).is_ok());
        assert!(validate_nonce(NOW + WINDOW_FUTURE_MS - 1, NOW).is_ok());
        for outside in [NOW - WINDOW_PAST_MS, NOW + WINDOW_FUTURE_MS, 1, u64::MAX] {
            assert!(
                matches!(validate_nonce(outside, NOW), Err(CliError::Unsupported(_))),
                "{outside}"
            );
        }
    }
}
