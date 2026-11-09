use hypersdk::hypercore::types::OrderGrouping;
use hypersdk::{Address, Decimal};

use super::planning::{ScheduleCancelPlan, TrailingStopPlan, TwapCreatePlan};
use super::prepared::{
    PreparedOrder, PreparedOrderBatch, PreparedTpslBatch, QuoteAmount, TpslLegKind,
};
use crate::builder_fee::BuilderFee;
use crate::clock;
use crate::command_spec::YES_FLAG;
use crate::consent::{self as policy, ConsentBlock, Tone};
use crate::errors::CliError;
use crate::mode::Mode;
use crate::network::Network;
use crate::output;
use crate::signing::SelectedSigner;

/// What a command asks approval for. It is the one place a request pairs the
/// block the person is shown with the error a decline fails with, so neither
/// can name another request's.
pub(super) enum Consent<'a> {
    Order(&'a PreparedOrder),
    Tpsl(&'a PreparedTpslBatch),
    Scale(&'a PreparedOrderBatch),
    BatchCreate(&'a PreparedOrderBatch),
    Twap(&'a TwapCreatePlan),
    ScheduleCancel(&'a ScheduleCancelPlan),
    TrailingStop(&'a TrailingStopPlan),
    ModifyOrders { count: usize },
    CancelAll { coin: Option<&'a str>, count: usize },
}

impl Consent<'_> {
    /// The block and question shown to the person; whether they approved.
    pub(super) fn ask(&self, target: &ConsentTarget<'_>) -> Result<bool, CliError> {
        match self {
            Self::Order(prepared) => confirm_order(prepared, target),
            Self::Tpsl(batch) => confirm_tpsl_batch(batch, target),
            Self::Scale(batch) => confirm_order_batch("scale", batch, target),
            Self::BatchCreate(batch) => confirm_order_batch("batch-create", batch, target),
            Self::Twap(plan) => confirm_twap(plan, target),
            Self::ScheduleCancel(plan) => confirm_schedule_cancel(plan, target),
            Self::TrailingStop(plan) => confirm_trailing_stop(plan, target),
            Self::ModifyOrders { count } => confirm_batch_modify(*count, target),
            Self::CancelAll { coin, count } => confirm_cancel_all(*coin, *count, target),
        }
    }

    /// The error a decline fails with.
    pub(super) fn declined(&self) -> CliError {
        let (label, cancelled) = match self {
            Self::Order(_) => ("order", "order placement"),
            Self::Tpsl(_) => ("TP/SL", "order placement"),
            Self::Scale(_) => ("scale order", "order placement"),
            Self::BatchCreate(_) => ("batch order", "order placement"),
            Self::Twap(_) => ("TWAP", "TWAP creation"),
            Self::ScheduleCancel(_) => ("schedule-cancel", "action"),
            Self::TrailingStop(_) => ("trailing stop", "trailing stop placement"),
            Self::ModifyOrders { .. } => ("batch modify", "order modification"),
            Self::CancelAll { .. } => return policy::declined("cancel-all aborted"),
        };
        policy::declined(format!(
            "Mainnet {label} confirmation required; {cancelled} cancelled. Rerun with {YES_FLAG} for deliberate automation."
        ))
    }
}

fn confirm_batch_modify(count: usize, target: &ConsentTarget<'_>) -> Result<bool, CliError> {
    let orders = output::format::noun(count, "order", "orders");
    ask_confirmation(
        target,
        &[format!("Modifications: {count} {orders}")],
        &format!("Modify {count} {orders} on {}?", target.network()),
    )
}

fn confirm_cancel_all(
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

fn confirm_schedule_cancel(
    plan: &ScheduleCancelPlan,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let question = if let Some(scheduled) = plan.scheduled(clock::now_utc()) {
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

fn confirm_order(prepared: &PreparedOrder, target: &ConsentTarget<'_>) -> Result<bool, CliError> {
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
        details.push(output::notice::caution(slippage_warning));
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

fn confirm_order_batch(
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

fn confirm_tpsl_batch(
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
            details.push(output::notice::caution(warning));
        }
    }
    ask_confirmation(
        target,
        &details,
        &format!("Place this TP/SL batch on {}?", target.network()),
    )
}

fn confirm_twap(plan: &TwapCreatePlan, target: &ConsentTarget<'_>) -> Result<bool, CliError> {
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

fn confirm_trailing_stop(
    plan: &TrailingStopPlan,
    target: &ConsentTarget<'_>,
) -> Result<bool, CliError> {
    let tracking = if plan.side().is_buy() {
        "lowest"
    } else {
        "highest"
    };
    let activation = plan
        .activation_px()
        .map(|price| format!("Activation: {} (mark price)", price.normalize()))
        .unwrap_or_else(|| "Activation: starts tracking immediately".to_string());
    let details = [
        format!("Trailing stop: {} market order when it fires", plan.side()),
        format!("Coin: {}", plan.coin()),
        format!("Side: {}", plan.side()),
        format!("Size: {}", plan.size().normalize()),
        format!(
            "Retracement: {} from the {tracking} mark price",
            plan.retracement_label()
        ),
        activation,
        format!(
            "Reduce-only: {}",
            if plan.reduce_only() { "yes" } else { "no" }
        ),
    ];
    ask_confirmation(
        target,
        &details,
        &format!("Place this trailing stop on {}?", target.network()),
    )
}

/// The network, the signer, the account it acts for and the builder fee that the
/// approval block shows.
pub(super) struct ConsentTarget<'a> {
    mode: &'a Mode,
    network: Network,
    signer: &'a SelectedSigner,
    vault_address: Option<Address>,
    builder: Option<&'a BuilderFee>,
}

impl<'a> ConsentTarget<'a> {
    pub(super) fn new(
        mode: &'a Mode,
        network: Network,
        signer: &'a SelectedSigner,
        vault_address: Option<Address>,
        builder: Option<&'a BuilderFee>,
    ) -> Self {
        Self {
            mode,
            network,
            signer,
            vault_address,
            builder,
        }
    }

    pub(super) fn network(&self) -> Network {
        self.network
    }

    fn block(&self, details: &[String]) -> ConsentBlock {
        let mut block = ConsentBlock::signed(self.network(), self.signer.address());
        if let Some(vault) = self.vault_address {
            block = block.field("Acting as", format!("{vault} (vault or subaccount)"));
        }
        if let Some(builder) = self.builder {
            block = block.field(
                "Builder",
                format!(
                    "{} (max fee {}%)",
                    builder.address_wire(),
                    builder.percent()
                ),
            );
            for sub in builder.subs() {
                block = block.field(
                    "Sub-builder",
                    format!(
                        "{} ({}% of the builder fee)",
                        sub.address_wire(),
                        sub.percent()
                    ),
                );
            }
        }
        block.texts(details)
    }
}

/// Runs that can't prompt fail with `--yes` as the way to answer before the block
/// is printed.
fn ask_confirmation(
    target: &ConsentTarget<'_>,
    details: &[String],
    question: &str,
) -> Result<bool, CliError> {
    policy::ask(target.mode, &target.block(details), question, Tone::Warning)
}

#[cfg(test)]
mod tests {
    use hypersdk::hypercore::types::{
        Action, OrderGrouping, OrderRequest, OrderTypePlacement, TimeInForce, TwapOrderParams,
    };

    use super::*;
    use crate::assets::test_support::perp;
    use crate::assets::{Market, PerpAsset};
    use crate::commands::orders::args::TrailingStopArgs;
    use crate::commands::orders::planning::prepare_trailing_stop_plan;
    use crate::commands::orders::prepared::order_batch;
    use crate::commands::orders::vocab::{CreateOrderType, OrderSide, TpslGroupingArg};
    use crate::margin::MarginPlan;

    fn order() -> PreparedOrder {
        let perp = perp("BTC", 0);
        PreparedOrder {
            request: OrderRequest {
                asset: perp.index,
                is_buy: true,
                limit_px: Decimal::from(10),
                sz: Decimal::ONE,
                reduce_only: false,
                order_type: OrderTypePlacement::Limit {
                    tif: TimeInForce::Gtc,
                },
                cloid: Default::default(),
            },
            coin: perp.display_name(),
            margin: None,
            market: Market::Perp(perp),
            side: OrderSide::Buy,
            order_type: CreateOrderType::Limit,
            tif: None,
            trigger_price: None,
            amount: None,
            warning: None,
            builder: None,
            priority_rate: None,
        }
    }

    fn twap(perp: PerpAsset) -> TwapCreatePlan {
        TwapCreatePlan {
            action: Action::TwapOrder {
                twap: TwapOrderParams {
                    a: perp.index,
                    b: true,
                    s: Decimal::ONE,
                    r: false,
                    m: 5,
                    t: false,
                },
            },
            coin: perp.display_name(),
            side: "buy".to_string(),
            size: Decimal::ONE,
            duration_seconds: 300,
            duration_minutes: 5,
            randomize: false,
            margin: MarginPlan::for_market(&perp, None).unwrap(),
            perp,
        }
    }

    #[test]
    fn a_declined_approval_names_what_was_cancelled() {
        let order = order();
        let tpsl = PreparedTpslBatch {
            batch: order_batch(Vec::new(), OrderGrouping::Na),
            grouping: TpslGroupingArg::NormalTpsl,
            legs: Vec::new(),
        };
        let batch = PreparedOrderBatch {
            batch: order_batch(Vec::new(), OrderGrouping::Na),
            legs: Vec::new(),
        };
        let twap = twap(perp("BTC", 0));
        let schedule = ScheduleCancelPlan::Clear;
        let trailing = prepare_trailing_stop_plan(
            &perp("BTC", 0),
            &TrailingStopArgs {
                coin: "BTC".to_string(),
                dex: None,
                side: OrderSide::Sell,
                size: Decimal::ONE,
                retracement_pct: Some(Decimal::from(5)),
                retracement_px: None,
                activation_price: None,
                reduce_only: false,
                on_behalf_of: None,
                yes: false,
            },
        )
        .unwrap();
        for (consent, message) in [
            (
                Consent::Order(&order),
                "Mainnet order confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::Tpsl(&tpsl),
                "Mainnet TP/SL confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::Scale(&batch),
                "Mainnet scale order confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::BatchCreate(&batch),
                "Mainnet batch order confirmation required; order placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::Twap(&twap),
                "Mainnet TWAP confirmation required; TWAP creation cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::ScheduleCancel(&schedule),
                "Mainnet schedule-cancel confirmation required; action cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::TrailingStop(&trailing),
                "Mainnet trailing stop confirmation required; trailing stop placement cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::ModifyOrders { count: 2 },
                "Mainnet batch modify confirmation required; order modification cancelled. Rerun with --yes for deliberate automation.",
            ),
            (
                Consent::CancelAll {
                    coin: None,
                    count: 2,
                },
                "cancel-all aborted",
            ),
        ] {
            let err = consent.declined();
            assert_eq!(err.exit_code(), 2, "{message}");
            assert_eq!(
                err.to_string(),
                format!("Configuration error: {message}"),
                "{message}"
            );
        }
    }
}
