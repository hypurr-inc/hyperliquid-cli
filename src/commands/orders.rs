//! The `orders` commands. This file holds the executors, which send what
//! planning prepared; planning, validation and rendering live in the modules
//! below and never import it.

use hypersdk::hypercore::types::{Action, OrderResponseStatus, ScheduleCancel};

use crate::assets::{AssetResolver, PerpAsset};
use crate::clock;
use crate::commands::common::rows::Rows;
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
mod limits;
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
mod vocab;

use args::{CancelAllArgs, CancelArgs, ModifyArgs};
use consent::Consent;
use context::OrderSubmissionContext;
use exchange::{
    Rejected, ScheduleCancelRawAction, Wording, ensure_action_statuses_ok, modification_result_oid,
    rejected_all,
};
use existing::{
    ModifyOrderPlan, batch_modify_action, prepare_cancel_all_orders_plan,
    prepare_cancel_order_plan, prepare_modify_order_plan,
};
use identifier::{BatchModifyOrder, OrderIdentifier, ensure_distinct};
use planning::{
    ScheduleCancelPlan, ScheduledCancel, TwapCancelPlan, TwapCreatePlan,
    prepare_position_tpsl_batch,
};
use prepared::{CreateOrderSubmission, PreparedOrder, PreparedOrderBatch, PreparedTpslBatch};
use rendering::{
    BatchVerb, CancelAllSummary, CancelConfirmation, CancelLeg, Legs, ModifyLeg, OrderConfirmation,
    PartialBatchOutput, ScheduleCancelConfirmation, TpslOrderConfirmation, TwapCancelConfirmation,
    TwapCreateConfirmation, pair_statuses, single_status,
};
use status::Status;
use submit::{Gate, clear_to_send, submit_orders};
use validation::TpslSpec;

async fn create(
    context: OrderSubmissionContext<'_>,
    submission: CreateOrderSubmission,
) -> Result<(), anyhow::Error> {
    match submission {
        CreateOrderSubmission::NormalTpsl(order) => {
            let cleared =
                clear_to_send(context, Gate::tpsl_order(&order.parent, &order.batch)).await?;
            let statuses = submit_orders(
                context,
                &cleared,
                order.batch.batch.clone(),
                Wording::PLACEMENT,
            )
            .await?;
            let rows = tpsl_confirmation_rows(&order.batch, statuses)?;
            report_legs(Legs::new(rows), BatchVerb::PlaceTpsl, context.inv)
        }
        CreateOrderSubmission::Single(prepared) => {
            let status = place_prepared_single(context, &prepared).await?;
            let row = OrderConfirmation::from_status(&prepared, status)?;
            context.inv.print(&Legs::new(vec![row]));
            Ok(())
        }
    }
}

async fn place_prepared_single(
    context: OrderSubmissionContext<'_>,
    prepared: &PreparedOrder,
) -> Result<OrderResponseStatus, anyhow::Error> {
    let cleared = clear_to_send(context, Gate::order(prepared)).await?;
    let statuses = submit_orders(context, &cleared, prepared.batch(), Wording::PLACEMENT).await?;
    Ok(single_status(statuses)?)
}

/// `orders scale` and `orders batch-create`: they differ only in the gate
/// (`Gate::scale` or `Gate::batch_create`) that names what the person approves.
async fn place_batch(
    context: OrderSubmissionContext<'_>,
    prepared: &PreparedOrderBatch,
    gate: for<'a> fn(&'a PreparedOrderBatch) -> Gate<'a>,
) -> Result<(), anyhow::Error> {
    let cleared = clear_to_send(context, gate(prepared)).await?;
    let statuses = submit_orders(
        context,
        &cleared,
        prepared.batch.clone(),
        Wording::PLACEMENT,
    )
    .await?;
    let rows = pair_statuses(&prepared.legs, statuses)?
        .into_iter()
        .map(|(leg, status)| OrderConfirmation::from_leg_status(leg, status))
        .collect();
    report_legs(Legs::new(rows), BatchVerb::PlaceOrders, context.inv)
}

/// Prints nothing; returns each leg's result.
async fn place_position_tpsl(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
) -> Result<Legs<TpslOrderConfirmation>, anyhow::Error> {
    let margin = MarginPlan::for_market(perp, spec.margin_mode)?;
    let prepared = prepare_position_tpsl_batch(context.api, perp, context.account(), spec).await?;
    let cleared = clear_to_send(context, Gate::position_tpsl(perp, margin, &prepared)).await?;

    let statuses = submit_orders(
        context,
        &cleared,
        prepared.batch.clone(),
        Wording::PLACEMENT,
    )
    .await?;
    Ok(Legs::new(tpsl_confirmation_rows(&prepared, statuses)?))
}

async fn tpsl(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    spec: &TpslSpec,
) -> Result<(), anyhow::Error> {
    let legs = place_position_tpsl(context, perp, spec).await?;
    report_legs(legs, BatchVerb::PlaceTpsl, context.inv)
}

/// Prints nothing: a rejected leg is returned as the error for the caller to report.
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

/// Prints the per-leg result, then returns the partial-results error (exit 15). When
/// every leg failed there is no partial result, so the rejection is the error.
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
    let statuses = context
        .send(plan.action, Wording::CANCEL)
        .await?
        .statuses()?;

    ensure_action_statuses_ok(&statuses, "cancel order")?;
    context.inv.print(&Rows::list_of_one(CancelConfirmation {
        coin: plan.coin,
        status: Status::Cancelled,
        order_id: plan.order_id,
        cloid: plan.cloid,
    }));
    Ok(())
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
        context.inv.print(&Rows::list_of_one(CancelAllSummary {
            coin: plan.summary_coin,
            cancelled_orders: 0,
        }));
        return Ok(());
    };
    context.consent(
        &Consent::CancelAll {
            coin: args.coin.as_deref(),
            count: order_ids.len(),
        },
        None,
    )?;

    let statuses = context
        .send(action, Wording::CANCEL_ALL)
        .await?
        .statuses()?;
    let rows: Vec<CancelLeg> = pair_statuses(&order_ids, statuses)?
        .into_iter()
        .map(|(order_id, status)| CancelLeg::from_status(*order_id, status))
        .collect();
    let legs = Legs::new(rows);
    let failures = legs.rejection_errors();
    if !failures.is_empty() {
        return finish_with_failures(legs, BatchVerb::CancelAll, failures, context.inv);
    }

    context.inv.print(&Rows::list_of_one(CancelAllSummary {
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
    let statuses = context.send(action, Wording::MODIFY).await?.statuses()?;
    ensure_action_statuses_ok(&statuses, "modify order")?;
    let mut confirmation = plan.confirmation;
    confirmation.order_id = modification_result_oid(&statuses).or(confirmation.order_id);

    context.inv.print(&Rows::list_of_one(confirmation));
    Ok(())
}

/// The planner's wording names an order, not its place in the orders file.
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
        ensure_distinct(
            index,
            plan.confirmation.order_id,
            plans.iter().map(|earlier| earlier.confirmation.order_id),
        )?;
        plans.push(plan);
    }

    context.consent(&Consent::ModifyOrders { count: plans.len() }, None)?;

    let action = batch_modify_action(
        plans.iter().map(|plan| plan.modify.clone()).collect(),
        always_place,
    );
    let statuses = context
        .send(action, Wording::BATCH_MODIFY)
        .await?
        .statuses()?;
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
    clear_to_send(context, Gate::twap(&plan)).await?;
    // Both TWAP actions go out through `send_raw`: it posts the SDK action in
    // the field order it serializes, where `send` would post it through a JSON
    // `Value`, which sorts the keys. The signed msgpack is the same either way.
    let twap_id = context
        .send_raw(&plan.action, Wording::TWAP_CREATE)
        .await?
        .twap_id()?;

    context
        .inv
        .print(&Rows::list_of_one(TwapCreateConfirmation {
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
    context
        .send_raw(&plan.action, Wording::TWAP_CANCEL)
        .await?
        .twap_cancelled()?;

    context
        .inv
        .print(&Rows::list_of_one(TwapCancelConfirmation {
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
    context.consent(&Consent::ScheduleCancel(&plan), None)?;
    // The trigger time counts from the moment of sending, not from the prompt.
    let scheduled = plan.scheduled(clock::now_utc());
    if let Some(scheduled) = &scheduled {
        let action = Action::ScheduleCancel(ScheduleCancel {
            time: Some(scheduled.at_ms()),
        });
        context.send(action, Wording::SCHEDULE_CANCEL).await?;
    } else {
        let action = ScheduleCancelRawAction {
            action_type: "scheduleCancel",
            time: None,
        };
        context
            .send_raw(&action, Wording::SCHEDULE_CANCEL_CLEAR)
            .await?;
    }

    context
        .inv
        .print(&Rows::list_of_one(ScheduleCancelConfirmation {
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
