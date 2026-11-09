use std::io::{self, Write};

use chrono::Utc;
use hypersdk::{Address, Decimal};

use super::{
    OrderBuilderFee, OrderSubmissionContext, PreparedOrder, PreparedOrderBatch, PreparedTpslBatch,
    TpslLegKind, TwapCreatePlan, planning::ScheduleCancelPlan,
};
use crate::errors::CliError;
use crate::mode::Mode;
use crate::network::Network;
use crate::output;
use crate::prompt;
use crate::signing::SelectedSigner;

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
        .map(|amount| format!(", amount {amount} {}", prepared.amount_unit))
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
    ask_confirmation(
        target,
        &details,
        &format!("Place this order on {}?", target.network()),
    )
}

pub(super) fn confirm_mainnet_order_batch(
    label: &str,
    prepared: &PreparedOrderBatch,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let count = prepared.legs.len();
    let orders = output::format::noun(count, "order", "orders");
    ask_confirmation(
        target,
        &[format!("Orders: {count} {label} {orders}")],
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
        vault_address: Option<Address>,
        builder: Option<&'a OrderBuilderFee>,
    ) -> Self {
        Self {
            mode: submission.inv.mode(),
            network: Network::from(submission.api.chain()),
            signer: submission.signer,
            vault_address,
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
                Decimal::new(i64::try_from(builder.f).unwrap_or(i64::MAX), 3).normalize()
            ));
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
