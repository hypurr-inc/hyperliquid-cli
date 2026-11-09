//! The skeleton every order command runs: who it acts for, how its preview is
//! finished and printed, and the context a live run sends in. A command
//! supplies its plan and its executor; it cannot forget the consent wiring or
//! name the acting account a second way.
//!
//! A preview signs nothing, so the reads it adds to a plan share one policy:
//! the account they read is the vault, else the signer's own when one resolves
//! (none: no read). A signer that resolves but could not sign the command fails
//! the preview, as the live run would. A read that fails never fails the
//! preview, except HIP-3 funding, whose error has always been the preview's:
//!
//! | read             | when it fails                                  |
//! |------------------|------------------------------------------------|
//! | margin mode      | the preview keeps the market rule's default    |
//! | builder approval | the preview reports `approval: unknown`        |
//! | HIP-3 funding    | the preview fails                              |

use hypersdk::Address;

use super::builder_check;
use super::context::OrderSubmissionContext;
use super::hip3::{Hip3MarginPlan, plan_hip3_margin_transfers};
use super::prepared::PreparedOrder;
use super::preview::OrderDryRunPlan;
use crate::assets::PerpAsset;
use crate::builder_fee::BuilderFee;
use crate::commands::common::support::{
    print_dry_run_acting_for, resolve_optional_acting_account_target,
};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::margin::{self, MarginPlan};
use crate::signing::SelectedSigner;

pub(super) struct OrderRun<'a> {
    inv: &'a Invocation,
    /// The vault or subaccount the command acts for; `None` acts for the signer.
    vault: Option<Address>,
    /// The `--on-behalf-of` selector as the user typed it.
    on_behalf_of: Option<String>,
    yes: bool,
}

impl<'a> OrderRun<'a> {
    /// A command with a `--yes` flag: it asks for approval where its spec
    /// prompts, unless `yes` was given.
    pub(super) fn new(
        inv: &'a Invocation,
        on_behalf_of: Option<&str>,
        yes: bool,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            inv,
            vault: resolve_optional_acting_account_target(inv, on_behalf_of)?,
            on_behalf_of: on_behalf_of.map(str::to_string),
            yes,
        })
    }

    /// A command that never asks (`cancel`, `modify`, `twap-cancel`): it has no
    /// `--yes` to read.
    pub(super) fn unprompted(
        inv: &'a Invocation,
        on_behalf_of: Option<&str>,
    ) -> Result<Self, anyhow::Error> {
        debug_assert!(!inv.requires_approval(false)?);
        Self::new(inv, on_behalf_of, false)
    }

    pub(super) fn on_behalf_of(&self) -> Option<&str> {
        self.on_behalf_of.as_deref()
    }

    /// Print the `--dry-run` document of `plan`, naming the account the
    /// command acts for.
    pub(super) fn preview(&self, plan: OrderDryRunPlan) -> Result<(), anyhow::Error> {
        let would_execute = plan.would_execute();
        let args = plan.with_account(self.on_behalf_of()).into_args();
        print_dry_run_acting_for(
            self.inv,
            would_execute,
            args,
            self.vault.map(|address| address.to_string()),
        )
    }

    /// The context a live run sends in. It fails before any side effect when
    /// the connection is not there. `yes` waives the approval prompt, which
    /// only commands whose spec prompts on this network ask for.
    pub(super) fn context<'s>(
        &'s self,
        signer: &'s SelectedSigner,
    ) -> Result<OrderSubmissionContext<'s>, CliError> {
        let remote = self.inv.remote()?;
        Ok(OrderSubmissionContext {
            inv: self.inv,
            api: remote.api(),
            signer,
            vault_address: self.vault,
            requires_approval: self.inv.requires_approval(self.yes)?,
        })
    }

    /// The account a preview reads; see the module's policy.
    fn preview_account(&self) -> Result<Option<Address>, CliError> {
        if let Some(vault) = self.vault {
            return Ok(Some(vault));
        }
        Ok(self
            .inv
            .dry_run_signer()?
            .map(SelectedSigner::query_address))
    }

    /// `plan` as the preview should show it: when nobody named a mode and the
    /// account can choose, the mode the account is set to for `perp`.
    pub(super) async fn previewed_margin(
        &self,
        perp: &PerpAsset,
        plan: MarginPlan,
    ) -> Result<MarginPlan, CliError> {
        if !plan.account_decides_preview() {
            return Ok(plan);
        }
        let Some(account) = self.preview_account()? else {
            return Ok(plan);
        };
        // Best effort: an unread account leaves the market rule's guess, and the
        // live run's own check reports the failure.
        Ok(
            match margin::read_account_mode(self.inv.remote()?.api(), account, perp).await {
                Ok(mode) => plan.with_account_mode(mode),
                Err(_unread) => plan,
            },
        )
    }

    /// [`Self::previewed_margin`] for an order that may be on a market with no
    /// margin, which previews none.
    pub(super) async fn previewed_margin_of(
        &self,
        perp: Option<&PerpAsset>,
        plan: Option<MarginPlan>,
    ) -> Result<Option<MarginPlan>, CliError> {
        match (perp, plan) {
            (Some(perp), Some(plan)) => self.previewed_margin(perp, plan).await.map(Some),
            _ => Ok(None),
        }
    }

    /// The HIP-3 helper transfers a live run of `orders` would make first, as
    /// the dry-run `side_effects`; `None` when there are none or no account to
    /// read.
    pub(super) async fn hip3_side_effects(
        &self,
        orders: &[&PreparedOrder],
    ) -> Result<Option<serde_json::Value>, anyhow::Error> {
        let Some(account) = self.preview_account()? else {
            return Ok(None);
        };
        let helpers = plan_hip3_margin_transfers(self.inv.remote()?.api(), account, orders).await?;
        Ok((!helpers.is_empty()).then(|| helpers.iter().map(Hip3MarginPlan::to_json).collect()))
    }

    /// The dry-run `builder_approval`: what `maxBuilderFee` says about the
    /// builder fee a live run would carry; `None` when there is no fee, or no
    /// account whose approval is checked ([`builder_check::approver`]).
    pub(super) async fn builder_approval(
        &self,
        fee: Option<&BuilderFee>,
    ) -> Result<Option<serde_json::Value>, CliError> {
        let Some(fee) = fee else {
            return Ok(None);
        };
        let signer_account = self
            .inv
            .dry_run_signer()?
            .map(SelectedSigner::query_address);
        let Some(account) = signer_account
            .and_then(|signer_account| builder_check::approver(self.vault, signer_account))
        else {
            return Ok(None);
        };
        let approval = builder_check::check(self.inv.remote()?.api(), account, fee).await;
        Ok(Some(approval.to_json(account, fee)))
    }
}
