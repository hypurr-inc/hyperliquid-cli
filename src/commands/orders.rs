//! The `orders` commands. This file holds the executors, which send what
//! planning prepared; planning, validation and rendering live in the modules
//! below and never import it.

use chrono::Utc;
use hypersdk::hypercore::types::{Action, OrderResponseStatus, ScheduleCancel};

use crate::assets::{AssetResolver, PerpAsset};
use crate::commands::actions;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::margin::MarginPlan;
use crate::output::{self, TableData};

mod args;
mod bracket;
mod builder_check;
mod chase;
pub mod cli;
mod cloid;
mod consent;
mod context;
mod exchange;
mod existing;
mod hip3;
mod identifier;
mod live_order;
mod planning;
mod prepared;
mod preview;
mod queries;
mod rendering;
mod run;
mod status;
mod submit;
mod trailing_stop;
mod validation;

use args::{CancelAllArgs, CancelArgs, ModifyArgs};
use consent::{
    ConsentKind, confirm_batch_modify, confirm_cancel_all, confirm_order, confirm_order_batch,
    confirm_schedule_cancel, confirm_tpsl_batch, confirm_twap,
};
use context::OrderSubmissionContext;
use exchange::{
    Rejected, ScheduleCancelRawAction, decode_statuses, ensure_action_statuses_ok,
    modification_result_oid, parse_twap_cancel_response, parse_twap_create_response, rejected_all,
};
use existing::{
    ModifyOrderPlan, batch_modify_action, prepare_cancel_all_orders_plan,
    prepare_cancel_order_plan, prepare_modify_order_plan,
};
use identifier::{BatchModifyOrder, OrderIdentifier, same_order_error};
use planning::{
    ScheduleCancelPlan, ScheduledCancel, TwapCancelPlan, TwapCreatePlan,
    prepare_position_tpsl_batch,
};
use prepared::{CreateOrderSubmission, PreparedOrder, PreparedOrderBatch, PreparedTpslBatch};
pub use queries::open_orders_output;
pub use rendering::OrderListOutput;
use rendering::{
    BatchVerb, CancelAllSummary, CancelConfirmation, CancelLeg, Legs, ModifyLeg, OrderConfirmation,
    PartialBatchOutput, ScheduleCancelConfirmation, TpslOrderConfirmation, TwapCancelConfirmation,
    TwapCreateConfirmation, pair_statuses, single_status,
};
use status::Status;
use submit::{Cleared, Gate, PLACEMENT_FAILED, clear_to_send, submit_orders};
use validation::TpslSpec;

async fn create(
    context: OrderSubmissionContext<'_>,
    submission: CreateOrderSubmission,
) -> Result<(), anyhow::Error> {
    match submission {
        CreateOrderSubmission::NormalTpsl(order) => {
            let cleared = clear_to_send(
                context,
                Gate::for_order(ConsentKind::Tpsl, &order.parent),
                |target| confirm_tpsl_batch(&order.batch, target),
            )
            .await?;
            let statuses = submit_orders(
                context,
                &cleared,
                order.batch.batch.clone(),
                PLACEMENT_FAILED,
            )
            .await?;
            let rows = tpsl_confirmation_rows(&order.batch, statuses)?;
            report_tpsl_batch(Legs::new(rows), context.inv)
        }
        CreateOrderSubmission::Single(prepared) => {
            let status = place_prepared_single(context, &prepared).await?;
            let row = OrderConfirmation::from_status(&prepared, status)?;
            context.inv.print(&Legs::new(vec![row]));
            Ok(())
        }
    }
}

/// Place one order and return the exchange's one status for it.
async fn place_prepared_single(
    context: OrderSubmissionContext<'_>,
    prepared: &PreparedOrder,
) -> Result<OrderResponseStatus, anyhow::Error> {
    let cleared = clear_to_send(
        context,
        Gate::for_order(ConsentKind::Order, prepared),
        |target| confirm_order(prepared, target),
    )
    .await?;
    let statuses = submit_orders(context, &cleared, prepared.batch(), PLACEMENT_FAILED).await?;
    Ok(single_status(statuses)?)
}

async fn scale(
    context: OrderSubmissionContext<'_>,
    prepared: PreparedOrderBatch,
) -> Result<(), anyhow::Error> {
    let cleared = clear_to_send(
        context,
        Gate::for_batch(ConsentKind::ScaleOrders, &prepared),
        |target| confirm_order_batch("scale", &prepared, target),
    )
    .await?;
    place_order_batch(context, &cleared, &prepared).await
}

async fn batch_create(
    context: OrderSubmissionContext<'_>,
    prepared: PreparedOrderBatch,
) -> Result<(), anyhow::Error> {
    let cleared = clear_to_send(
        context,
        Gate::for_batch(ConsentKind::BatchOrders, &prepared),
        |target| confirm_order_batch("batch-create", &prepared, target),
    )
    .await?;
    place_order_batch(context, &cleared, &prepared).await
}

async fn place_order_batch(
    context: OrderSubmissionContext<'_>,
    cleared: &Cleared<'_>,
    prepared: &PreparedOrderBatch,
) -> Result<(), anyhow::Error> {
    let statuses =
        submit_orders(context, cleared, prepared.batch.clone(), PLACEMENT_FAILED).await?;
    let rows = pair_statuses(&prepared.legs, statuses)?
        .into_iter()
        .map(|(leg, status)| OrderConfirmation::from_leg_status(leg, status))
        .collect();
    finish_order_batch(rows, context.inv)
}

/// Place position-attached TP/SL orders and return each leg's result without
/// printing anything.
async fn place_position_tpsl(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
) -> Result<Legs<TpslOrderConfirmation>, anyhow::Error> {
    let margin = MarginPlan::for_market(perp, spec.margin_mode)?;
    let prepared = prepare_position_tpsl_batch(context.api, perp, context.account(), spec).await?;
    let cleared = clear_to_send(
        context,
        Gate::for_position(ConsentKind::Tpsl, perp, margin),
        |target| confirm_tpsl_batch(&prepared, target),
    )
    .await?;

    let statuses =
        submit_orders(context, &cleared, prepared.batch.clone(), PLACEMENT_FAILED).await?;
    Ok(Legs::new(tpsl_confirmation_rows(&prepared, statuses)?))
}

async fn tpsl(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
) -> Result<(), anyhow::Error> {
    let legs = place_position_tpsl(context, perp, spec).await?;
    report_tpsl_batch(legs, context.inv)
}

/// Place the TP/SL that protects a position opened by another command. Prints
/// nothing: a rejected leg is returned as the error for the caller to report.
async fn protect_position(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
) -> Result<(), anyhow::Error> {
    let legs = place_position_tpsl(context, perp, spec).await?;
    let failures = legs.rejection_errors();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(rejected_all("place TP/SL orders", &failures).into())
    }
}

/// Report a batch where some legs failed. Prints the per-leg result, then
/// returns the partial-results error (exit 15). When every leg failed there is
/// no partial result, so the rejection is the error.
fn finish_with_failures(
    legs: impl TableData + 'static,
    verb: BatchVerb,
    failures: Vec<Rejected>,
    inv: &Invocation,
) -> Result<(), anyhow::Error> {
    let total = legs.rows().len();
    let failed = failures.len();
    if failed >= total {
        return Err(rejected_all(verb.action(), &failures).into());
    }
    let succeeded = total - failed;
    let partial = PartialBatchOutput {
        legs: Box::new(legs),
        succeeded,
        failed,
        headline: format!(
            "{} {succeeded} of {total} {}. {failed} failed.",
            verb.done(),
            output::format::noun(total, "order", "orders")
        ),
    };
    inv.print(&partial);
    Err(partial.error().into())
}

/// Print a batch's legs, or report the legs that failed.
fn report_legs<T: rendering::LegRow + 'static>(
    legs: Legs<T>,
    verb: BatchVerb,
    inv: &Invocation,
) -> Result<(), anyhow::Error> {
    let failures = legs.rejection_errors();
    if failures.is_empty() {
        inv.print(&legs);
        return Ok(());
    }
    finish_with_failures(legs, verb, failures, inv)
}

fn finish_order_batch(rows: Vec<OrderConfirmation>, inv: &Invocation) -> Result<(), anyhow::Error> {
    report_legs(Legs::new(rows), BatchVerb::PlaceOrders, inv)
}

fn report_tpsl_batch(
    legs: Legs<TpslOrderConfirmation>,
    inv: &Invocation,
) -> Result<(), anyhow::Error> {
    report_legs(legs, BatchVerb::PlaceTpsl, inv)
}

fn tpsl_confirmation_rows(
    prepared: &PreparedTpslBatch,
    statuses: Vec<OrderResponseStatus>,
) -> Result<Vec<TpslOrderConfirmation>, CliError> {
    Ok(pair_statuses(&prepared.legs, statuses)?
        .into_iter()
        .map(|(leg, status)| {
            TpslOrderConfirmation::from_tpsl_status(leg, prepared.grouping, status)
        })
        .collect())
}

async fn cancel(
    context: OrderSubmissionContext<'_>,
    resolver: &AssetResolver,
    args: &CancelArgs,
) -> Result<(), anyhow::Error> {
    let plan = prepare_cancel_order_plan(context.api, resolver, context.account(), args).await?;
    let response = actions::send_l1_action(
        context.api,
        context.signer,
        plan.action,
        context.vault_address,
        "cancel failed",
    )
    .await?;
    let statuses = decode_statuses(response, "cancel")?;

    ensure_action_statuses_ok(&statuses, "cancel order")?;
    context.inv.print(&rendering_rows(CancelConfirmation {
        coin: plan.coin,
        status: Status::Cancelled,
        order_id: plan.order_id,
        cloid: plan.cloid,
    }));
    Ok(())
}

/// A single-row result, as the table and the JSON array the order commands print.
fn rendering_rows<T>(row: T) -> crate::commands::rows::Rows<T> {
    crate::commands::rows::Rows::new(vec![row])
}

async fn cancel_all(
    context: OrderSubmissionContext<'_>,
    resolver: &AssetResolver,
    args: &CancelAllArgs,
) -> Result<(), anyhow::Error> {
    let plan =
        prepare_cancel_all_orders_plan(context.api, resolver, context.account(), args).await?;
    let order_ids = plan.order_ids();

    let Some(action) = plan.action() else {
        context.inv.print(&rendering_rows(CancelAllSummary {
            coin: plan.summary_coin,
            cancelled_orders: 0,
        }));
        return Ok(());
    };
    context.consent(ConsentKind::CancelAll, None, |target| {
        confirm_cancel_all(args.coin.as_deref(), order_ids.len(), target)
    })?;

    let response = actions::send_l1_action(
        context.api,
        context.signer,
        action,
        context.vault_address,
        "cancel-all failed",
    )
    .await?;
    let statuses = decode_statuses(response, "cancel-all")?;
    let rows: Vec<CancelLeg> = pair_statuses(&order_ids, statuses)?
        .into_iter()
        .map(|(order_id, status)| CancelLeg::from_status(*order_id, status))
        .collect();
    let legs = Legs::new(rows);
    let failures = legs.rejection_errors();
    if !failures.is_empty() {
        return finish_with_failures(legs, BatchVerb::CancelAll, failures, context.inv);
    }

    context.inv.print(&rendering_rows(CancelAllSummary {
        coin: plan.summary_coin,
        cancelled_orders: order_ids.len(),
    }));
    Ok(())
}

async fn modify(
    context: OrderSubmissionContext<'_>,
    resolver: &AssetResolver,
    identifier: &OrderIdentifier,
    args: &ModifyArgs,
) -> Result<(), anyhow::Error> {
    let plan = prepare_modify_order_plan(
        context.api,
        resolver,
        context.account(),
        identifier,
        args,
        "orders modify",
    )
    .await?;
    let action = batch_modify_action(vec![plan.modify], args.always_place);
    let response = actions::send_l1_action(
        context.api,
        context.signer,
        action,
        context.vault_address,
        "modify failed",
    )
    .await?;
    let statuses = decode_statuses(response, "modify")?;
    ensure_action_statuses_ok(&statuses, "modify order")?;
    let mut confirmation = plan.confirmation;
    confirmation.order_id = modification_result_oid(&statuses).or(confirmation.order_id);

    context.inv.print(&rendering_rows(confirmation));
    Ok(())
}

/// The error for modification `index` of a batch, led by which one it was:
/// the planner's wording names an order, not its place in the orders file.
fn for_modification(index: usize, error: CliError) -> CliError {
    match error {
        CliError::Configuration(message) => {
            CliError::Configuration(format!("orders[{index}]: {message}"))
        }
        CliError::Unsupported(message) => {
            CliError::Unsupported(format!("orders[{index}]: {message}"))
        }
        other => other,
    }
}

async fn batch_modify(
    context: OrderSubmissionContext<'_>,
    resolver: &AssetResolver,
    orders: &[BatchModifyOrder],
    always_place: bool,
) -> Result<(), anyhow::Error> {
    let mut plans: Vec<ModifyOrderPlan> = Vec::with_capacity(orders.len());
    for (index, order) in orders.iter().enumerate() {
        let plan = prepare_modify_order_plan(
            context.api,
            resolver,
            context.account(),
            &order.identifier,
            &order.modify_args(always_place),
            "orders batch-modify",
        )
        .await
        .map_err(|error| for_modification(index, error))?;
        // Two names for one order (an order id and its cloid) are only
        // visible once both are looked up.
        if let Some(earlier) = plans
            .iter()
            .position(|earlier| earlier.confirmation.order_id == plan.confirmation.order_id)
        {
            return Err(same_order_error(index, earlier).into());
        }
        plans.push(plan);
    }

    context.consent(ConsentKind::ModifyOrders, None, |target| {
        confirm_batch_modify(plans.len(), target)
    })?;

    let action = batch_modify_action(
        plans.iter().map(|plan| plan.modify.clone()).collect(),
        always_place,
    );
    let response = actions::send_l1_action(
        context.api,
        context.signer,
        action,
        context.vault_address,
        "batch modify failed",
    )
    .await?;
    let statuses = decode_statuses(response, "batch modify")?;
    let rows = pair_statuses(&plans, statuses)?
        .into_iter()
        .map(|(plan, status)| ModifyLeg::from_status(plan.confirmation.clone(), status))
        .collect();
    report_legs(
        Legs::<ModifyLeg>::new(rows),
        BatchVerb::ModifyOrders,
        context.inv,
    )
}

async fn twap_create(
    context: OrderSubmissionContext<'_>,
    plan: TwapCreatePlan,
) -> Result<(), anyhow::Error> {
    clear_to_send(
        context,
        Gate::for_position(ConsentKind::Twap, &plan.perp, plan.margin),
        |target| confirm_twap(&plan, target),
    )
    .await?;
    let response = actions::send_raw_l1_json_action(
        context.api,
        context.signer,
        &plan.action,
        context.vault_address,
        "twap-create rejected",
    )
    .await?;
    let twap_id = parse_twap_create_response(response)?;

    context.inv.print(&rendering_rows(TwapCreateConfirmation {
        coin: plan.coin,
        side: plan.side,
        size: plan.size,
        duration_seconds: plan.duration_seconds,
        duration_minutes: plan.duration_minutes,
        status: Status::Running,
        twap_id,
    }));
    Ok(())
}

async fn twap_cancel(
    context: OrderSubmissionContext<'_>,
    plan: TwapCancelPlan,
) -> Result<(), anyhow::Error> {
    let response = actions::send_raw_l1_json_action(
        context.api,
        context.signer,
        &plan.action,
        context.vault_address,
        "twap-cancel rejected",
    )
    .await?;
    parse_twap_cancel_response(response)?;

    context.inv.print(&rendering_rows(TwapCancelConfirmation {
        coin: plan.coin,
        status: Status::Cancelled,
        twap_id: plan.twap_id,
    }));
    Ok(())
}

async fn schedule_cancel(
    context: OrderSubmissionContext<'_>,
    plan: ScheduleCancelPlan,
) -> Result<(), anyhow::Error> {
    context.consent(ConsentKind::ScheduleCancel, None, |target| {
        confirm_schedule_cancel(&plan, target)
    })?;
    // The trigger time counts from the moment of sending, not from the prompt.
    let scheduled = plan.scheduled(Utc::now());
    if let Some(scheduled) = &scheduled {
        actions::send_l1_action(
            context.api,
            context.signer,
            Action::ScheduleCancel(ScheduleCancel {
                time: Some(scheduled.at_ms()),
            }),
            context.vault_address,
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
            context.vault_address,
            "schedule-cancel clear rejected",
        )
        .await?;
    }

    context
        .inv
        .print(&rendering_rows(ScheduleCancelConfirmation {
            status: if scheduled.is_some() {
                Status::Scheduled
            } else {
                Status::Cleared
            },
            scheduled_time: scheduled.as_ref().map(|value| value.at.to_rfc3339()),
            scheduled_time_ms: scheduled.as_ref().map(ScheduledCancel::at_ms),
            in_seconds: scheduled.as_ref().map(|value| value.in_seconds),
        }));
    Ok(())
}
