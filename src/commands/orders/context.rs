//! What a live order command runs against: the connection, the signer, the
//! account it acts for, and whether the user still has to approve.

use hypersdk::Address;

use super::consent::{ConsentKind, ConsentTarget};
use super::exchange::OrderBuilderFee;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::signing::SelectedSigner;

#[derive(Clone, Copy)]
pub(super) struct OrderSubmissionContext<'a> {
    pub(super) inv: &'a Invocation,
    pub(super) api: &'a ApiClient,
    pub(super) signer: &'a SelectedSigner,
    /// The vault or subaccount the orders act for; `None` acts for the signer.
    pub(super) vault_address: Option<Address>,
    /// The user must approve this action: the command prompts on this network
    /// and `--yes` was not given.
    pub(super) requires_approval: bool,
}

impl<'a> OrderSubmissionContext<'a> {
    /// The account whose orders and balances this run reads and changes.
    pub(super) fn account(self) -> Address {
        self.vault_address
            .unwrap_or_else(|| self.signer.query_address())
    }

    /// Ask the user to approve an action before it is sent. A no-op when no
    /// approval is required. A decline fails with `kind`'s error.
    pub(super) fn consent(
        self,
        kind: ConsentKind,
        builder: Option<&OrderBuilderFee>,
        ask: impl FnOnce(&ConsentTarget<'_>) -> Result<bool, CliError>,
    ) -> Result<(), CliError> {
        if !self.requires_approval {
            return Ok(());
        }
        let target = ConsentTarget::new(self, builder);
        if ask(&target)? {
            return Ok(());
        }
        Err(kind.declined())
    }

    /// Refuse a command that cannot ask (it runs unattended for minutes) when
    /// approval is still required. `action` completes "Can't {action} on
    /// mainnet without --yes"; `command` is the invocation to rerun, which
    /// gets `--yes` appended.
    pub(super) fn refuse_unattended_without_yes(
        self,
        action: &str,
        command: &str,
    ) -> Result<(), CliError> {
        if self.requires_approval {
            return Err(unattended_refusal(action, command));
        }
        Ok(())
    }
}

/// The error for an unattended command that still needs approval; see
/// [`OrderSubmissionContext::refuse_unattended_without_yes`].
pub(super) fn unattended_refusal(action: &str, command: &str) -> CliError {
    CliError::Configuration(format!(
        "Can't {action} on mainnet without --yes. Rerun with --yes once you approve it:\n    {command} --yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_names_the_action_and_the_command_to_rerun_with_yes() {
        let err = unattended_refusal("chase", "hyperliquid --format json orders chase --coin ETH");
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            err.to_string(),
            "Configuration error: Can't chase on mainnet without --yes. Rerun with --yes once you approve it:\n    hyperliquid --format json orders chase --coin ETH --yes"
        );
    }
}
