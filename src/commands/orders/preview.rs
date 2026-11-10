//! What an order command shows under `--dry-run`: the action it would execute
//! and the args it would run with. Plain data, built by the planners and
//! finished and printed by [`OrderRun`](super::run::OrderRun).

use chrono::Utc;
use hypersdk::Decimal;
use hypersdk::hypercore::types::{OrderGrouping, OrderTypePlacement, TpSl};

use super::args::{
    BatchCreateArgs, CreateArgs, CreateTpslGroupingArg, TriggerPriceSpec, TwapCancelArgs,
    TwapCreateArgs,
};
use super::cloid::cloid_from_wire;
use super::planning::{
    ScheduleCancelPlan, TwapCancelPlan, TwapCreatePlan, position_tpsl_batch,
    position_tpsl_triggers, trigger_specs_need_entry, truncate_triggers,
};
use super::prepared::{
    CreateOrderSubmission, PreparedOrder, PreparedOrderBatch, PreparedTpslBatch, TpslLegKind,
};
use super::validation::{ClosingSize, MarketPrecision, TpslSpec};
use super::vocab::{CreateOrderType, OrderSide, TpslGroupingArg};
use crate::assets::{Market, PerpAsset};
use crate::errors::CliError;
use crate::margin::{MarginModeArg, MarginPlan};

pub(super) struct OrderDryRunPlan {
    would_execute: &'static str,
    args: serde_json::Value,
    /// Whether `args` names the acting account (`on_behalf_of`). Every order preview does
    /// but chase's and bracket's, whose documents do not carry the key at their top level.
    names_account: bool,
}

impl OrderDryRunPlan {
    pub(super) fn new(would_execute: &'static str, args: serde_json::Value) -> Self {
        Self {
            would_execute,
            args,
            names_account: true,
        }
    }

    pub(super) fn unattributed(would_execute: &'static str, args: serde_json::Value) -> Self {
        Self {
            names_account: false,
            ..Self::new(would_execute, args)
        }
    }

    pub(super) fn with_account(mut self, on_behalf_of: Option<&str>) -> Self {
        if self.names_account {
            self.insert("on_behalf_of", serde_json::json!(on_behalf_of));
        }
        self
    }

    pub(super) fn with_margin_mode(mut self, mode: MarginModeArg) -> Self {
        self.insert("margin_mode", serde_json::json!(mode.to_string()));
        self
    }

    pub(super) fn with_extra(mut self, key: &str, value: serde_json::Value) -> Self {
        self.insert(key, value);
        self
    }

    fn insert(&mut self, key: &str, value: serde_json::Value) {
        if let Some(object) = self.args.as_object_mut() {
            object.insert(key.to_string(), value);
        }
    }

    #[must_use]
    pub(super) fn would_execute(&self) -> &'static str {
        self.would_execute
    }

    #[must_use]
    pub(super) fn into_args(self) -> serde_json::Value {
        self.args
    }
}

/// The flags a create-like preview echoes back: what the user typed, which is
/// not always what was planned. `orders create` takes them from its args and a
/// bracket entry names its own.
pub(super) struct OrderEcho<'a> {
    pub(super) coin: &'a str,
    pub(super) dex: Option<&'a str>,
    pub(super) side: OrderSide,
    pub(super) order_type: CreateOrderType,
    pub(super) cloid: Option<&'a str>,
    pub(super) max_slippage_bps: u16,
    pub(super) take_profit: Option<Decimal>,
    pub(super) stop_loss: Option<Decimal>,
    pub(super) grouping: Option<CreateTpslGroupingArg>,
    pub(super) builder_address: Option<&'a str>,
    pub(super) builder_fee_rate: Option<&'a str>,
}

impl<'a> OrderEcho<'a> {
    /// The echo of an order that names only what every order has.
    pub(super) fn new(
        coin: &'a str,
        dex: Option<&'a str>,
        side: OrderSide,
        order_type: CreateOrderType,
        max_slippage_bps: u16,
    ) -> Self {
        Self {
            coin,
            dex,
            side,
            order_type,
            cloid: None,
            max_slippage_bps,
            take_profit: None,
            stop_loss: None,
            grouping: None,
            builder_address: None,
            builder_fee_rate: None,
        }
    }
}

impl<'a> From<&'a CreateArgs> for OrderEcho<'a> {
    fn from(args: &'a CreateArgs) -> Self {
        Self {
            coin: &args.coin,
            dex: args.dex.as_deref(),
            side: args.side,
            order_type: args.order_type,
            cloid: args.cloid.as_deref(),
            max_slippage_bps: args.max_slippage_bps,
            take_profit: args.take_profit,
            stop_loss: args.stop_loss,
            grouping: args.grouping,
            builder_address: args.builder.as_deref(),
            builder_fee_rate: args.builder_fee_rate.as_deref(),
        }
    }
}

/// `margin` is the margin plan its `margin_mode` shows: the one it was planned with,
/// or the plan once the account was read.
pub(super) fn create_dry_run_preview(
    submission: &CreateOrderSubmission,
    echo: &OrderEcho<'_>,
    margin: Option<&MarginPlan>,
) -> Result<serde_json::Value, CliError> {
    let mut preview = prepared_order_dry_run_args(submission.parent(), echo, margin)?;
    if let CreateOrderSubmission::NormalTpsl(order) = submission {
        append_tpsl_preview(&mut preview, &order.batch)?;
    }
    Ok(preview)
}

pub(super) fn create_dry_run_plan(
    submission: &CreateOrderSubmission,
    echo: &OrderEcho<'_>,
    margin: Option<&MarginPlan>,
) -> Result<OrderDryRunPlan, CliError> {
    Ok(OrderDryRunPlan::new(
        "submit_order",
        create_dry_run_preview(submission, echo, margin)?,
    ))
}

pub(super) fn scale_dry_run_plan(
    prepared: &PreparedOrderBatch,
    margin: Option<&MarginPlan>,
) -> OrderDryRunPlan {
    // A batch on a market with no margin previews cross.
    let mode = margin.map_or(MarginModeArg::Cross, MarginPlan::mode);
    OrderDryRunPlan::new("submit_order_batch", order_batch_dry_run_args(prepared))
        .with_margin_mode(mode)
}

pub(super) fn batch_create_dry_run_plan(
    prepared: &PreparedOrderBatch,
    args: &BatchCreateArgs,
) -> OrderDryRunPlan {
    OrderDryRunPlan::new("submit_order_batch", order_batch_dry_run_args(prepared))
        .with_extra("orders_file", serde_json::json!(args.orders_file))
}

pub(super) fn twap_create_dry_run_plan(
    plan: &TwapCreatePlan,
    args: &TwapCreateArgs,
    margin: &MarginPlan,
) -> OrderDryRunPlan {
    OrderDryRunPlan::new(
        "create_twap_order",
        serde_json::json!({
            "coin": args.coin,
            "dex": args.dex,
            "side": plan.side,
            "size": plan.size.to_string(),
            "duration": plan.duration_seconds,
            "duration_minutes": plan.duration_minutes,
            "randomize": plan.randomize,
            "asset_id": plan.perp.index,
            "resolved_asset": plan.coin,
        }),
    )
    .with_margin_mode(margin.mode())
}

pub(super) fn twap_cancel_dry_run_plan(
    plan: &TwapCancelPlan,
    args: &TwapCancelArgs,
) -> OrderDryRunPlan {
    OrderDryRunPlan::new(
        "cancel_twap_order",
        serde_json::json!({
            "twap_id": plan.twap_id,
            "coin": args.coin,
            "dex": args.dex,
            "resolved_asset": plan.coin,
        }),
    )
}

pub(super) fn schedule_cancel_dry_run_plan(
    plan: &ScheduleCancelPlan,
    now: chrono::DateTime<Utc>,
) -> OrderDryRunPlan {
    let preview = match plan.scheduled(now) {
        Some(scheduled) => serde_json::json!({
            "mode": "set",
            "in_duration_ms": scheduled.in_seconds * 1000,
            "scheduled_time": scheduled.at.to_rfc3339(),
            "scheduled_time_ms": scheduled.at_ms(),
        }),
        None => serde_json::json!({ "mode": "clear" }),
    };
    OrderDryRunPlan::new("schedule_dead_mans_switch", preview)
}

/// The trigger fields a preview shows for an order placement: all `null` for
/// a limit order.
struct TriggerFields {
    is_market: Option<bool>,
    trigger_px: Option<Decimal>,
    tpsl: Option<TpSl>,
}

impl TriggerFields {
    fn of(placement: &OrderTypePlacement) -> Self {
        match placement {
            OrderTypePlacement::Trigger {
                is_market,
                trigger_px,
                tpsl,
            } => Self {
                is_market: Some(*is_market),
                trigger_px: Some(*trigger_px),
                tpsl: Some(*tpsl),
            },
            OrderTypePlacement::Limit { .. } => Self {
                is_market: None,
                trigger_px: None,
                tpsl: None,
            },
        }
    }
}

fn prepared_order_dry_run_args(
    prepared: &PreparedOrder,
    echo: &OrderEcho<'_>,
    margin: Option<&MarginPlan>,
) -> Result<serde_json::Value, CliError> {
    let trigger = TriggerFields::of(&prepared.request.order_type);

    let mut value = serde_json::json!({
        "coin": echo.coin,
        "dex": echo.dex,
        "side": echo.side.to_string(),
        "type": echo.order_type.to_string(),
        "asset_id": prepared.request.asset,
        "resolved_asset": prepared.coin.clone(),
        "limit_px": prepared.request.limit_px.to_string(),
        "trigger_px": trigger.trigger_px.map(|value| value.to_string()),
        "is_market": trigger.is_market,
        "tpsl": trigger.tpsl,
        "size": prepared.request.sz.to_string(),
        "amount": prepared.amount.as_ref().map(|amount| amount.value.to_string()),
        "amount_unit": prepared.amount.as_ref().map(|amount| amount.unit.clone()),
        "tif": prepared.tif.map(|tif| tif.to_string()),
        "reduce_only": prepared.request.reduce_only,
        "cloid": echo.cloid,
        "max_slippage_bps": echo.max_slippage_bps,
        "take_profit": echo.take_profit.map(|value| value.to_string()),
        "stop_loss": echo.stop_loss.map(|value| value.to_string()),
        "grouping": echo.grouping.map(|grouping| TpslGroupingArg::from(grouping).to_string()),
        "builder": prepared.builder.as_ref().map(|builder| serde_json::json!(builder)),
        "builder_address": echo.builder_address,
        "builder_fee_rate": echo.builder_fee_rate,
    });
    if let Some(rate) = prepared.priority_rate {
        insert_priority_rate(&mut value, rate);
    }
    match &prepared.market {
        Market::Outcome(outcome) => {
            let notation = outcome.parse()?;
            if let Some(object) = value.as_object_mut() {
                object.insert("outcome".to_string(), serde_json::json!({
                    "outcome": notation.outcome,
                    "side": notation.side,
                    "encoding": notation.encoding,
                    "asset_id": prepared.request.asset,
                    "notation": echo.coin,
                    "live_submission": "enabled: outcome order encoding is verified; use explicit --price and --size"
                }));
            }
        }
        Market::Perp(_) => {
            if let Some(margin) = margin {
                value["margin_mode"] = serde_json::json!(margin.mode().to_string());
            }
        }
        Market::Spot(_) => {}
    }
    Ok(value)
}

fn order_batch_dry_run_args(prepared: &PreparedOrderBatch) -> serde_json::Value {
    let legs = prepared
        .legs
        .iter()
        .enumerate()
        .map(|(leg_index, order)| {
            let trigger = TriggerFields::of(&order.request.order_type);
            serde_json::json!({
                "leg_index": leg_index,
                "coin": order.coin,
                "side": order.side.to_string(),
                "type": order.order_type.to_string(),
                "asset_id": order.request.asset,
                "resolved_asset": order.coin,
                "limit_px": order.request.limit_px.to_string(),
                "trigger_px": trigger.trigger_px.map(|price| price.to_string()),
                "is_market": trigger.is_market,
                "tpsl": trigger.tpsl,
                "price": order.price().to_string(),
                "size": order.size().to_string(),
                "amount": order.amount.as_ref().map(|amount| amount.value.to_string()),
                "amount_unit": order.amount.as_ref().map(|amount| amount.unit.clone()),
                "tif": order.tif.map(|tif| tif.to_string()),
                "reduce_only": order.request.reduce_only,
                "cloid": cloid_from_wire(order.request.cloid).map(|cloid| cloid.to_string()),
            })
        })
        .collect::<Vec<_>>();
    let mut preview = serde_json::json!({
        "grouping": "na",
        "order_count": prepared.legs.len(),
        "orders": legs,
    });
    if let OrderGrouping::PriorityRate(rate) = prepared.batch.grouping {
        insert_priority_rate(&mut preview, rate);
    }
    preview
}

/// Name the order write priority a preview would send: the grouping is the
/// rate, and the wire form is the SDK's own (`{ "p": rate }`).
fn insert_priority_rate(preview: &mut serde_json::Value, rate: u32) {
    if let Some(object) = preview.as_object_mut() {
        object.insert("grouping".to_string(), "priority-rate".into());
        object.insert(
            "grouping_wire".to_string(),
            serde_json::json!(OrderGrouping::PriorityRate(rate)),
        );
        object.insert("priority_rate".to_string(), rate.into());
    }
}

/// `orders tpsl` dry-run details: what the live command would send, without a
/// signer or a position lookup. The legs come from the same
/// [`position_tpsl_triggers`] and [`position_tpsl_batch`] the live order uses.
/// `margin` is the plan its `margin_mode` shows.
pub(super) fn tpsl_dry_run_plan(
    perp: &PerpAsset,
    spec: &TpslSpec,
    margin: &MarginPlan,
) -> Result<OrderDryRunPlan, CliError> {
    let (absolute_tp, absolute_sl) =
        absolute_trigger_prices(spec.take_profit.as_ref(), spec.stop_loss.as_ref());
    let (absolute_tp, absolute_sl) = truncate_triggers(perp, absolute_tp, absolute_sl)?;
    let (inferred_side, fixed_size) = match spec.close {
        ClosingSize::Fixed { side, size } => (Some(side), Some(size)),
        ClosingSize::CurrentPosition => (
            infer_close_side_from_tpsl_prices(absolute_tp, absolute_sl),
            None,
        ),
    };
    let needs_entry = trigger_specs_need_entry(spec.take_profit.as_ref(), spec.stop_loss.as_ref());
    let grouping = TpslGroupingArg::PositionTpsl;
    let mut preview = serde_json::json!({
        "coin": spec.coin,
        "dex": spec.dex,
        "take_profit": spec.take_profit.as_ref().map(TriggerPriceSpec::display),
        "stop_loss": spec.stop_loss.as_ref().map(TriggerPriceSpec::display),
        "grouping": grouping.to_string(),
        "grouping_wire": grouping.wire_value(),
        "resolved_asset": perp.display_name(),
        "side": inferred_side.map(|side| side.to_string()),
        "size": fixed_size.map(|value| value.to_string()),
        "size_mode": if fixed_size.is_some() { "fixed" } else { "current_position" },
    });
    preview["margin_mode"] = serde_json::json!(margin.mode().to_string());

    match spec.close {
        ClosingSize::Fixed { side, size } => {
            // Percent and entry triggers need the position's entry price, which
            // a dry run does not read: it previews no legs for them.
            if !needs_entry {
                let size = MarketPrecision::of_perp(perp).size(size, &perp.display_name())?;
                let (take_profit, stop_loss) = position_tpsl_triggers(perp, spec, side, None)?;
                let batch =
                    position_tpsl_batch(perp, side, size, take_profit, stop_loss, spec.cloid);
                append_tpsl_preview(&mut preview, &batch)?;
            }
        }
        ClosingSize::CurrentPosition => {
            if needs_entry {
                preview.as_object_mut().expect("preview object").insert(
                    "resolution".to_string(),
                    serde_json::json!("percent_or_entry_requires_live_position"),
                );
            }
            append_position_sized_tpsl_preview(
                &mut preview,
                perp.index,
                inferred_side,
                absolute_tp,
                absolute_sl,
            );
        }
    }
    Ok(OrderDryRunPlan::new("submit_position_tpsl", preview))
}

fn append_tpsl_preview(
    preview: &mut serde_json::Value,
    prepared: &PreparedTpslBatch,
) -> Result<(), CliError> {
    let mut parent = None;
    let mut children = Vec::new();
    let mut legs = Vec::new();

    for leg in &prepared.legs {
        let order = serde_json::to_value(&leg.request)
            .map_err(|err| CliError::Internal(anyhow::anyhow!("serialize TP/SL leg: {err}")))?;
        let entry = serde_json::json!({
            "leg": leg.leg.to_string(),
            "coin": leg.coin,
            "side": leg.side.to_string(),
            "type": leg.order_type.to_string(),
            "price": leg.price.to_string(),
            "size": leg.size.to_string(),
            "tif": leg.tif.map(|tif| tif.to_string()),
            "reduce_only": leg.reduce_only,
            "warning": leg.warning,
            "order": order,
        });
        if matches!(leg.leg, TpslLegKind::Parent) {
            parent = Some(entry.clone());
        } else {
            children.push(entry.clone());
        }
        legs.push(entry);
    }

    let object = preview
        .as_object_mut()
        .ok_or_else(|| CliError::Internal(anyhow::anyhow!("dry-run preview must be an object")))?;
    object.insert(
        "batch_grouping".to_string(),
        serde_json::json!(prepared.grouping.to_string()),
    );
    object.insert(
        "batch_grouping_wire".to_string(),
        serde_json::json!(prepared.grouping.wire_value()),
    );
    object.insert(
        "batch_order".to_string(),
        serde_json::to_value(&prepared.batch)
            .map_err(|err| CliError::Internal(anyhow::anyhow!("serialize TP/SL batch: {err}")))?,
    );
    object.insert("legs".to_string(), serde_json::Value::Array(legs));
    object.insert(
        "tpsl_children".to_string(),
        serde_json::Value::Array(children),
    );
    if let Some(parent) = parent {
        object.insert("parent_order".to_string(), parent);
    }
    Ok(())
}

/// Preview of a TP/SL sized to the position the live command will read. The
/// size is not known yet, so the legs show `current_position` instead.
fn append_position_sized_tpsl_preview(
    preview: &mut serde_json::Value,
    asset: usize,
    inferred_side: Option<OrderSide>,
    take_profit: Option<Decimal>,
    stop_loss: Option<Decimal>,
) {
    let grouping = TpslGroupingArg::PositionTpsl;
    let mut position_tp_order = serde_json::Value::Null;
    let mut position_sl_order = serde_json::Value::Null;
    let mut orders = Vec::new();
    let mut legs = Vec::new();
    let mut children = Vec::new();
    for (tpsl, price) in [(TpSl::Tp, take_profit), (TpSl::Sl, stop_loss)] {
        let Some(price) = price else {
            continue;
        };
        let tpsl_wire = serde_json::json!(tpsl);
        let child = serde_json::json!({
            "asset": asset,
            "is_buy": inferred_side.map(OrderSide::is_buy),
            "limit_px": "0",
            "sz": null,
            "reduce_only": true,
            "trigger_px": price.to_string(),
            "is_market": true,
            "tpsl": tpsl_wire.clone(),
        });
        match tpsl {
            TpSl::Tp => position_tp_order = child.clone(),
            TpSl::Sl => position_sl_order = child.clone(),
        }
        legs.push(serde_json::json!({
            "leg": TpslLegKind::of(tpsl).to_string(),
            "side": inferred_side.map(|side| side.to_string()),
            "type": CreateOrderType::market_trigger(tpsl).to_string(),
            "price": price.to_string(),
            "size": "current_position",
            "reduce_only": true,
            "grouping": grouping.to_string(),
            "grouping_wire": grouping.wire_value(),
            "order": child.clone(),
        }));
        children.push(child);
        orders.push(serde_json::json!({
            "a": asset,
            "b": inferred_side.map(OrderSide::is_buy),
            "p": price.to_string(),
            "s": "current_position",
            "r": true,
            "t": {
                "trigger": {
                    "isMarket": true,
                    "triggerPx": price.to_string(),
                    "tpsl": tpsl_wire,
                }
            }
        }));
    }

    if let Some(object) = preview.as_object_mut() {
        object.insert(
            "batch_grouping".to_string(),
            serde_json::json!(grouping.to_string()),
        );
        object.insert(
            "batch_grouping_wire".to_string(),
            serde_json::json!(grouping.wire_value()),
        );
        object.insert("position_size_lookup".to_string(), serde_json::json!(true));
        object.insert(
            "position_lookup_note".to_string(),
            serde_json::json!("live submission resolves current position size before signing"),
        );
        object.insert("legs".to_string(), serde_json::Value::Array(legs));
        object.insert(
            "tpsl_children".to_string(),
            serde_json::Value::Array(children),
        );
        object.insert(
            "batch_order_preview".to_string(),
            serde_json::json!({
                "grouping": grouping.wire_value(),
                "orders": orders,
                "size_source": "current_position",
            }),
        );
        object.insert("position_tp_order".to_string(), position_tp_order);
        object.insert("position_sl_order".to_string(), position_sl_order);
    }
}

fn infer_close_side_from_tpsl_prices(
    take_profit: Option<Decimal>,
    stop_loss: Option<Decimal>,
) -> Option<OrderSide> {
    match (take_profit, stop_loss) {
        (Some(tp), Some(sl)) if tp > sl => Some(OrderSide::Sell),
        (Some(tp), Some(sl)) if tp < sl => Some(OrderSide::Buy),
        _ => None,
    }
}

fn absolute_trigger_prices(
    take_profit: Option<&TriggerPriceSpec>,
    stop_loss: Option<&TriggerPriceSpec>,
) -> (Option<Decimal>, Option<Decimal>) {
    (
        take_profit.and_then(TriggerPriceSpec::absolute),
        stop_loss.and_then(TriggerPriceSpec::absolute),
    )
}
