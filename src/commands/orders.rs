//! The `orders` commands. This file holds the executors, which send what
//! planning prepared; planning, validation and rendering live in the modules
//! below and never import it.

use std::io::{self, Write};

use chrono::Utc;
use hypersdk::hypercore::types::{
    Action, BatchOrder, LeverageType, OrderGrouping, OrderResponseStatus, ScheduleCancel,
};

use crate::assets::{AssetResolver, PerpAsset};
use crate::commands::actions;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::output::{self, TableData};
use crate::prompt;

mod args;
mod bracket;
mod chase;
pub mod cli;
mod cloid;
mod consent;
mod context;
mod exchange;
mod existing;
mod hip3;
mod live_order;
mod planning;
mod prepared;
mod queries;
mod rendering;
mod trailing_stop;
mod validation;

use args::{CancelAllArgs, CancelArgs, MarginModeArg, ModifyArgs};
use consent::{
    ConsentKind, ConsentTarget, confirm_cancel_all, confirm_mainnet_order,
    confirm_mainnet_order_batch, confirm_mainnet_schedule_cancel, confirm_mainnet_tpsl_batch,
    confirm_mainnet_twap,
};
use context::OrderSubmissionContext;
use exchange::{
    OrderBuilderFee, Rejected, ScheduleCancelRawAction, decode_statuses, ensure_action_statuses_ok,
    modification_result_oid, parse_twap_cancel_response, parse_twap_create_response, place_orders,
    rejected_all,
};
use existing::{
    OrderIdentifier, prepare_cancel_all_orders_plan, prepare_cancel_order_plan,
    prepare_modify_order_plan,
};
use hip3::{Hip3MarginPlan, execute_hip3_margin_transfers, plan_hip3_margin_transfers};
use planning::{
    ScheduleCancelPlan, ScheduledCancel, TwapCancelPlan, TwapCreatePlan, fetch_position,
    prepare_position_tpsl_batch,
};
use prepared::{CreateOrderSubmission, PreparedOrder, PreparedOrderBatch, PreparedTpslBatch};
pub use rendering::OrderListOutput;
use rendering::{
    BatchVerb, CancelAllSummary, CancelAllSummaryOutput, CancelConfirmation,
    CancelConfirmationOutput, CancelLeg, CancelLegsOutput, ModifyConfirmationOutput,
    OrderConfirmation, OrderConfirmationOutput, PartialBatchOutput, ScheduleCancelConfirmation,
    ScheduleCancelOutput, TpslOrderConfirmation, TpslOrderConfirmationOutput,
    TwapCancelConfirmation, TwapCancelOutput, TwapCreateConfirmation, TwapCreateOutput,
    pair_statuses, single_status,
};
use validation::TpslSpec;

/// What a submission must clear before anything is sent, in the one order that
/// is safe: the margin mode holds, the HIP-3 dexes are funded, the user
/// approves. Every command that places orders clears it through
/// [`clear_to_send`], so none can skip a step by spelling the sequence itself.
struct Gate<'a> {
    kind: ConsentKind,
    /// A perp position that must already use this margin mode.
    margin: Option<(&'a PerpAsset, MarginModeArg)>,
    /// The orders that draw on the margin of a HIP-3 dex.
    funded: Vec<&'a PreparedOrder>,
    builder: Option<&'a OrderBuilderFee>,
}

impl<'a> Gate<'a> {
    fn for_order(kind: ConsentKind, order: &'a PreparedOrder) -> Self {
        Self {
            kind,
            margin: order.perp().zip(order.margin_mode),
            funded: vec![order],
            builder: order.builder.as_ref(),
        }
    }

    fn for_batch(kind: ConsentKind, legs: &'a [PreparedOrder]) -> Self {
        Self {
            kind,
            margin: legs.first().and_then(|leg| leg.perp().zip(leg.margin_mode)),
            funded: legs.iter().collect(),
            builder: None,
        }
    }

    /// Orders that act on an existing position and need no margin of their own.
    fn for_position(
        kind: ConsentKind,
        perp: &'a PerpAsset,
        margin_mode: Option<MarginModeArg>,
    ) -> Self {
        Self {
            kind,
            margin: margin_mode.map(|mode| (perp, mode)),
            funded: Vec::new(),
            builder: None,
        }
    }
}

async fn clear_to_send(
    context: OrderSubmissionContext<'_>,
    gate: Gate<'_>,
    ask: impl FnOnce(&ConsentTarget<'_>) -> Result<bool, CliError>,
) -> Result<(), anyhow::Error> {
    if let Some((perp, margin_mode)) = gate.margin {
        verify_margin_mode(context, perp, margin_mode).await?;
    }
    let user = context.account();
    let helpers = plan_hip3_margin_transfers(context.api, user, &gate.funded).await?;
    context.consent(gate.kind, gate.builder, |target| {
        warn_hip3_margin_helpers(&helpers)?;
        ask(target)
    })?;
    execute_hip3_margin_transfers(
        context.inv.mode(),
        context.inv.remote()?,
        context.signer,
        user,
        &helpers,
    )
    .await
}

fn warn_hip3_margin_helpers(helpers: &[Hip3MarginPlan]) -> Result<(), CliError> {
    let mut stderr = io::stderr();
    for plan in helpers {
        writeln!(
            stderr,
            "{}",
            prompt::warning(&format!(
                "This order requires a helper transfer of {} {} from perp to dex:{} before placement.",
                plan.amount, plan.token, plan.dest_dex
            ))
        )
        .map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
    }
    Ok(())
}

async fn create(
    context: OrderSubmissionContext<'_>,
    submission: CreateOrderSubmission,
) -> Result<(), anyhow::Error> {
    match submission {
        CreateOrderSubmission::NormalTpsl(order) => {
            clear_to_send(
                context,
                Gate::for_order(ConsentKind::Tpsl, &order.parent),
                |target| confirm_mainnet_tpsl_batch(&order.batch, target),
            )
            .await?;
            let statuses = place_orders(
                context.api,
                context.signer,
                order.batch.batch.clone(),
                order.parent.builder.as_ref(),
                context.vault_address,
            )
            .await?;
            let rows = tpsl_confirmation_rows(&order.batch, statuses)?;
            report_tpsl_batch(TpslOrderConfirmationOutput { rows }, context.inv)
        }
        CreateOrderSubmission::Single(prepared) => {
            let status = place_prepared_single(context, &prepared).await?;
            let row = OrderConfirmation::from_status(&prepared, status)?;
            context
                .inv
                .print(&OrderConfirmationOutput { rows: vec![row] });
            Ok(())
        }
    }
}

/// Place one order and return the exchange's one status for it.
async fn place_prepared_single(
    context: OrderSubmissionContext<'_>,
    prepared: &PreparedOrder,
) -> Result<OrderResponseStatus, anyhow::Error> {
    clear_to_send(
        context,
        Gate::for_order(ConsentKind::Order, prepared),
        |target| confirm_mainnet_order(prepared, target),
    )
    .await?;

    let batch = BatchOrder {
        orders: vec![prepared.request.clone()],
        grouping: OrderGrouping::Na,
        builder: None,
    };
    let statuses = place_orders(
        context.api,
        context.signer,
        batch,
        prepared.builder.as_ref(),
        context.vault_address,
    )
    .await?;
    Ok(single_status(statuses)?)
}

async fn scale(
    context: OrderSubmissionContext<'_>,
    prepared: PreparedOrderBatch,
) -> Result<(), anyhow::Error> {
    clear_to_send(
        context,
        Gate::for_batch(ConsentKind::ScaleOrders, &prepared.legs),
        |target| confirm_mainnet_order_batch("scale", &prepared, target),
    )
    .await?;
    place_order_batch(context, prepared).await
}

async fn batch_create(
    context: OrderSubmissionContext<'_>,
    prepared: PreparedOrderBatch,
) -> Result<(), anyhow::Error> {
    clear_to_send(
        context,
        Gate::for_batch(ConsentKind::BatchOrders, &prepared.legs),
        |target| confirm_mainnet_order_batch("batch-create", &prepared, target),
    )
    .await?;
    place_order_batch(context, prepared).await
}

async fn place_order_batch(
    context: OrderSubmissionContext<'_>,
    prepared: PreparedOrderBatch,
) -> Result<(), anyhow::Error> {
    let statuses = place_orders(
        context.api,
        context.signer,
        prepared.batch.clone(),
        None,
        context.vault_address,
    )
    .await?;
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
) -> Result<TpslOrderConfirmationOutput, anyhow::Error> {
    let prepared =
        prepare_position_tpsl_batch(context.api.sdk(), perp, context.account(), spec).await?;
    clear_to_send(
        context,
        Gate::for_position(ConsentKind::Tpsl, perp, spec.margin_mode),
        |target| confirm_mainnet_tpsl_batch(&prepared, target),
    )
    .await?;

    let statuses = place_orders(
        context.api,
        context.signer,
        prepared.batch.clone(),
        None,
        context.vault_address,
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

/// Fail before placing an order when `--margin-mode` does not match the
/// account's open position on `perp`.
async fn verify_margin_mode(
    context: OrderSubmissionContext<'_>,
    perp: &PerpAsset,
    margin_mode: MarginModeArg,
) -> Result<(), anyhow::Error> {
    let coin = perp.name.as_str();
    let Some(position) = fetch_position(context.api.sdk(), context.account(), perp).await? else {
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
        LeverageType::Cross => MarginModeArg::Cross,
        LeverageType::Isolated => MarginModeArg::Isolated,
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

fn finish_order_batch(rows: Vec<OrderConfirmation>, inv: &Invocation) -> Result<(), anyhow::Error> {
    let legs = OrderConfirmationOutput { rows };
    let failures = legs.rejection_errors();
    if failures.is_empty() {
        inv.print(&legs);
        return Ok(());
    }
    finish_with_failures(legs, BatchVerb::PlaceOrders, failures, inv)
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
    finish_with_failures(legs, BatchVerb::PlaceTpsl, failures, inv)
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
    let plan =
        prepare_cancel_order_plan(context.api.sdk(), resolver, context.account(), args).await?;
    let response = actions::send_l1_action_raw(
        context.api,
        context.signer,
        plan.action,
        actions::nonce_now(),
        context.vault_address,
        "cancel failed",
    )
    .await?;
    let statuses = decode_statuses(response, "cancel")?;

    ensure_action_statuses_ok(&statuses, "cancel order")?;
    context.inv.print(&CancelConfirmationOutput {
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
    context: OrderSubmissionContext<'_>,
    resolver: &AssetResolver,
    args: &CancelAllArgs,
) -> Result<(), anyhow::Error> {
    let plan = prepare_cancel_all_orders_plan(context.api.sdk(), resolver, context.account(), args)
        .await?;
    let order_ids = plan.order_ids();

    let Some(action) = plan.action() else {
        context.inv.print(&CancelAllSummaryOutput {
            rows: vec![CancelAllSummary {
                coin: plan.summary_coin,
                cancelled_orders: 0,
            }],
        });
        return Ok(());
    };
    context.consent(ConsentKind::CancelAll, None, |target| {
        confirm_cancel_all(args.coin.as_deref(), order_ids.len(), target)
    })?;

    let response = actions::send_l1_action_raw(
        context.api,
        context.signer,
        action,
        actions::nonce_now(),
        context.vault_address,
        "cancel-all failed",
    )
    .await?;
    let statuses = decode_statuses(response, "cancel-all")?;
    let rows: Vec<CancelLeg> = pair_statuses(&order_ids, statuses)?
        .into_iter()
        .map(|(order_id, status)| CancelLeg::from_status(*order_id, status))
        .collect();
    let failures: Vec<Rejected> = rows.iter().filter_map(|row| row.error.clone()).collect();
    if !failures.is_empty() {
        return finish_with_failures(
            CancelLegsOutput { rows },
            BatchVerb::CancelAll,
            failures,
            context.inv,
        );
    }

    context.inv.print(&CancelAllSummaryOutput {
        rows: vec![CancelAllSummary {
            coin: plan.summary_coin,
            cancelled_orders: order_ids.len(),
        }],
    });
    Ok(())
}

async fn modify(
    context: OrderSubmissionContext<'_>,
    resolver: &AssetResolver,
    identifier: &OrderIdentifier,
    args: &ModifyArgs,
) -> Result<(), anyhow::Error> {
    let plan = prepare_modify_order_plan(
        context.api.sdk(),
        resolver,
        context.account(),
        identifier,
        args,
    )
    .await?;
    let response = actions::send_l1_action_raw(
        context.api,
        context.signer,
        plan.action,
        actions::nonce_now(),
        context.vault_address,
        "modify failed",
    )
    .await?;
    let statuses = decode_statuses(response, "modify")?;
    ensure_action_statuses_ok(&statuses, "modify order")?;
    let mut confirmation = plan.confirmation;
    confirmation.order_id = modification_result_oid(&statuses).or(confirmation.order_id);

    context.inv.print(&ModifyConfirmationOutput {
        rows: vec![confirmation],
    });
    Ok(())
}

async fn twap_create(
    context: OrderSubmissionContext<'_>,
    plan: TwapCreatePlan,
) -> Result<(), anyhow::Error> {
    clear_to_send(
        context,
        Gate::for_position(ConsentKind::Twap, &plan.perp, plan.margin_mode),
        |target| confirm_mainnet_twap(&plan, target),
    )
    .await?;
    let response = actions::send_raw_l1_json_action(
        context.api,
        context.signer,
        &plan.action,
        actions::RawL1ActionMetadata::new(actions::nonce_now())
            .with_vault_address(context.vault_address),
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
) -> Result<(), anyhow::Error> {
    let response = actions::send_raw_l1_json_action(
        context.api,
        context.signer,
        &plan.action,
        actions::RawL1ActionMetadata::new(actions::nonce_now())
            .with_vault_address(context.vault_address),
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
) -> Result<(), anyhow::Error> {
    context.consent(ConsentKind::ScheduleCancel, None, |target| {
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
            actions::RawL1ActionMetadata::new(nonce).with_vault_address(context.vault_address),
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
