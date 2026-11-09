use std::fmt;
use std::io::{self, Write};

use chrono::Utc;
use either::Either;
use hypersdk::hypercore::types::{
    Action, BasicOrder, BatchOrder, OrderGrouping, OrderRequest, OrderResponseStatus, OrderType,
    OrderTypePlacement, ScheduleCancel, TimeInForce, TpSl,
};
use hypersdk::hypercore::{Cloid, HttpClient, OidOrCloid};
use hypersdk::{Address, Decimal};
use serde::Serialize;

use crate::assets::asset_id::{
    outcome_coin, parse_spot_coin_key, spot_coin_key, spot_internal_coin_key,
};
use crate::assets::{
    AssetQuery, AssetResolver, Market, PerpAsset, parse_asset_query, qualify_dex_asset,
};
use crate::commands::actions;
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::{self, TableData};
use crate::prompt;
use crate::signing::SelectedSigner;

const DEFAULT_MARKET_ORDER_SLIPPAGE_BPS: u16 = 500;
const MIN_MARKET_ORDER_SLIPPAGE_BPS: u16 = 1;
const MAX_MARKET_ORDER_SLIPPAGE_BPS: u16 = 1_000;
const MAX_BATCH_ORDER_COUNT: usize = 500;
const BPS_DENOMINATOR: i64 = 10_000;

mod args;
mod bracket;
mod chase;
pub mod cli;
mod consent;
mod exchange;
mod hip3;
mod live_order;
mod planning;
mod queries;
mod rendering;
mod trailing_stop;
mod validation;

use args::*;
use consent::{
    ConsentTarget, ask_confirmation, confirm_cancel_all, confirm_mainnet_order,
    confirm_mainnet_order_batch, confirm_mainnet_schedule_cancel, confirm_mainnet_tpsl_batch,
    confirm_mainnet_twap,
};
use exchange::{
    Rejected, ScheduleCancelRawAction, ensure_action_statuses_ok, modification_result_oid,
    order_statuses_from_response, parse_twap_cancel_response, parse_twap_create_response,
    place_builder_order_batch_raw, place_order_batch_raw, rejected_all,
};
use hip3::{Hip3MarginPlan, execute_hip3_margin_transfer, plan_hip3_margin_transfer};
use planning::{
    CreateOrderSubmission, ScheduleCancelPlan, ScheduledCancel, TwapCancelPlan, TwapCreatePlan,
    prepare_cancel_all_orders_plan, prepare_cancel_order_plan, prepare_modify_order_plan,
    prepare_position_tpsl_batch,
};
pub use rendering::OrderListOutput;
use rendering::{
    CancelAllSummary, CancelAllSummaryOutput, CancelConfirmation, CancelConfirmationOutput,
    CancelLeg, CancelLegsOutput, ModifyConfirmationOutput, OrderConfirmation,
    OrderConfirmationOutput, PartialBatchOutput, ScheduleCancelConfirmation, ScheduleCancelOutput,
    TpslOrderConfirmation, TpslOrderConfirmationOutput, TwapCancelConfirmation, TwapCancelOutput,
    TwapCreateConfirmation, TwapCreateOutput,
};
use validation::{MarketPrecision, TpslSpec};

#[derive(Debug, Clone)]
struct PreparedOrder {
    request: OrderRequest,
    market: Market,
    precision: MarketPrecision,
    coin: String,
    side: OrderSide,
    order_type: CreateOrderType,
    tif: Option<TifArg>,
    trigger_price: Option<Decimal>,
    amount: Option<Decimal>,
    amount_unit: String,
    warning: Option<String>,
    builder: Option<OrderBuilderFee>,
    /// The margin mode the account's position must already use.
    margin_mode: Option<MarginModeArg>,
}

impl PreparedOrder {
    fn perp(&self) -> Option<&PerpAsset> {
        match &self.market {
            Market::Perp(perp) => Some(perp),
            Market::Spot(_) | Market::Outcome(_) => None,
        }
    }

    /// The HIP-3 dex whose margin this order draws on.
    fn hip3_dex(&self) -> Option<&str> {
        self.perp().and_then(|perp| perp.dex.as_deref())
    }

    fn price(&self) -> Decimal {
        self.request.limit_px
    }

    fn size(&self) -> Decimal {
        self.request.sz
    }

    fn notional(&self) -> Decimal {
        self.size() * self.price()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct OrderBuilderFee {
    b: String,
    f: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TpslLegKind {
    Parent,
    TakeProfit,
    StopLoss,
}

impl TpslLegKind {
    fn of(tpsl: TpSl) -> Self {
        match tpsl {
            TpSl::Tp => Self::TakeProfit,
            TpSl::Sl => Self::StopLoss,
        }
    }
}

impl fmt::Display for TpslLegKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parent => write!(f, "parent"),
            Self::TakeProfit => write!(f, "take_profit"),
            Self::StopLoss => write!(f, "stop_loss"),
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedTpslLeg {
    request: OrderRequest,
    leg: TpslLegKind,
    coin: String,
    side: OrderSide,
    order_type: CreateOrderType,
    price: Decimal,
    size: Decimal,
    tif: Option<TifArg>,
    reduce_only: bool,
    warning: Option<String>,
}

#[derive(Debug, Clone)]
struct PreparedTpslBatch {
    batch: BatchOrder,
    grouping: TpslGroupingArg,
    legs: Vec<PreparedTpslLeg>,
}

#[derive(Debug, Clone)]
struct PreparedOrderLeg {
    leg_index: usize,
    order: PreparedOrder,
}

#[derive(Debug, Clone)]
struct PreparedOrderBatch {
    batch: BatchOrder,
    legs: Vec<PreparedOrderLeg>,
}

#[derive(Debug, Clone)]
enum OrderIdentifier {
    Oid(u64),
    Cloid { raw: String, parsed: Cloid },
}

impl OrderIdentifier {
    fn to_status_identifier(&self) -> OidOrCloid {
        match self {
            Self::Oid(oid) => Either::Left(*oid),
            Self::Cloid { parsed, .. } => Either::Right(*parsed),
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Oid(oid) => oid.to_string(),
            Self::Cloid { raw, .. } => raw.clone(),
        }
    }
}

/// The client order id of an order that has none: the exchange's all-zero id.
/// This is the only place that sentinel is written or read.
fn wire_cloid(cloid: Option<Cloid>) -> Cloid {
    cloid.unwrap_or_default()
}

fn cloid_from_wire(cloid: Cloid) -> Option<Cloid> {
    (cloid != Cloid::default()).then_some(cloid)
}

#[derive(Clone, Copy)]
pub struct OrderExecutionContext<'a> {
    pub submission: OrderSubmissionContext<'a>,
    pub resolver: &'a AssetResolver,
}

#[derive(Clone, Copy)]
pub struct OrderSubmissionContext<'a> {
    pub inv: &'a Invocation,
    pub api: &'a ApiClient,
    pub signer: &'a SelectedSigner,
    /// The user must approve this action: it is on mainnet and `--yes` was not given.
    pub require_mainnet_confirmation: bool,
}

impl<'a> OrderSubmissionContext<'a> {
    /// Ask the user to approve an action on mainnet before it is sent. A no-op
    /// on testnet or with `--yes`. `label` and `cancelled` complete the error
    /// when the user declines: "Mainnet {label} confirmation required;
    /// {cancelled} cancelled."
    fn consent(
        self,
        vault_address: Option<Address>,
        builder: Option<&OrderBuilderFee>,
        label: &str,
        cancelled: &str,
        ask: impl FnOnce(&ConsentTarget<'_>) -> Result<bool, CliError>,
    ) -> Result<(), CliError> {
        if !self.require_mainnet_confirmation {
            return Ok(());
        }
        let target = ConsentTarget::new(self, vault_address, builder);
        if ask(&target)? {
            return Ok(());
        }
        Err(CliError::Configuration(format!(
            "Mainnet {label} confirmation required; {cancelled} cancelled. Rerun with --yes for deliberate automation."
        )))
    }

    fn account(self, vault_address: Option<Address>) -> Address {
        vault_address.unwrap_or_else(|| self.signer.query_address())
    }
}

async fn create(
    context: OrderExecutionContext<'_>,
    submission: CreateOrderSubmission,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let parent = submission.parent();
    verify_margin_mode(
        context.submission,
        vault_address,
        parent.perp(),
        parent.margin_mode,
    )
    .await?;
    match submission {
        CreateOrderSubmission::NormalTpsl(order) => {
            let user = context.submission.account(vault_address);
            let helper = plan_hip3_helper(context, user, &order.parent).await?;
            context.submission.consent(
                vault_address,
                None,
                "TP/SL",
                "order placement",
                |target| {
                    warn_hip3_margin_helper(&helper)?;
                    confirm_mainnet_tpsl_batch(&order.batch, target)
                },
            )?;
            execute_hip3_helper(context, user, &helper).await?;

            let statuses = place_order_batch_raw(
                context.submission.api,
                context.submission.signer,
                order.batch.batch.clone(),
                vault_address,
            )
            .await?;
            let rows = tpsl_confirmation_rows(&order.batch, statuses)?;
            report_tpsl_batch(TpslOrderConfirmationOutput { rows }, context.submission.inv)
        }
        CreateOrderSubmission::Single(prepared) => {
            let statuses = place_prepared_single(context, &prepared, vault_address).await?;
            let rows = statuses
                .into_iter()
                .map(|status| OrderConfirmation::from_status(&prepared, status))
                .collect::<Result<Vec<_>, _>>()?;
            context
                .submission
                .inv
                .print(&OrderConfirmationOutput { rows });
            Ok(())
        }
    }
}

async fn plan_hip3_helper(
    context: OrderExecutionContext<'_>,
    user: Address,
    order: &PreparedOrder,
) -> Result<Option<Hip3MarginPlan>, anyhow::Error> {
    plan_hip3_margin_transfer(
        context.submission.api,
        user,
        order.hip3_dex(),
        order.notional(),
    )
    .await
}

fn warn_hip3_margin_helper(helper: &Option<Hip3MarginPlan>) -> Result<(), CliError> {
    if let Some(plan) = helper {
        let mut stderr = io::stderr();
        writeln!(
            stderr,
            "{}",
            prompt::warning(&format!(
                "This order requires a helper transfer of {} USDC from perp to dex:{} before placement.",
                plan.amount, plan.dest_dex
            ))
        )
        .map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
    }
    Ok(())
}

async fn execute_hip3_helper(
    context: OrderExecutionContext<'_>,
    user: Address,
    helper: &Option<Hip3MarginPlan>,
) -> Result<(), anyhow::Error> {
    if let Some(plan) = helper {
        execute_hip3_margin_transfer(
            context.submission.inv.mode(),
            context.submission.inv.remote()?,
            context.submission.signer,
            user,
            plan,
        )
        .await?;
    }
    Ok(())
}

async fn place_prepared_single(
    context: OrderExecutionContext<'_>,
    prepared: &PreparedOrder,
    vault_address: Option<Address>,
) -> Result<Vec<OrderResponseStatus>, anyhow::Error> {
    let user = context.submission.account(vault_address);
    let helper = plan_hip3_helper(context, user, prepared).await?;
    context.submission.consent(
        vault_address,
        prepared.builder.as_ref(),
        "order",
        "order placement",
        |target| {
            warn_hip3_margin_helper(&helper)?;
            confirm_mainnet_order(prepared, target)
        },
    )?;
    execute_hip3_helper(context, user, &helper).await?;

    let batch = BatchOrder {
        orders: vec![prepared.request.clone()],
        grouping: OrderGrouping::Na,
        // The CLI sends builder fees through `place_builder_order_batch_raw`, which
        // serializes its own `{ b, f }` object; the SDK field stays unset.
        builder: None,
    };
    let statuses = if let Some(builder) = prepared.builder.clone() {
        place_builder_order_batch_raw(
            context.submission.api,
            context.submission.signer,
            batch,
            builder,
            vault_address,
        )
        .await?
    } else {
        place_order_batch_raw(
            context.submission.api,
            context.submission.signer,
            batch,
            vault_address,
        )
        .await?
    };
    Ok(statuses)
}

async fn scale(
    context: OrderExecutionContext<'_>,
    prepared: PreparedOrderBatch,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    if let Some(first) = prepared.legs.first() {
        verify_margin_mode(
            context.submission,
            vault_address,
            first.order.perp(),
            first.order.margin_mode,
        )
        .await?;
    }
    context.submission.consent(
        vault_address,
        None,
        "scale order",
        "order placement",
        |target| confirm_mainnet_order_batch("scale", &prepared, target),
    )?;
    let rows = place_order_batch(context, prepared, vault_address).await?;
    finish_order_batch(rows, context.submission.inv)
}

async fn batch_create(
    context: OrderExecutionContext<'_>,
    prepared: PreparedOrderBatch,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    context.submission.consent(
        vault_address,
        None,
        "batch order",
        "order placement",
        |target| confirm_mainnet_order_batch("batch-create", &prepared, target),
    )?;
    let rows = place_order_batch(context, prepared, vault_address).await?;
    finish_order_batch(rows, context.submission.inv)
}

async fn place_order_batch(
    context: OrderExecutionContext<'_>,
    prepared: PreparedOrderBatch,
    vault_address: Option<Address>,
) -> Result<Vec<OrderConfirmation>, CliError> {
    let statuses = place_order_batch_raw(
        context.submission.api,
        context.submission.signer,
        prepared.batch.clone(),
        vault_address,
    )
    .await?;

    if statuses.len() != prepared.legs.len() {
        return Err(status_count_mismatch(statuses.len(), prepared.legs.len()));
    }

    Ok(prepared
        .legs
        .iter()
        .zip(statuses)
        .map(|(leg, status)| OrderConfirmation::from_leg_status(&leg.order, status))
        .collect())
}

/// Place position-attached TP/SL orders and return each leg's result without
/// printing anything.
async fn place_position_tpsl(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
    vault_address: Option<Address>,
) -> Result<TpslOrderConfirmationOutput, anyhow::Error> {
    verify_margin_mode(context, vault_address, Some(perp), spec.margin_mode).await?;
    let user = context.account(vault_address);
    let prepared = prepare_position_tpsl_batch(context.api.sdk(), perp, user, spec).await?;
    context.consent(vault_address, None, "TP/SL", "order placement", |target| {
        confirm_mainnet_tpsl_batch(&prepared, target)
    })?;

    let statuses = place_order_batch_raw(
        context.api,
        context.signer,
        prepared.batch.clone(),
        vault_address,
    )
    .await?;
    Ok(TpslOrderConfirmationOutput {
        rows: tpsl_confirmation_rows(&prepared, statuses)?,
    })
}

async fn tpsl(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let legs = place_position_tpsl(context, perp, spec, vault_address).await?;
    report_tpsl_batch(legs, context.inv)
}

/// Place the TP/SL that protects a position opened by another command. Prints
/// nothing: a rejected leg is returned as the error for the caller to report.
async fn protect_position(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let legs = place_position_tpsl(context, perp, spec, vault_address).await?;
    let failures = legs.rejection_errors();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(rejected_all("place TP/SL orders", &failures).into())
    }
}

/// Fail before placing an order when `--margin-mode` does not match the
/// account's open position on `perp`. Orders without a margin mode, and on
/// markets that have none, are not checked.
async fn verify_margin_mode(
    context: OrderSubmissionContext<'_>,
    vault_address: Option<Address>,
    perp: Option<&PerpAsset>,
    margin_mode: Option<MarginModeArg>,
) -> Result<(), anyhow::Error> {
    let (Some(perp), Some(margin_mode)) = (perp, margin_mode) else {
        return Ok(());
    };
    let user = context.account(vault_address);
    let coin = perp.name.as_str();
    let state = context
        .api
        .sdk()
        .clearinghouse_state(user, perp.dex.clone())
        .await?;
    let Some(position) = state
        .asset_positions
        .into_iter()
        .find(|position| position.position.coin.eq_ignore_ascii_case(coin))
    else {
        if matches!(margin_mode, MarginModeArg::Cross) {
            return Ok(());
        }
        return Err(CliError::Configuration(format!(
            "orders with --margin-mode {margin_mode} require an open {coin} perp position so the mode can be verified; establish it first with `{}`",
            update_leverage_command(coin, margin_mode)
        ))
        .into());
    };

    let actual_margin_mode = match position.position.leverage.leverage_type {
        hypersdk::hypercore::types::LeverageType::Cross => MarginModeArg::Cross,
        hypersdk::hypercore::types::LeverageType::Isolated => MarginModeArg::Isolated,
    };
    if actual_margin_mode != margin_mode {
        return Err(CliError::Configuration(format!(
            "orders requested --margin-mode {margin_mode} but the current {coin} perp position is {actual_margin_mode}; use `{}` first",
            update_leverage_command(coin, margin_mode)
        ))
        .into());
    }

    Ok(())
}

fn update_leverage_command(coin: &str, margin_mode: MarginModeArg) -> String {
    let isolated = if margin_mode == MarginModeArg::Isolated {
        " --isolated"
    } else {
        ""
    };
    format!("positions update-leverage --coin {coin} --leverage <N>{isolated}")
}

/// Report a batch where some legs failed. Prints the per-leg result, then
/// returns the partial-results error (exit 15). When every leg failed there is
/// no partial result, so the rejection is the error.
fn finish_with_failures(
    legs: impl TableData + 'static,
    verb: &str,
    action: &str,
    failures: Vec<Rejected>,
    inv: &Invocation,
) -> Result<(), anyhow::Error> {
    let total = legs.rows().len();
    let failed = failures.len();
    if failed >= total {
        return Err(rejected_all(action, &failures).into());
    }
    let succeeded = total - failed;
    let partial = PartialBatchOutput {
        legs: Box::new(legs),
        legs_key: "orders",
        succeeded,
        failed,
        headline: format!(
            "{verb} {succeeded} of {total} {}. {failed} failed.",
            output::format::noun(total, "order", "orders")
        ),
    };
    inv.print(&partial);
    Err(partial.error().into())
}

fn finish_order_batch(rows: Vec<OrderConfirmation>, inv: &Invocation) -> Result<(), anyhow::Error> {
    let legs = OrderConfirmationOutput { rows };
    let failures = legs.rejection_errors();
    if failures.is_empty() {
        inv.print(&legs);
        return Ok(());
    }
    finish_with_failures(legs, "Placed", "place orders", failures, inv)
}

fn report_tpsl_batch(
    legs: TpslOrderConfirmationOutput,
    inv: &Invocation,
) -> Result<(), anyhow::Error> {
    let failures = legs.rejection_errors();
    if failures.is_empty() {
        inv.print(&legs);
        return Ok(());
    }
    finish_with_failures(legs, "Placed", "place TP/SL orders", failures, inv)
}

async fn cancel(
    context: OrderExecutionContext<'_>,
    user: Address,
    args: &CancelArgs,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let plan =
        prepare_cancel_order_plan(context.submission.api.sdk(), context.resolver, user, args)
            .await?;
    let response = actions::send_l1_action_raw(
        context.submission.api,
        context.submission.signer,
        plan.action,
        actions::nonce_now(),
        vault_address,
        "cancel failed",
    )
    .await?;
    let statuses = order_statuses_from_response(response, "cancel")?;

    ensure_action_statuses_ok(&statuses, "cancel order")?;
    context.submission.inv.print(&CancelConfirmationOutput {
        rows: vec![CancelConfirmation {
            coin: plan.coin,
            status: "cancelled".to_string(),
            order_id: plan.order_id,
            cloid: plan.cloid,
        }],
    });
    Ok(())
}

async fn cancel_all(
    context: OrderExecutionContext<'_>,
    user: Address,
    args: &CancelAllArgs,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let plan =
        prepare_cancel_all_orders_plan(context.submission.api.sdk(), context.resolver, user, args)
            .await?;

    if plan.cancelled_orders != 0 {
        let target = ConsentTarget::new(context.submission, vault_address, None);
        if !args.yes && !confirm_cancel_all(args.coin.as_deref(), plan.cancelled_orders, &target)? {
            return Err(CliError::Configuration("cancel-all aborted".to_string()).into());
        }
    }

    let cancelled_orders = if let Some(action) = plan.action {
        let order_ids: Vec<u64> = match &action {
            Action::Cancel(batch) => batch.cancels.iter().map(|cancel| cancel.oid).collect(),
            _ => Vec::new(),
        };
        let response = actions::send_l1_action_raw(
            context.submission.api,
            context.submission.signer,
            action,
            actions::nonce_now(),
            vault_address,
            "cancel-all failed",
        )
        .await?;
        let statuses = order_statuses_from_response(response, "cancel-all")?;
        if statuses.len() != order_ids.len() {
            return Err(status_count_mismatch(statuses.len(), order_ids.len()).into());
        }
        let rows: Vec<CancelLeg> = order_ids
            .iter()
            .zip(&statuses)
            .map(|(order_id, status)| CancelLeg::from_status(*order_id, status))
            .collect();
        let failures: Vec<Rejected> = rows.iter().filter_map(|row| row.error.clone()).collect();
        if !failures.is_empty() {
            return finish_with_failures(
                CancelLegsOutput { rows },
                "Cancelled",
                "cancel all orders",
                failures,
                context.submission.inv,
            );
        }
        statuses.len()
    } else {
        0
    };

    context.submission.inv.print(&CancelAllSummaryOutput {
        rows: vec![CancelAllSummary {
            coin: plan.summary_coin,
            cancelled_orders,
        }],
    });
    Ok(())
}

async fn modify(
    context: OrderExecutionContext<'_>,
    user: Address,
    args: &ModifyArgs,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let plan =
        prepare_modify_order_plan(context.submission.api.sdk(), context.resolver, user, args)
            .await?;
    let response = actions::send_l1_action_raw(
        context.submission.api,
        context.submission.signer,
        plan.action,
        actions::nonce_now(),
        vault_address,
        "modify failed",
    )
    .await?;
    let statuses = order_statuses_from_response(response, "modify")?;
    ensure_action_statuses_ok(&statuses, "modify order")?;
    let mut confirmation = plan.confirmation;
    confirmation.order_id = modification_result_oid(&statuses).or(confirmation.order_id);

    context.submission.inv.print(&ModifyConfirmationOutput {
        rows: vec![confirmation],
    });
    Ok(())
}

async fn twap_create(
    context: OrderSubmissionContext<'_>,
    plan: TwapCreatePlan,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    verify_margin_mode(context, vault_address, Some(&plan.perp), plan.margin_mode).await?;
    context.consent(vault_address, None, "TWAP", "TWAP creation", |target| {
        confirm_mainnet_twap(&plan, target)
    })?;
    let response = actions::send_raw_l1_json_action(
        context.api,
        context.signer,
        &plan.action,
        actions::RawL1ActionMetadata::new(actions::nonce_now()).with_vault_address(vault_address),
        "twap-create rejected",
    )
    .await?;
    let twap_id = parse_twap_create_response(response)?;

    context.inv.print(&TwapCreateOutput {
        rows: vec![TwapCreateConfirmation {
            coin: plan.coin,
            side: plan.side,
            size: plan.size,
            duration_seconds: plan.duration_seconds,
            duration_minutes: plan.duration_minutes,
            status: "running".to_string(),
            twap_id,
        }],
    });
    Ok(())
}

async fn twap_cancel(
    context: OrderSubmissionContext<'_>,
    plan: TwapCancelPlan,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let response = actions::send_raw_l1_json_action(
        context.api,
        context.signer,
        &plan.action,
        actions::RawL1ActionMetadata::new(actions::nonce_now()).with_vault_address(vault_address),
        "twap-cancel rejected",
    )
    .await?;
    parse_twap_cancel_response(response)?;

    context.inv.print(&TwapCancelOutput {
        rows: vec![TwapCancelConfirmation {
            coin: plan.coin,
            status: "cancelled".to_string(),
            twap_id: plan.twap_id,
        }],
    });
    Ok(())
}

async fn schedule_cancel(
    context: OrderSubmissionContext<'_>,
    plan: ScheduleCancelPlan,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    context.consent(vault_address, None, "schedule-cancel", "action", |target| {
        confirm_mainnet_schedule_cancel(&plan, target)
    })?;
    // The trigger time counts from the moment of sending, not from the prompt.
    let scheduled = plan.scheduled(Utc::now());
    let nonce = actions::nonce_now();
    if let Some(scheduled) = &scheduled {
        actions::send_l1_action_raw(
            context.api,
            context.signer,
            Action::ScheduleCancel(ScheduleCancel {
                time: Some(scheduled.at_ms()),
            }),
            nonce,
            vault_address,
            "schedule-cancel rejected",
        )
        .await?;
    } else {
        actions::send_raw_l1_json_action(
            context.api,
            context.signer,
            &ScheduleCancelRawAction {
                action_type: "scheduleCancel",
                time: None,
            },
            actions::RawL1ActionMetadata::new(nonce).with_vault_address(vault_address),
            "schedule-cancel clear rejected",
        )
        .await?;
    }

    context.inv.print(&ScheduleCancelOutput {
        rows: vec![ScheduleCancelConfirmation {
            status: if scheduled.is_some() {
                "scheduled".to_string()
            } else {
                "cleared".to_string()
            },
            scheduled_time: scheduled.as_ref().map(|value| value.at.to_rfc3339()),
            scheduled_time_ms: scheduled.as_ref().map(ScheduledCancel::at_ms),
            in_seconds: scheduled.as_ref().map(|value| value.in_seconds),
        }],
    });
    Ok(())
}

fn tpsl_confirmation_rows(
    prepared: &PreparedTpslBatch,
    statuses: Vec<OrderResponseStatus>,
) -> Result<Vec<TpslOrderConfirmation>, CliError> {
    if statuses.len() != prepared.legs.len() {
        return Err(status_count_mismatch(statuses.len(), prepared.legs.len()));
    }
    Ok(prepared
        .legs
        .iter()
        .zip(statuses)
        .map(|(leg, status)| {
            TpslOrderConfirmation::from_tpsl_status(leg, prepared.grouping, status)
        })
        .collect())
}

/// The exchange answered with a different number of statuses than orders sent,
/// so some orders may be live without a matching row.
fn status_count_mismatch(statuses: usize, orders: usize) -> CliError {
    CliError::Internal(anyhow::anyhow!(
        "The exchange returned {statuses} {} for {orders} {}. Check what is live:\n    {}",
        output::format::noun(statuses, "status", "statuses"),
        output::format::noun(orders, "order", "orders"),
        rendering::CHECK_LIVE_COMMAND
    ))
}

fn parse_cloid(input: &str) -> Result<Cloid, CliError> {
    let Some(stripped) = input.strip_prefix("0x") else {
        return Err(CliError::Configuration(
            "CLOID must be a 0x-prefixed 16-byte hex value".to_string(),
        ));
    };

    if stripped.len() != 32 {
        return Err(CliError::Configuration(format!(
            "CLOID must be exactly 16 bytes (32 hex characters after 0x); got {} hex characters",
            stripped.len()
        )));
    }

    if !stripped.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(CliError::Configuration(
            "CLOID must contain only hexadecimal characters after 0x".to_string(),
        ));
    }

    input.parse::<Cloid>().map_err(|err| {
        CliError::Configuration(format!("CLOID must be a valid 16-byte hex value: {err}"))
    })
}

fn parse_lookup_cloid(input: &str) -> Result<Cloid, CliError> {
    let stripped = input.strip_prefix("0x").unwrap_or(input);
    if stripped.is_empty()
        || stripped.len() > 32
        || !stripped.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return Err(CliError::Configuration(
            "CLOID must be a 0x-prefixed hex value up to 16 bytes".to_string(),
        ));
    }

    let padded = format!("0x{stripped:0>32}");
    padded.parse::<Cloid>().map_err(|err| {
        CliError::Configuration(format!("CLOID must be a valid 16-byte hex value: {err}"))
    })
}

async fn lookup_order(
    client: &HttpClient,
    user: Address,
    identifier: &OrderIdentifier,
) -> Result<BasicOrder, CliError> {
    let order = client
        .order_status(user, identifier.to_status_identifier())
        .await
        .map_err(from_sdk)?
        .ok_or_else(|| CliError::Unsupported(format!("unknown order {}", identifier.display())))?;
    Ok(order.order)
}

fn filter_orders_by_coin(orders: Vec<BasicOrder>, coin: Option<&str>) -> Vec<BasicOrder> {
    let Some(coin) = coin else {
        return orders;
    };
    orders
        .into_iter()
        .filter(|order| order.coin.eq_ignore_ascii_case(coin))
        .collect()
}

/// What `cancel-all --coin` cancels: orders on `coin`, as the exchange lists
/// them, which sit in the open orders of `dex` (the default dex when `None`).
pub(super) struct CoinFilter {
    pub(super) coin: String,
    pub(super) dex: Option<String>,
}

impl CoinFilter {
    fn on_default_dex(coin: String) -> Self {
        Self { coin, dex: None }
    }
}

fn resolve_cancel_all_coin_filter(
    resolver: &AssetResolver,
    coin: Option<&str>,
    dex: Option<&str>,
) -> Result<Option<CoinFilter>, CliError> {
    let Some(coin) = coin else {
        return Ok(None);
    };
    if let Some(internal_id) = parse_spot_coin_key(coin) {
        return Ok(Some(CoinFilter::on_default_dex(spot_internal_coin_key(
            internal_id,
        ))));
    }

    let query = match parse_asset_query(coin) {
        AssetQuery::Outcome(_) => coin.to_string(),
        _ => qualify_dex_asset(dex, coin),
    };
    match resolver.resolve(&query) {
        Ok(Market::Perp(perp)) => Ok(Some(CoinFilter {
            coin: perp.display_name(),
            dex: perp.dex.clone(),
        })),
        Ok(Market::Spot(spot)) => Ok(Some(CoinFilter::on_default_dex(spot_coin_key(spot.index)))),
        Ok(Market::Outcome(outcome)) => Ok(Some(CoinFilter::on_default_dex(outcome_coin(
            outcome.parse()?.encoding,
        )))),
        Err(err) => match parse_asset_query(&query) {
            AssetQuery::Hip3 { dex, token } => Ok(Some(CoinFilter {
                coin: format!("{dex}:{token}"),
                dex: Some(dex),
            })),
            _ => Err(err.into()),
        },
    }
}

fn order_type_placement(
    order: &BasicOrder,
    trigger_price: Option<Decimal>,
    legacy_price: Option<Decimal>,
) -> Result<OrderTypePlacement, CliError> {
    // A market trigger keeps the order's own trigger price; its limit price is
    // not the trigger and would silently move it on a size-only modify.
    let market_trigger = |tpsl| OrderTypePlacement::Trigger {
        is_market: true,
        trigger_px: trigger_price
            .or(legacy_price)
            .or(order.trigger_px)
            .unwrap_or(order.limit_px),
        tpsl,
    };
    match order.order_type {
        // A generic `Trigger` order reports no TP/SL side; it has always been
        // replaced as a stop.
        OrderType::StopMarket | OrderType::Trigger => Ok(market_trigger(TpSl::Sl)),
        OrderType::TakeProfitMarket => Ok(market_trigger(TpSl::Tp)),
        OrderType::StopLimit => Ok(OrderTypePlacement::Trigger {
            is_market: false,
            trigger_px: require_modify_trigger_price(trigger_price, "stop-limit")?,
            tpsl: TpSl::Sl,
        }),
        OrderType::TakeProfitLimit => Ok(OrderTypePlacement::Trigger {
            is_market: false,
            trigger_px: require_modify_trigger_price(trigger_price, "take-limit")?,
            tpsl: TpSl::Tp,
        }),
        OrderType::TrailingStopMarket => Err(CliError::Unsupported(
            "orders modify can't change a Trailing Stop Market order. Cancel it with `hyperliquid orders cancel <OID>`, then place a new one with `hyperliquid orders trailing-stop`."
                .to_string(),
        )),
        OrderType::TwapSlice => Err(CliError::Unsupported(
            "orders modify can't change a Twap Slice order. Cancel the TWAP with `hyperliquid orders twap-cancel`, then re-create it with `hyperliquid orders twap-create`."
                .to_string(),
        )),
        OrderType::VaultClose => Err(CliError::Unsupported(
            "orders modify can't change a Vault Close order. The exchange places it when a vault closes positions; cancel it with `hyperliquid orders cancel <OID>` and place a regular order instead."
                .to_string(),
        )),
        OrderType::SpotDustConversion => Err(CliError::Unsupported(
            "orders modify can't change a Spot Dust Conversion order. Cancel it with `hyperliquid orders cancel <OID>` and place a regular order instead."
                .to_string(),
        )),
        OrderType::Limit | OrderType::Market => {
            if trigger_price.is_some() {
                return Err(CliError::Configuration(
                    "orders modify --trigger-price only applies to trigger orders".to_string(),
                ));
            }
            Ok(OrderTypePlacement::Limit {
                tif: order.tif.unwrap_or(TimeInForce::Gtc),
            })
        }
    }
}

fn require_modify_trigger_price(
    trigger_price: Option<Decimal>,
    order_type: &'static str,
) -> Result<Decimal, CliError> {
    trigger_price.ok_or_else(|| {
        CliError::Configuration(format!(
            "orders modify requires --trigger-price when modifying {order_type} orders so the trigger price is preserved"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::planning::{
        position_tpsl_batch, prepare_normal_tpsl_batch, prepare_order, scale_sizes,
    };
    use super::validation::{OrderSpec, validate_order_for_market, validate_scale_args};
    use super::*;
    use crate::assets::test_support::hip3_perp;
    use crate::assets::{AssetMetadata, PerpAsset, SpotAsset};
    use crate::settings::{EnvSource, Settings};
    use hypersdk::hypercore::Chain;
    use std::time::Duration;

    fn resolver() -> AssetResolver {
        AssetResolver::new(AssetMetadata::from_assets(
            vec![
                PerpAsset {
                    name: "BTC".to_string(),
                    index: 0,
                    dex: None,
                    sz_decimals: 5,
                    collateral: "USDC".to_string(),
                },
                hip3_perp("testdex", "HYPE", 11_100),
            ],
            vec![SpotAsset {
                symbol: "HYPE/USDC".to_string(),
                index: 11_035,
                base: "HYPE".to_string(),
                quote: "USDC".to_string(),
                base_sz_decimals: 2,
            }],
        ))
    }

    async fn prepare(args: &CreateArgs) -> Result<PreparedOrder, CliError> {
        let client = HttpClient::new(Chain::Testnet);
        let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), None);
        prepare_order(
            &client,
            &resolver(),
            &settings,
            &OrderSpec::from_create(args)?,
        )
        .await
    }

    fn btc() -> PerpAsset {
        resolver().resolve_perp("BTC").unwrap()
    }

    fn base_args() -> CreateArgs {
        CreateArgs {
            coin: "BTC".to_string(),
            dex: None,
            side: OrderSide::Buy,
            price: Some(Decimal::from(50_000)),
            trigger_price: None,
            size: Some(Decimal::new(1, 1)),
            amount: None,
            order_type: CreateOrderType::Limit,
            tif: TifArg::Gtc,
            reduce_only: false,
            max_slippage_bps: DEFAULT_MARKET_ORDER_SLIPPAGE_BPS,
            take_profit: None,
            stop_loss: None,
            grouping: None,
            on_behalf_of: None,
            margin_mode: None,
            builder: None,
            builder_fee_rate: None,
            cloid: None,
            yes: false,
        }
    }

    #[test]
    fn market_limit_price_rounds_buy_up_to_perp_tick() {
        let price = MarketPrecision::perp(5)
            .marketable_price(Decimal::new(9_472_033, 2), OrderSide::Buy, 200)
            .unwrap();

        assert_eq!(price, Decimal::from(96_615));
    }

    #[test]
    fn market_limit_price_rounds_sell_down_to_perp_tick() {
        let price = MarketPrecision::perp(5)
            .marketable_price(Decimal::new(9_472_033, 2), OrderSide::Sell, 200)
            .unwrap();

        assert_eq!(price, Decimal::from(92_825));
    }

    #[tokio::test]
    async fn limit_order_builds_hypersdk_request() {
        let prepared = prepare(&base_args()).await.unwrap();

        assert_eq!(prepared.request.asset, 0);
        assert!(prepared.request.is_buy);
        assert_eq!(prepared.request.limit_px, Decimal::from(50_000));
        assert_eq!(prepared.request.sz, Decimal::new(1, 1));
        assert!(!prepared.request.reduce_only);
        assert!(matches!(
            prepared.request.order_type,
            OrderTypePlacement::Limit {
                tif: TimeInForce::Gtc
            }
        ));
    }

    #[tokio::test]
    async fn normal_tpsl_grouping_builds_parent_and_child_orders() {
        let mut args = base_args();
        args.take_profit = Some(Decimal::from(55_000));
        args.stop_loss = Some(Decimal::from(49_000));
        args.grouping = Some(CreateTpslGroupingArg::NormalTpsl);

        let parent = prepare(&args).await.unwrap();
        let prepared = prepare_normal_tpsl_batch(
            &parent,
            &OrderSpec::from_create(&args).unwrap().tpsl.unwrap(),
        )
        .unwrap();
        let json = serde_json::to_value(&prepared.batch).unwrap();

        assert_eq!(prepared.legs.len(), 3);
        assert!(matches!(prepared.batch.grouping, OrderGrouping::NormalTpsl));
        assert_eq!(json["grouping"], "normalTpsl");
        assert_eq!(prepared.legs[0].leg, TpslLegKind::Parent);
        assert!(!prepared.legs[0].reduce_only);
        assert_eq!(prepared.legs[1].leg, TpslLegKind::TakeProfit);
        assert_eq!(prepared.legs[1].side, OrderSide::Sell);
        assert_eq!(prepared.legs[1].size, Decimal::new(1, 1));
        assert!(prepared.legs[1].reduce_only);
        assert!(matches!(
            prepared.legs[1].request.order_type,
            OrderTypePlacement::Trigger {
                is_market: true,
                trigger_px,
                tpsl: TpSl::Tp,
            } if trigger_px == Decimal::from(55_000)
        ));
        assert!(matches!(
            prepared.legs[2].request.order_type,
            OrderTypePlacement::Trigger {
                is_market: true,
                trigger_px,
                tpsl: TpSl::Sl,
            } if trigger_px == Decimal::from(49_000)
        ));
    }

    #[tokio::test]
    async fn normal_tpsl_grouping_accepts_hip3_perp_asset_ids() {
        let mut args = base_args();
        args.coin = "HYPE".to_string();
        args.dex = Some("testdex".to_string());
        args.take_profit = Some(Decimal::from(55_000));
        args.stop_loss = Some(Decimal::from(49_000));
        args.grouping = Some(CreateTpslGroupingArg::NormalTpsl);

        let parent = prepare(&args).await.unwrap();

        assert!(parent.perp().is_some());
        assert!(parent.request.asset >= 10_000);

        let prepared = prepare_normal_tpsl_batch(
            &parent,
            &OrderSpec::from_create(&args).unwrap().tpsl.unwrap(),
        )
        .unwrap();
        let json = serde_json::to_value(&prepared.batch).unwrap();

        assert_eq!(prepared.legs.len(), 3);
        assert_eq!(prepared.legs[0].coin, "testdex:HYPE");
        assert!(matches!(prepared.batch.grouping, OrderGrouping::NormalTpsl));
        assert_eq!(json["grouping"], "normalTpsl");
    }

    #[test]
    fn position_tpsl_grouping_builds_reduce_only_trigger_legs() {
        let prepared = position_tpsl_batch(
            &btc(),
            OrderSide::Sell,
            Decimal::new(1, 1),
            Some(Decimal::from(55_000)),
            Some(Decimal::from(49_000)),
            None,
        );
        let json = serde_json::to_value(&prepared.batch).unwrap();

        assert_eq!(prepared.legs.len(), 2);
        assert!(matches!(
            prepared.batch.grouping,
            OrderGrouping::PositionTpsl
        ));
        assert_eq!(json["grouping"], "positionTpsl");
        assert!(prepared.legs.iter().all(|leg| leg.reduce_only));
        assert!(prepared.legs.iter().all(|leg| leg.side == OrderSide::Sell));
        assert_eq!(json["orders"][0]["r"], true);
        assert_eq!(json["orders"][0]["t"]["trigger"]["tpsl"], "tp");
        assert_eq!(json["orders"][1]["t"]["trigger"]["tpsl"], "sl");
    }

    #[test]
    fn position_tpsl_batch_applies_custom_cloid_to_first_child_leg() {
        let cloid = "0x00000000000000000000000000000001"
            .parse::<Cloid>()
            .unwrap();
        let prepared = position_tpsl_batch(
            &btc(),
            OrderSide::Sell,
            Decimal::new(1, 1),
            Some(Decimal::from(55_000)),
            Some(Decimal::from(49_000)),
            Some(cloid),
        );

        assert_eq!(prepared.legs.len(), 2);
        assert_eq!(prepared.legs[0].request.cloid, cloid);
        assert_eq!(prepared.legs[0].leg, TpslLegKind::TakeProfit);
        // Stop-loss leg (second child) gets default to avoid duplicates
        assert_eq!(prepared.legs[1].request.cloid, Cloid::default());
        assert_eq!(prepared.legs[1].leg, TpslLegKind::StopLoss);
    }

    #[test]
    fn position_tpsl_batch_applies_cloid_to_sl_when_tp_absent() {
        let cloid = "0x00000000000000000000000000000001"
            .parse::<Cloid>()
            .unwrap();
        let prepared = position_tpsl_batch(
            &btc(),
            OrderSide::Sell,
            Decimal::new(1, 1),
            None,
            Some(Decimal::from(49_000)),
            Some(cloid),
        );

        assert_eq!(prepared.legs.len(), 1);
        assert_eq!(prepared.legs[0].request.cloid, cloid);
        assert_eq!(prepared.legs[0].leg, TpslLegKind::StopLoss);
    }

    #[test]
    fn tpsl_flags_reject_invalid_cloid() {
        let mut args = TpslArgs {
            coin: "BTC".to_string(),
            dex: None,
            take_profit: Some(TriggerPriceSpec::Absolute(Decimal::from(55_000))),
            stop_loss: None,
            grouping: PositionTpslGroupingArg::PositionTpsl,
            side: Some(OrderSide::Sell),
            size: Some(Decimal::new(1, 1)),
            on_behalf_of: None,
            margin_mode: None,
            yes: false,
            cloid: Some("not-a-hex-value".to_string()),
        };
        let err = TpslSpec::from_flags(&args).unwrap_err();
        assert!(
            err.to_string().contains("CLOID must be a 0x-prefixed"),
            "expected cloid validation error, got: {err}"
        );

        args.cloid = Some("0xDEADBEEF".to_string());
        let err = TpslSpec::from_flags(&args).unwrap_err();
        assert!(
            err.to_string().contains("CLOID must be exactly 16 bytes"),
            "expected cloid length error, got: {err}"
        );
    }

    #[test]
    fn tpsl_grouping_validation_rejects_missing_legs() {
        let mut create_args = base_args();
        create_args.grouping = Some(CreateTpslGroupingArg::NormalTpsl);
        let err = OrderSpec::from_create(&create_args).unwrap_err();
        assert!(err.to_string().contains("--grouping requires"));

        create_args.take_profit = Some(Decimal::from(55_000));
        create_args.grouping = None;
        let err = OrderSpec::from_create(&create_args).unwrap_err();
        assert!(err.to_string().contains("normal-tpsl"));

        let tpsl_args = TpslArgs {
            coin: "BTC".to_string(),
            dex: None,
            take_profit: None,
            stop_loss: None,
            grouping: PositionTpslGroupingArg::PositionTpsl,
            side: None,
            size: None,
            on_behalf_of: None,
            margin_mode: None,
            yes: false,
            cloid: None,
        };
        let err = TpslSpec::from_flags(&tpsl_args).unwrap_err();
        assert!(
            err.to_string()
                .contains("requires --take-profit or --stop-loss")
        );
    }

    #[tokio::test]
    async fn spot_limit_order_builds_hypersdk_request() {
        let mut args = base_args();
        args.coin = "HYPE/USDC".to_string();
        args.price = Some(Decimal::new(9_690, 2));
        args.size = Some(Decimal::new(20, 2));
        args.tif = TifArg::Ioc;

        let prepared = prepare(&args).await.unwrap();

        assert_eq!(prepared.coin, "HYPE/USDC");
        assert_eq!(prepared.request.asset, 11_035);
        assert!(prepared.request.is_buy);
        assert_eq!(prepared.request.limit_px, Decimal::new(9_690, 2));
        assert_eq!(prepared.request.sz, Decimal::new(20, 2));
        assert!(!prepared.request.reduce_only);
        assert!(matches!(
            prepared.request.order_type,
            OrderTypePlacement::Limit {
                tif: TimeInForce::Ioc
            }
        ));
    }

    #[tokio::test]
    async fn spot_buy_builder_fees_are_rejected() {
        let mut args = base_args();
        args.coin = "HYPE/USDC".to_string();
        args.builder = Some("0x1111111111111111111111111111111111111111".to_string());
        args.builder_fee_rate = Some("0.01%".to_string());

        let err = prepare(&args).await.unwrap_err();

        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("spot buy orders"));
        assert!(err.to_string().contains("spot sells"));
    }

    #[tokio::test]
    async fn spot_sell_builder_fees_are_allowed() {
        let mut args = base_args();
        args.coin = "HYPE/USDC".to_string();
        args.side = OrderSide::Sell;
        args.builder = Some("0x1111111111111111111111111111111111111111".to_string());
        args.builder_fee_rate = Some("0.01%".to_string());

        let prepared = prepare(&args).await.unwrap();

        let builder = prepared.builder.unwrap();
        assert_eq!(builder.b, "0x1111111111111111111111111111111111111111");
        assert_eq!(builder.f, 10);
    }

    #[test]
    fn scale_validation_rejects_unbounded_or_ambiguous_ladders() {
        let mut args = ScaleArgs {
            coin: "BTC".to_string(),
            dex: None,
            side: OrderSide::Buy,
            start_price: Decimal::from(50_000),
            end_price: Decimal::from(50_000),
            total_size: Decimal::new(1, 1),
            order_count: 2,
            tif: TifArg::Gtc,
            reduce_only: false,
            on_behalf_of: None,
            margin_mode: None,
            yes: false,
        };

        let err = validate_scale_args(&args).unwrap_err();
        assert!(err.to_string().contains("different start and end"));

        args.end_price = Decimal::from(49_000);
        args.order_count = MAX_BATCH_ORDER_COUNT + 1;
        let err = validate_scale_args(&args).unwrap_err();
        assert!(err.to_string().contains("at most"));
    }

    #[test]
    fn scale_sizes_rejects_non_even_distribution() {
        let err = scale_sizes(Decimal::new(1, 2), 3).unwrap_err();
        assert!(err.to_string().contains("evenly distributed"));
    }

    #[tokio::test]
    async fn spot_reduce_only_orders_are_rejected_before_submission() {
        let mut args = base_args();
        args.coin = "HYPE/USDC".to_string();
        args.reduce_only = true;

        let err = prepare(&args).await.unwrap_err();

        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("--reduce-only"));
        assert!(err.to_string().contains("perpetual markets only"));
    }

    #[tokio::test]
    async fn spot_trigger_orders_are_rejected_before_submission() {
        let mut args = base_args();
        args.coin = "HYPE/USDC".to_string();
        args.order_type = CreateOrderType::StopLimit;
        args.trigger_price = Some(Decimal::from(48_000));

        let err = prepare(&args).await.unwrap_err();

        assert_eq!(err.exit_code(), 13);
        assert!(
            err.to_string()
                .contains("trigger orders currently support perpetual markets only")
        );
    }

    #[tokio::test]
    async fn orders_trigger_limit_take_limit_with_reduce_only_builds_limit_trigger_payload() {
        let mut args = base_args();
        args.side = OrderSide::Sell;
        args.order_type = CreateOrderType::TakeLimit;
        args.trigger_price = Some(Decimal::from(55_000));
        args.price = Some(Decimal::from(54_750));
        args.reduce_only = true;

        let prepared = prepare(&args).await.unwrap();

        assert_eq!(prepared.request.limit_px, Decimal::from(54_750));
        assert!(matches!(
            prepared.request.order_type,
            OrderTypePlacement::Trigger {
                is_market: false,
                trigger_px,
                tpsl: TpSl::Tp,
            } if trigger_px == Decimal::from(55_000)
        ));
        assert!(prepared.request.reduce_only);
    }

    #[test]
    fn orders_trigger_limit_requires_trigger_price() {
        let mut args = base_args();
        args.order_type = CreateOrderType::StopLimit;
        args.trigger_price = None;

        let err = OrderSpec::from_create(&args).unwrap_err();

        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--trigger-price"));
    }

    fn basic_order(order_type: OrderType) -> BasicOrder {
        BasicOrder {
            timestamp: 0,
            coin: "BTC".to_string(),
            side: hypersdk::hypercore::Side::Bid,
            limit_px: Decimal::from(50_000),
            sz: Decimal::ONE,
            oid: 7,
            orig_sz: Decimal::ONE,
            cloid: None,
            order_type,
            tif: None,
            reduce_only: false,
            is_trigger: None,
            trigger_px: None,
            trigger_condition: None,
            is_position_tpsl: None,
        }
    }

    #[test]
    fn modify_rejects_order_types_that_cannot_be_replaced() {
        for (order_type, name, hint) in [
            (
                OrderType::TrailingStopMarket,
                "Trailing Stop Market",
                "hyperliquid orders cancel <OID>",
            ),
            (OrderType::TwapSlice, "Twap Slice", "orders twap-cancel"),
            (OrderType::VaultClose, "Vault Close", "orders cancel <OID>"),
            (
                OrderType::SpotDustConversion,
                "Spot Dust Conversion",
                "orders cancel <OID>",
            ),
        ] {
            let err = order_type_placement(&basic_order(order_type), None, None).unwrap_err();

            assert!(matches!(err, CliError::Unsupported(_)));
            assert_eq!(err.exit_code(), 13);
            let message = err.to_string();
            assert!(message.contains(name), "{message}");
            assert!(message.contains(hint), "{message}");
        }
    }
    fn scale_args(total_size: Decimal, start: Decimal, end: Decimal, count: usize) -> ScaleArgs {
        ScaleArgs {
            coin: "BTC".to_string(),
            dex: None,
            side: OrderSide::Buy,
            start_price: start,
            end_price: end,
            total_size,
            order_count: count,
            tif: TifArg::Gtc,
            reduce_only: false,
            on_behalf_of: None,
            margin_mode: None,
            yes: false,
        }
    }

    #[tokio::test]
    async fn scale_places_the_whole_total_size_across_legs() {
        let client = HttpClient::new(Chain::Testnet);
        let args = scale_args(
            Decimal::new(3, 1),
            Decimal::from(50_000),
            Decimal::from(49_000),
            3,
        );
        let prepared = planning::prepare_scale_batch(&client, &resolver(), &args)
            .await
            .unwrap();
        let sizes: Vec<_> = prepared.legs.iter().map(|leg| leg.order.size()).collect();
        assert_eq!(sizes, vec![Decimal::new(1, 1); 3]);
    }

    #[tokio::test]
    async fn scale_rejects_legs_whose_size_would_be_lost_to_the_lot() {
        // 0.00003 / 2 = 0.000015 per leg, which BTC's 5-decimal lot truncates to
        // 0.00001: the ladder would place 0.00002 of the 0.00003 requested.
        let client = HttpClient::new(Chain::Testnet);
        let args = scale_args(
            Decimal::new(3, 5),
            Decimal::from(50_000),
            Decimal::from(49_000),
            2,
        );
        let err = planning::prepare_scale_batch(&client, &resolver(), &args)
            .await
            .expect_err("size lost to the lot must be rejected");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("lot size"), "{err}");
    }

    #[tokio::test]
    async fn scale_rejects_prices_that_collapse_onto_one_tick() {
        // 50000 and 50000.9 are both 50000 on BTC's whole-number tick grid.
        let client = HttpClient::new(Chain::Testnet);
        let args = scale_args(
            Decimal::new(2, 1),
            Decimal::from(50_000),
            Decimal::new(500_009, 1),
            2,
        );
        let err = planning::prepare_scale_batch(&client, &resolver(), &args)
            .await
            .expect_err("collapsed ladder must be rejected");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("same tick"), "{err}");
    }

    #[test]
    fn scale_validation_names_the_flag_that_is_not_positive() {
        let args = scale_args(
            Decimal::ZERO,
            Decimal::from(50_000),
            Decimal::from(49_000),
            2,
        );
        let err = validate_scale_args(&args).unwrap_err();
        assert!(
            err.to_string()
                .contains("total-size must be greater than zero"),
            "{err}"
        );

        let args = scale_args(Decimal::ONE, Decimal::from(-1), Decimal::from(49_000), 2);
        let err = validate_scale_args(&args).unwrap_err();
        assert!(
            err.to_string().contains("start-price must be positive"),
            "{err}"
        );
    }

    fn twap_args(size: Decimal) -> TwapCreateArgs {
        TwapCreateArgs {
            coin: "BTC".to_string(),
            dex: None,
            side: OrderSide::Buy,
            size,
            duration: 300,
            margin_mode: None,
            on_behalf_of: None,
            randomize: false,
            yes: false,
        }
    }

    #[test]
    fn twap_size_truncates_to_the_market_lot() {
        let plan =
            planning::prepare_twap_create_plan(&btc(), &twap_args(Decimal::new(123_456_789, 9)))
                .unwrap();
        assert_eq!(plan.size, Decimal::new(12_345, 5));
        let wire = serde_json::to_value(&plan.action).unwrap();
        assert_eq!(wire["twap"]["s"], "0.12345");
        assert_eq!(plan.duration_minutes, 5);

        let err = planning::prepare_twap_create_plan(&btc(), &twap_args(Decimal::new(1, 8)))
            .err()
            .expect("a size below one lot is rejected");
        assert!(err.to_string().contains("truncates to zero"), "{err}");
    }

    fn fixed_tpsl_args(size: Decimal) -> TpslArgs {
        TpslArgs {
            coin: "BTC".to_string(),
            dex: None,
            take_profit: Some(TriggerPriceSpec::Absolute(Decimal::from(55_000))),
            stop_loss: Some(TriggerPriceSpec::Absolute(Decimal::from(49_000))),
            grouping: PositionTpslGroupingArg::PositionTpsl,
            side: Some(OrderSide::Sell),
            size: Some(size),
            on_behalf_of: None,
            margin_mode: None,
            yes: false,
            cloid: None,
        }
    }

    #[test]
    fn tpsl_fixed_size_truncates_to_the_market_lot_in_the_dry_run_plan() {
        let spec = TpslSpec::from_flags(&fixed_tpsl_args(Decimal::new(123_456_789, 9))).unwrap();
        let plan = planning::tpsl_dry_run_plan(&btc(), &spec, &None)
            .unwrap()
            .into_args();
        assert_eq!(plan["legs"][0]["size"], "0.12345");
        assert_eq!(plan["batch_order"]["orders"][1]["s"], "0.12345");
    }

    #[test]
    fn market_precision_truncates_prices_to_the_tick_grid() {
        let perp = MarketPrecision::perp(5);
        for (price, expected) in [
            // One decimal of room on a 5-decimal-lot perp, integers exempt from the 5-figure cap.
            (Decimal::new(500_005, 1), Decimal::from(50_000)),
            (Decimal::new(123_456_789, 3), Decimal::from(123_456)),
            (Decimal::new(12_345_678, 4), Decimal::new(12_345, 1)),
            (Decimal::new(1_234_567, 6), Decimal::new(12, 1)),
        ] {
            assert_eq!(perp.price(price, "BTC").unwrap(), expected, "{price}");
        }
        assert_eq!(
            MarketPrecision::spot(2)
                .price(Decimal::new(969_012_345, 8), "HYPE/USDC")
                .unwrap(),
            Decimal::new(96_901, 4)
        );
        let err = perp.price(Decimal::new(1, 9), "BTC").unwrap_err();
        assert!(err.to_string().contains("smallest valid tick"), "{err}");
    }

    #[test]
    fn market_precision_clamps_the_decimal_budget_for_oversized_lots() {
        // szDecimals above the budget leaves no decimals: whole-number prices only.
        assert_eq!(
            MarketPrecision::perp(8)
                .price(Decimal::new(500_005, 1), "TOKEN")
                .unwrap(),
            Decimal::from(50_000)
        );
    }

    #[test]
    fn margin_mode_on_a_spot_market_is_rejected_with_one_message() {
        let mut args = base_args();
        args.coin = "HYPE/USDC".to_string();
        args.margin_mode = Some(MarginModeArg::Isolated);
        let spot = resolver().resolve("HYPE/USDC").unwrap();
        let err =
            validate_order_for_market(&OrderSpec::from_create(&args).unwrap(), &spot).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            err.to_string(),
            "Configuration error: --margin-mode isolated is only supported for perpetual orders"
        );
    }

    #[tokio::test]
    async fn outcome_market_orders_are_rejected_before_any_mid_lookup() {
        let mut args = base_args();
        args.coin = "#10".to_string();
        args.order_type = CreateOrderType::Market;
        args.price = None;
        args.size = None;
        args.amount = Some(Decimal::from(5));
        let err = prepare(&args).await.unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(
            err.to_string()
                .contains("outcome orders do not support --type market"),
            "{err}"
        );
    }

    #[test]
    fn schedule_cancel_counts_from_the_moment_of_sending() {
        let plan = planning::prepare_schedule_cancel_plan(&ScheduleCancelArgs {
            in_duration: Some(Duration::from_secs(30)),
            clear: false,
            on_behalf_of: None,
            yes: false,
        })
        .unwrap();
        let now = Utc::now();
        let first = plan.scheduled(now).unwrap();
        let later = plan.scheduled(now + chrono::Duration::seconds(7)).unwrap();
        assert_eq!(first.in_seconds, 30);
        assert_eq!(later.at - first.at, chrono::Duration::seconds(7));
        assert!(
            planning::prepare_schedule_cancel_plan(&ScheduleCancelArgs {
                in_duration: None,
                clear: true,
                on_behalf_of: None,
                yes: false,
            })
            .unwrap()
            .scheduled(now)
            .is_none()
        );
    }

    #[test]
    fn cloid_sentinel_round_trips_through_the_wire_value() {
        let cloid = "0x00000000000000000000000000000001"
            .parse::<Cloid>()
            .unwrap();
        assert_eq!(cloid_from_wire(wire_cloid(Some(cloid))), Some(cloid));
        assert_eq!(cloid_from_wire(wire_cloid(None)), None);
    }
}
