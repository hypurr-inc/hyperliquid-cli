use std::io::{self, Write};

use chrono::Utc;
use hypersdk::hypercore::types::OrderGrouping;
use hypersdk::{Address, Decimal};

use super::context::OrderSubmissionContext;
use super::exchange::OrderBuilderFee;
use super::planning::{ScheduleCancelPlan, TwapCreatePlan};
use super::prepared::{
    PreparedOrder, PreparedOrderBatch, PreparedTpslBatch, QuoteAmount, TpslLegKind,
};
use crate::errors::CliError;
use crate::mode::Mode;
use crate::network::Network;
use crate::output;
use crate::prompt;
use crate::quantity::Scale;
use crate::signing::SelectedSigner;

/// What the user is asked to approve. It names the error a decline fails with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConsentKind {
    Order,
    Tpsl,
    ScaleOrders,
    BatchOrders,
    Twap,
    ScheduleCancel,
    TrailingStop,
    ModifyOrders,
    CancelAll,
}

impl ConsentKind {
    pub(super) fn declined(self) -> CliError {
        let (label, cancelled) = match self {
            Self::Order => ("order", "order placement"),
            Self::Tpsl => ("TP/SL", "order placement"),
            Self::ScaleOrders => ("scale order", "order placement"),
            Self::BatchOrders => ("batch order", "order placement"),
            Self::Twap => ("TWAP", "TWAP creation"),
            Self::ScheduleCancel => ("schedule-cancel", "action"),
            Self::TrailingStop => ("trailing stop", "trailing stop placement"),
            Self::ModifyOrders => ("batch modify", "order modification"),
            Self::CancelAll => {
                return CliError::Configuration("cancel-all aborted".to_string());
            }
        };
        CliError::Configuration(format!(
            "Mainnet {label} confirmation required; {cancelled} cancelled. Rerun with --yes for deliberate automation."
        ))
    }
}

pub(super) fn confirm_mainnet_batch_modify(
    count: usize,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let orders = output::format::noun(count, "order", "orders");
    ask_confirmation(
        target,
        &[format!("Modifications: {count} {orders}")],
        &format!("Modify {count} {orders} on {}?", target.network()),
    )
}

pub(super) fn confirm_cancel_all(
    coin: Option<&str>,
    count: usize,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let scope = coin.map(|coin| format!("{coin} ")).unwrap_or_default();
    let question = format!(
        "Cancel {count} open {scope}{} on {}?",
        output::format::noun(count, "order", "orders"),
        target.network()
    );
    ask_confirmation(target, &[], &question)
}

pub(super) fn confirm_mainnet_schedule_cancel(
    plan: &ScheduleCancelPlan,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let question = if let Some(scheduled) = plan.scheduled(Utc::now()) {
        format!(
            "Schedule cancel-all for {} on {}?",
            scheduled.at.to_rfc3339(),
            target.network()
        )
    } else {
        format!("Clear scheduled cancel-all on {}?", target.network())
    };
    ask_confirmation(target, &[], &question)
}

pub(super) fn confirm_mainnet_order(
    prepared: &PreparedOrder,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let amount = prepared
        .amount
        .as_ref()
        .map(|QuoteAmount { value, unit }| format!(", amount {value} {unit}"))
        .unwrap_or_default();
    let trigger = prepared
        .trigger_price
        .map(|trigger_price| format!(", trigger price {trigger_price}"))
        .unwrap_or_default();
    let reduce_only = if prepared.request.reduce_only {
        "reduce-only"
    } else {
        "not reduce-only"
    };
    let mut details = vec![format!(
        "Order: {} {} order for {} {} at price {}{}{}, {}",
        prepared.side,
        prepared.order_type,
        prepared.size(),
        prepared.coin,
        prepared.price(),
        trigger,
        amount,
        reduce_only
    )];
    if let Some(slippage_warning) = &prepared.warning {
        details.push(prompt::warning(slippage_warning));
    }
    if let Some(rate) = prepared.priority_rate {
        details.push(priority_line(rate));
    }
    ask_confirmation(
        target,
        &details,
        &format!("Place this order on {}?", target.network()),
    )
}

/// The consent line for an order write priority: a real charge, burned, from
/// staking balance rather than from the trading account.
fn priority_line(rate: u32) -> String {
    let bps = (Decimal::from(rate) / Decimal::from(10_000)).normalize();
    format!(
        "Priority: {rate} ({bps} bps of the filled notional for IOC, resting notional for ALO), charged from undelegated staking balance"
    )
}

pub(super) fn confirm_mainnet_order_batch(
    label: &str,
    prepared: &PreparedOrderBatch,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let count = prepared.legs.len();
    let orders = output::format::noun(count, "order", "orders");
    let mut details = vec![format!("Orders: {count} {label} {orders}")];
    if let OrderGrouping::PriorityRate(rate) = prepared.batch.grouping {
        details.push(priority_line(rate));
    }
    ask_confirmation(
        target,
        &details,
        &format!("Submit {count} {label} {orders} on {}?", target.network()),
    )
}

pub(super) fn confirm_mainnet_tpsl_batch(
    prepared: &PreparedTpslBatch,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let parent = prepared
        .legs
        .iter()
        .find(|leg| leg.leg == TpslLegKind::Parent);
    let summary = if let Some(parent) = parent {
        let children = prepared.legs.len().saturating_sub(1);
        format!(
            "Orders: {} parent order for {} {} at price {} with {children} grouped child {} using {}",
            parent.side,
            parent.size,
            parent.coin,
            parent.price,
            output::format::noun(children, "order", "orders"),
            prepared.grouping
        )
    } else {
        let first = prepared.legs.first().ok_or_else(|| {
            CliError::Configuration("TP/SL batch requires at least one leg".to_string())
        })?;
        format!(
            "Orders: {} grouped TP/SL {} for {} using {}",
            prepared.legs.len(),
            output::format::noun(prepared.legs.len(), "order", "orders"),
            first.coin,
            prepared.grouping
        )
    };
    let mut details = vec![summary];
    for leg in &prepared.legs {
        if let Some(warning) = &leg.warning {
            details.push(prompt::warning(warning));
        }
    }
    ask_confirmation(
        target,
        &details,
        &format!("Place this TP/SL batch on {}?", target.network()),
    )
}

pub(super) fn confirm_mainnet_twap(
    plan: &TwapCreatePlan,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let details = [format!(
        "TWAP: {} {} {} over {} seconds",
        plan.side, plan.size, plan.coin, plan.duration_seconds
    )];
    ask_confirmation(
        target,
        &details,
        &format!("Create this TWAP on {}?", target.network()),
    )
}

pub(super) struct ConsentTarget<'a> {
    mode: &'a Mode,
    network: Network,
    signer: &'a SelectedSigner,
    vault_address: Option<Address>,
    builder: Option<&'a OrderBuilderFee>,
}

impl<'a> ConsentTarget<'a> {
    pub(super) fn new(
        submission: OrderSubmissionContext<'a>,
        builder: Option<&'a OrderBuilderFee>,
    ) -> Self {
        Self {
            mode: submission.inv.mode(),
            network: Network::from(submission.api.chain()),
            signer: submission.signer,
            vault_address: submission.vault_address,
            builder,
        }
    }

    pub(super) fn network(&self) -> Network {
        self.network
    }

    fn preview_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Network: {}", self.network()),
            format!("Signer: {}", self.signer.address()),
        ];
        if let Some(vault) = self.vault_address {
            lines.push(format!("Acting as: {vault} (vault or subaccount)"));
        }
        if let Some(builder) = self.builder {
            lines.push(format!(
                "Builder: {} (max fee {}%)",
                builder.b,
                builder.percent()
            ));
            for sub in &builder.s {
                lines.push(format!(
                    "Sub-builder: {} ({}% of the builder fee)",
                    sub.b,
                    Scale::TENTHS_BPS_PER_PERCENT.amount(sub.s)
                ));
            }
        }
        lines
    }
}

/// Print the consent preview block on stderr, then ask `question`.
///
/// Runs that can't prompt fail with `--yes` as the way to answer before the
/// block is printed.
pub(super) fn ask_confirmation(
    target: &ConsentTarget<'_>,
    details: &[String],
    question: &str,
) -> Result<bool, CliError> {
    prompt::ensure_can_prompt(target.mode, "--yes")?;
    let mut stderr = io::stderr();
    for line in target.preview_lines().iter().chain(details) {
        writeln!(stderr, "{line}").map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
    }
    prompt::confirm_warning(target.mode, question, "--yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declined_approval_names_what_was_cancelled() {
        for (kind, message) in [
            (
                ConsentKind::Order,
                "Mainnet order confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                ConsentKind::Tpsl,
                "Mainnet TP/SL confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                ConsentKind::ScaleOrders,
                "Mainnet scale order confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                ConsentKind::BatchOrders,
                "Mainnet batch order confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                ConsentKind::Twap,
                "Mainnet TWAP confirmation required; TWAP creation cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                ConsentKind::ScheduleCancel,
                "Mainnet schedule-cancel confirmation required; action cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                ConsentKind::TrailingStop,
                "Mainnet trailing stop confirmation required; trailing stop placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                ConsentKind::ModifyOrders,
                "Mainnet batch modify confirmation required; order modification cancelled. Rerun with --yes for deliberate automation.",
            ),
            (ConsentKind::CancelAll, "cancel-all aborted"),
        ] {
            let err = kind.declined();
            assert_eq!(err.exit_code(), 2, "{kind:?}");
            assert_eq!(
                err.to_string(),
                format!("Configuration error: {message}"),
                "{kind:?}"
            );
        }
    }
}
