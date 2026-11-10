//! How orders reach the exchange. A placement is sent only with a [`Cleared`],
//! and only [`clear_to_send`] and an [`Ungated`] declaration make one, so no
//! command can place orders by skipping a step.

use hypersdk::hypercore::types::{
    Action, BatchOrder, OrderGrouping, OrderRequest, OrderResponseStatus,
};
use serde::Serialize;

use super::builder_check;
use super::consent::Consent;
use super::context::OrderSubmissionContext;
use super::exchange::Wording;
use super::hip3::{Hip3MarginPlan, execute_hip3_margin_transfers, plan_hip3_margin_transfers};
use super::planning::TwapCreatePlan;
use super::prepared::{PreparedOrder, PreparedOrderBatch, PreparedTpslBatch};
use crate::assets::PerpAsset;
use crate::builder_fee::BuilderFee;
use crate::errors::CliError;
use crate::hints;
use crate::margin::{self, MarginPlan};
use crate::output::Notice;

/// What a placement must clear before anything is sent, in the one order that
/// is safe: the margin mode holds, the builder is approved, the HIP-3 dexes are
/// funded, the user approves.
pub(super) struct Gate<'a> {
    consent: Consent<'a>,
    /// A perp whose margin mode for the account must match what the plan
    /// requires, if it requires one.
    margin: Option<(&'a PerpAsset, MarginPlan)>,
    /// The orders whose HIP-3 dex margin the helper transfer tops up. Only a
    /// single order (`orders create`, a bracket entry) funds; every other
    /// gate leaves this empty so it moves no money.
    funded: Vec<&'a PreparedOrder>,
    builder: Option<&'a BuilderFee>,
}

impl<'a> Gate<'a> {
    /// One order: `orders create`, a bracket entry.
    pub(super) fn order(order: &'a PreparedOrder) -> Self {
        Self::funding(Consent::Order(order), order)
    }

    /// A parent order with grouped TP/SL children: the parent funds.
    pub(super) fn tpsl_order(parent: &'a PreparedOrder, children: &'a PreparedTpslBatch) -> Self {
        Self::funding(Consent::Tpsl(children), parent)
    }

    fn funding(consent: Consent<'a>, order: &'a PreparedOrder) -> Self {
        Self {
            consent,
            margin: order.perp().zip(order.margin),
            funded: vec![order],
            builder: order.builder.as_ref(),
        }
    }

    pub(super) fn scale(batch: &PreparedOrderBatch) -> Gate<'_> {
        Gate::batch(Consent::Scale(batch), batch)
    }

    pub(super) fn batch_create(batch: &PreparedOrderBatch) -> Gate<'_> {
        Gate::batch(Consent::BatchCreate(batch), batch)
    }

    /// A batch never moves funds on its own: `funded` stays empty, so the
    /// helper transfer is never sent for scale or batch-create. Its margin
    /// check is the one [`PreparedOrderBatch::margin_check`] states.
    fn batch(consent: Consent<'a>, batch: &'a PreparedOrderBatch) -> Self {
        Self {
            consent,
            margin: batch.margin_check().map(|(perp, margin)| (perp, *margin)),
            funded: Vec::new(),
            builder: None,
        }
    }

    /// TP/SL orders on an existing position.
    pub(super) fn position_tpsl(
        perp: &'a PerpAsset,
        margin: MarginPlan,
        batch: &'a PreparedTpslBatch,
    ) -> Self {
        Self::position(Consent::Tpsl(batch), perp, margin)
    }

    pub(super) fn twap(plan: &'a TwapCreatePlan) -> Self {
        Self::position(Consent::Twap(plan), &plan.perp, plan.margin)
    }

    /// Orders that act on an existing position and need no margin of their own.
    fn position(consent: Consent<'a>, perp: &'a PerpAsset, margin: MarginPlan) -> Self {
        Self {
            consent,
            margin: Some((perp, margin)),
            funded: Vec::new(),
            builder: None,
        }
    }
}

/// Proof that a placement passed what its command owes. It carries the builder
/// fee the checks and the consent block were shown, so the fee that is sent is
/// the fee that was approved.
pub(super) struct Cleared<'a> {
    builder: Option<&'a BuilderFee>,
}

/// The commands that place orders without [`clear_to_send`], and why. A new one
/// is declared here, in view, not by skipping the gate.
///
/// `orders trailing-stop` is not listed: it sends a `trailingStop` action, not
/// an order batch, so it never reaches [`submit_orders`]. `orders cancel`,
/// `modify` and the TWAP and schedule commands act on existing orders.
#[derive(Debug, Clone, Copy)]
pub(super) enum Ungated {
    /// Chase's quotes are post-only limit orders on a perpetual market it
    /// resolved itself, with no builder fee, no margin-mode request and no
    /// HIP-3 funding. It runs unattended for minutes, so instead of asking it
    /// refuses to run on mainnet without `--yes`.
    Chase,
}

impl Ungated {
    pub(super) fn cleared(self) -> Cleared<'static> {
        match self {
            Self::Chase => Cleared { builder: None },
        }
    }
}

pub(super) async fn clear_to_send<'a>(
    context: OrderSubmissionContext<'_>,
    gate: Gate<'a>,
) -> Result<Cleared<'a>, anyhow::Error> {
    if let Some((perp, margin)) = gate.margin {
        verify_margin_mode(context, perp, &margin).await?;
    }
    if let (Some(builder), Some(user)) = (
        gate.builder,
        builder_check::approver(context.vault_address, context.signer.query_address()),
    ) {
        builder_check::ensure_approved(context.api, context.inv.mode(), user, builder).await?;
    }
    let user = context.account();
    let helpers = plan_hip3_margin_transfers(context.api, user, &gate.funded).await?;
    if context.requires_approval {
        warn_hip3_margin_helpers(&helpers);
    }
    context.consent(&gate.consent, gate.builder)?;
    execute_hip3_margin_transfers(context.inv, context.signer, user, &helpers).await?;
    Ok(Cleared {
        builder: gate.builder,
    })
}

fn warn_hip3_margin_helpers(helpers: &[Hip3MarginPlan]) {
    for plan in helpers {
        Notice::Caution(format!(
            "This order requires a helper transfer of {} {} from perp to dex:{} before placement.",
            plan.amount, plan.token, plan.dest_dex
        ))
        .eprint();
    }
}

/// Fail before placing an order when `--margin-mode` does not match the margin
/// mode the account is set to for `perp`. The account's setting comes from
/// `activeAssetData`, so it is known before a position exists.
async fn verify_margin_mode(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    margin: &MarginPlan,
) -> Result<(), anyhow::Error> {
    let Some(requested) = margin.required_account_mode() else {
        return Ok(());
    };
    let coin = perp.display_name();
    let current = margin::read_account_mode(context.api, context.account(), perp).await?;
    if current != requested {
        let acting_as = context.vault_address.map(|vault| vault.to_string());
        return Err(CliError::Configuration(format!(
            "orders requested --margin-mode {requested} but the current {coin} margin mode is {current}; use `{}` first",
            hints::update_leverage_command(
                &coin,
                "<N>",
                !requested.is_cross(),
                acting_as.as_deref()
            )
        ))
        .into());
    }
    Ok(())
}

/// The action a builder fee travels in: `{ type: "order", orders, grouping,
/// builder: { b, f, s } }`.
#[derive(Debug, Serialize)]
struct BuilderOrderAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    orders: Vec<OrderRequest>,
    grouping: OrderGrouping,
    builder: BuilderFee,
}

/// The builder fee in `cleared` goes in the raw action, because the SDK's
/// `BatchOrder.builder` carries only `{ b, f }`, which cannot express sub-builders;
/// without a builder fee the SDK action is sent. `wording` names the placement in
/// the error.
pub(super) async fn submit_orders(
    context: OrderSubmissionContext<'_>,
    cleared: &Cleared<'_>,
    batch: BatchOrder,
    wording: Wording,
) -> Result<Vec<OrderResponseStatus>, CliError> {
    let answer = match cleared.builder {
        Some(builder) => {
            let action = BuilderOrderAction {
                action_type: "order",
                orders: batch.orders,
                grouping: batch.grouping,
                builder: builder.clone(),
            };
            context.send_raw(&action, wording).await?
        }
        None => context.send(Action::Order(batch), wording).await?,
    };
    answer.statuses()
}
