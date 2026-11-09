use hypersdk::Address;
use hypersdk::hypercore::types::Action;
use serde::Serialize;

use super::consent::{Consent, ConsentTarget};
use super::exchange::{Answer, Wording};
use crate::builder_fee::BuilderFee;
use crate::errors::CliError;
use crate::exchange::actions;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::network::Network;
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
    pub(super) fn account(self) -> Address {
        self.vault_address
            .unwrap_or_else(|| self.signer.query_address())
    }

    /// Sign and send `action` for the account this context acts for; the
    /// exchange's answer, worded by `wording` where it is refused or cannot be
    /// read.
    pub(super) async fn send(self, action: Action, wording: Wording) -> Result<Answer, CliError> {
        let payload = actions::send_l1_action(
            self.api,
            self.signer,
            action,
            self.vault_address,
            wording.rejected,
        )
        .await?;
        Ok(Answer::new(payload, wording))
    }

    /// [`send`](Self::send) for an action the SDK has no type for.
    pub(super) async fn send_raw(
        self,
        action: &impl Serialize,
        wording: Wording,
    ) -> Result<Answer, CliError> {
        let payload = actions::send_raw_l1_json_action(
            self.api,
            self.signer,
            action,
            self.vault_address,
            wording.rejected,
        )
        .await?;
        Ok(Answer::new(payload, wording))
    }

    /// A no-op when no approval is required. A decline fails with `consent`'s error.
    pub(super) fn consent(
        self,
        consent: &Consent<'_>,
        builder: Option<&BuilderFee>,
    ) -> Result<(), CliError> {
        if !self.requires_approval {
            return Ok(());
        }
        let target = ConsentTarget::new(
            self.inv.mode(),
            Network::from(self.api.chain()),
            self.signer,
            self.vault_address,
            builder,
        );
        if consent.ask(&target)? {
            return Ok(());
        }
        Err(consent.declined())
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
            return Err(crate::consent::unattended_refusal(action, command));
        }
        Ok(())
    }
}
