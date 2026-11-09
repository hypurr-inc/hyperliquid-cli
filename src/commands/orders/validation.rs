use std::path::Path;
use std::time::Duration;

use hypersdk::hypercore::types::{Side, TpSl};
use hypersdk::hypercore::{Cloid, PriceTick};
use hypersdk::{Address, Decimal};
use rust_decimal::RoundingStrategy;

use super::planning::order_identifier;
use super::{
    BPS_DENOMINATOR, BatchCreateArgs, BatchCreateOrder, BracketArgs, CreateArgs, CreateOrderType,
    CreateTpslGroupingArg, MAX_BATCH_ORDER_COUNT, MAX_MARKET_ORDER_SLIPPAGE_BPS,
    MIN_MARKET_ORDER_SLIPPAGE_BPS, MarginModeArg, ModifyArgs, OrderSide, ScaleArgs, TifArg,
    TpslArgs, TriggerPriceSpec, TwapCreateArgs, parse_cloid,
};
use crate::assets::Market;
use crate::builder_fee::MaxFeeRate;
use crate::errors::CliError;
use crate::input_hardening::{FilePolicy, read_json_file};
use crate::wire::{self, AddressRole};

/// How the exchange constrains prices and sizes on one market.
///
/// The price rule is the SDK's [`PriceTick`] (five significant figures, at most
/// `MAX_DECIMALS - szDecimals` decimals, integers exempt); the size rule is the
/// lot size `szDecimals`. Every price and size the CLI sends goes through this
/// type, so an over-precise value is truncated toward zero (never rounded up)
/// instead of being rejected after submission. The one side-aware exception is
/// [`MarketPrecision::marketable_price`], which must round toward the book.
#[derive(Debug, Clone, Copy)]
pub(super) struct MarketPrecision {
    sz_decimals: u32,
    price_tick: PriceTick,
}

impl MarketPrecision {
    pub(super) fn of(market: &Market) -> Self {
        match market {
            Market::Perp(perp) => Self::perp(perp.sz_decimals),
            Market::Spot(spot) => Self::spot(spot.base_sz_decimals),
            // Outcome size decimals are not verified; they trade on the spot tick grid in whole lots.
            Market::Outcome(_) => Self::spot(0),
        }
    }

    /// A market whose `szDecimals` exceeds the price's decimal budget (6 for
    /// perps, 8 for spot) has no decimals left: it takes whole-number prices.
    /// [`PriceTick`] treats that as an invalid market, so the budget is
    /// clamped here, once.
    pub(super) fn perp(sz_decimals: u32) -> Self {
        Self {
            sz_decimals,
            price_tick: PriceTick::for_perp(i64::from(sz_decimals.min(6))),
        }
    }

    pub(super) fn spot(sz_decimals: u32) -> Self {
        Self {
            sz_decimals,
            price_tick: PriceTick::for_spot(i64::from(sz_decimals.min(8))),
        }
    }

    /// The price a market order sends: `mid` moved by `max_slippage_bps`
    /// against the order's side, then rounded toward the book so it stays
    /// marketable.
    pub(super) fn marketable_price(
        self,
        mid: Decimal,
        side: OrderSide,
        max_slippage_bps: u16,
    ) -> Result<Decimal, CliError> {
        let multiplier = match side {
            OrderSide::Buy => {
                Decimal::from(BPS_DENOMINATOR + i64::from(max_slippage_bps))
                    / Decimal::from(BPS_DENOMINATOR)
            }
            OrderSide::Sell => {
                Decimal::from(BPS_DENOMINATOR - i64::from(max_slippage_bps))
                    / Decimal::from(BPS_DENOMINATOR)
            }
        };
        let limit_px = mid * multiplier;
        let sdk_side = if side.is_buy() { Side::Bid } else { Side::Ask };
        self.price_tick
            .round_by_side(sdk_side, limit_px, false)
            .ok_or_else(|| {
                CliError::Configuration(format!(
                    "derived market order price {limit_px} could not be rounded to a valid tick"
                ))
            })
    }

    /// Truncate a price to the tick grid; a price below one tick is rejected.
    pub(super) fn price(self, price: Decimal, coin: &str) -> Result<Decimal, CliError> {
        let tick = self.price_tick.tick_for(price).ok_or_else(|| {
            CliError::Configuration(format!(
                "price {price} for {coin} cannot be placed on this market's tick grid"
            ))
        })?;
        let truncated =
            price.round_dp_with_strategy(tick.normalize().scale(), RoundingStrategy::ToZero);
        if truncated <= Decimal::ZERO {
            return Err(CliError::Configuration(format!(
                "price for {coin} is below the smallest valid tick; it truncates to zero"
            )));
        }
        Ok(truncated)
    }

    /// Truncate a size to the lot size; a size below one lot is rejected.
    pub(super) fn size(self, size: Decimal, coin: &str) -> Result<Decimal, CliError> {
        let truncated =
            size.round_dp_with_strategy(self.sz_decimals.min(28), RoundingStrategy::ToZero);
        if truncated <= Decimal::ZERO {
            return Err(CliError::Configuration(format!(
                "size is too small for {coin}; it truncates to zero at {} decimal places",
                self.sz_decimals
            )));
        }
        Ok(truncated)
    }
}

/// One order to place, in the terms the planner needs. `orders create`, `buy`,
/// and `sell` build it with [`OrderSpec::from_create`], which is the only place
/// their flags are validated; generated legs (scale, batch-create) build it
/// with [`OrderSpec::limit_leg`].
#[derive(Debug, Clone)]
pub(crate) struct OrderSpec {
    pub(super) coin: String,
    pub(super) dex: Option<String>,
    pub(super) side: OrderSide,
    pub(super) kind: OrderKind,
    pub(super) reduce_only: bool,
    pub(super) cloid: Option<Cloid>,
    pub(super) builder: BuilderRequest,
    pub(super) margin_mode: Option<MarginModeArg>,
    pub(super) tpsl: Option<AttachedTpsl>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum OrderKind {
    Limit {
        price: Decimal,
        size: Decimal,
        tif: TifArg,
    },
    /// Stop-loss or take-profit: fires at `trigger_price`, then places a market
    /// order (`limit_price` is `None`) or a limit order at `limit_price`.
    Trigger {
        tpsl: TpSl,
        trigger_price: Decimal,
        limit_price: Option<Decimal>,
        size: Decimal,
    },
    /// Priced from the mid with a slippage bound, sized by base size or by
    /// quote amount.
    Market {
        sizing: MarketSizing,
        max_slippage_bps: u16,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) enum MarketSizing {
    Size(Decimal),
    Amount(Decimal),
}

impl OrderKind {
    pub(super) fn order_type(&self) -> CreateOrderType {
        match self {
            Self::Limit { .. } => CreateOrderType::Limit,
            Self::Market { .. } => CreateOrderType::Market,
            Self::Trigger {
                tpsl,
                limit_price: None,
                ..
            } => CreateOrderType::market_trigger(*tpsl),
            Self::Trigger {
                tpsl,
                limit_price: Some(_),
                ..
            } => CreateOrderType::limit_trigger(*tpsl),
        }
    }

    fn is_trigger(&self) -> bool {
        matches!(self, Self::Trigger { .. })
    }
}

/// The builder fee an order should carry. `Default` defers to the configured
/// default builder, if any; `None` carries no builder fee and never consults
/// that configuration (generated batch legs are sent without one).
#[derive(Debug, Clone, Copy)]
pub(super) enum BuilderRequest {
    None,
    Default,
    Explicit { address: Address, fee: u64 },
}

impl BuilderRequest {
    pub(super) fn from_flags(
        builder: Option<&str>,
        fee_rate: Option<&str>,
    ) -> Result<Self, CliError> {
        match (builder, fee_rate) {
            (Some(raw_builder), Some(raw_fee)) => Ok(Self::Explicit {
                address: wire::address(raw_builder, AddressRole::Builder)?,
                fee: MaxFeeRate::parse(raw_fee)?.tenths_bps(),
            }),
            (Some(_), None) => Err(CliError::Configuration(
                "orders create --builder requires --builder-fee-rate".to_string(),
            )),
            (None, Some(_)) => Err(CliError::Configuration(
                "orders create --builder-fee-rate requires --builder".to_string(),
            )),
            (None, None) => Ok(Self::Default),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AttachedTpsl {
    pub(super) grouping: CreateTpslGroupingArg,
    pub(super) take_profit: Option<Decimal>,
    pub(super) stop_loss: Option<Decimal>,
}

impl OrderSpec {
    pub(crate) fn from_create(args: &CreateArgs) -> Result<Self, CliError> {
        let cloid = args.cloid.as_deref().map(parse_cloid).transpose()?;
        validate_create_order_type_flags(args)?;
        positive_price(args.price, "price")?;
        positive_price(args.trigger_price, "trigger-price")?;
        positive_quantity(args.size, "size")?;
        positive_quantity(args.amount, "amount")?;
        positive_price(args.take_profit, "take-profit")?;
        positive_price(args.stop_loss, "stop-loss")?;
        validate_market_slippage(args.max_slippage_bps)?;
        let tpsl = attached_tpsl(args)?;
        let builder =
            BuilderRequest::from_flags(args.builder.as_deref(), args.builder_fee_rate.as_deref())?;
        Ok(Self {
            coin: args.coin.clone(),
            dex: args.dex.clone(),
            side: args.side,
            kind: order_kind(args)?,
            reduce_only: args.reduce_only,
            cloid,
            builder,
            margin_mode: args.margin_mode,
            tpsl,
        })
    }

    /// A plain limit order, as generated by scale and batch-create.
    pub(crate) fn limit_leg(
        coin: &str,
        dex: Option<&str>,
        side: OrderSide,
        price: Decimal,
        size: Decimal,
        tif: TifArg,
    ) -> Result<Self, CliError> {
        positive_price(Some(price), "price")?;
        positive_quantity(Some(size), "size")?;
        Ok(Self {
            coin: coin.to_string(),
            dex: dex.map(str::to_string),
            side,
            kind: OrderKind::Limit { price, size, tif },
            reduce_only: false,
            cloid: None,
            builder: BuilderRequest::None,
            margin_mode: None,
            tpsl: None,
        })
    }

    pub(crate) fn reduce_only(mut self, reduce_only: bool) -> Self {
        self.reduce_only = reduce_only;
        self
    }

    pub(crate) fn with_cloid(mut self, cloid: Option<Cloid>) -> Self {
        self.cloid = cloid;
        self
    }

    pub(crate) fn with_margin_mode(mut self, margin_mode: Option<MarginModeArg>) -> Self {
        self.margin_mode = margin_mode;
        self
    }

    pub(super) fn with_builder(mut self, builder: BuilderRequest) -> Self {
        self.builder = builder;
        self
    }

    /// A market order priced from the mid, as placed by a bracket entry.
    pub(crate) fn market(
        coin: &str,
        dex: Option<&str>,
        side: OrderSide,
        sizing: MarketSizing,
        max_slippage_bps: u16,
    ) -> Result<Self, CliError> {
        match sizing {
            MarketSizing::Size(size) => positive_quantity(Some(size), "size")?,
            MarketSizing::Amount(amount) => positive_quantity(Some(amount), "amount")?,
        }
        validate_market_slippage(max_slippage_bps)?;
        Ok(Self {
            coin: coin.to_string(),
            dex: dex.map(str::to_string),
            side,
            kind: OrderKind::Market {
                sizing,
                max_slippage_bps,
            },
            reduce_only: false,
            cloid: None,
            builder: BuilderRequest::None,
            margin_mode: None,
            tpsl: None,
        })
    }
}

fn order_kind(args: &CreateArgs) -> Result<OrderKind, CliError> {
    let trigger = |tpsl: TpSl, limit: bool| -> Result<OrderKind, CliError> {
        if limit {
            let [trigger_price, price, size] = require_flags(
                [
                    (args.trigger_price, "--trigger-price"),
                    (args.price, "--price"),
                    (args.size, "--size"),
                ],
                "limit trigger orders",
            )?;
            Ok(OrderKind::Trigger {
                tpsl,
                trigger_price,
                limit_price: Some(price),
                size,
            })
        } else {
            let trigger_price = args.trigger_price.or(args.price).ok_or_else(|| {
                CliError::Configuration(
                    "orders create requires --trigger-price or --price for market trigger orders"
                        .to_string(),
                )
            })?;
            let [size] = require_flags([(args.size, "--size")], "market trigger orders")?;
            Ok(OrderKind::Trigger {
                tpsl,
                trigger_price,
                limit_price: None,
                size,
            })
        }
    };
    match args.order_type {
        CreateOrderType::Limit => {
            let [price, size] = require_flags(
                [(args.price, "--price"), (args.size, "--size")],
                "limit orders",
            )?;
            Ok(OrderKind::Limit {
                price,
                size,
                tif: args.tif,
            })
        }
        CreateOrderType::StopLoss => trigger(TpSl::Sl, false),
        CreateOrderType::TakeProfit => trigger(TpSl::Tp, false),
        CreateOrderType::StopLimit => trigger(TpSl::Sl, true),
        CreateOrderType::TakeLimit => trigger(TpSl::Tp, true),
        CreateOrderType::Market => {
            let sizing = match (args.amount, args.size) {
                (Some(amount), None) => MarketSizing::Amount(amount),
                (None, Some(size)) => MarketSizing::Size(size),
                (Some(_), Some(_)) => {
                    return Err(CliError::Configuration(
                        "orders create --type market accepts --amount or --size, not both"
                            .to_string(),
                    ));
                }
                (None, None) => {
                    return Err(CliError::Configuration(
                        "orders create requires --amount or --size for market orders.\n  \
                         hyperliquid --format json --dry-run buy --coin BTC --size 0.001"
                            .to_string(),
                    ));
                }
            };
            Ok(OrderKind::Market {
                sizing,
                max_slippage_bps: args.max_slippage_bps,
            })
        }
    }
}

fn attached_tpsl(args: &CreateArgs) -> Result<Option<AttachedTpsl>, CliError> {
    let has_legs = args.take_profit.is_some() || args.stop_loss.is_some();
    match (has_legs, args.grouping) {
        (false, Some(_)) => Err(CliError::Configuration(
            "orders create --grouping requires --take-profit or --stop-loss".to_string(),
        )),
        (true, None) => Err(CliError::Configuration(
            "orders create --take-profit/--stop-loss require --grouping normal-tpsl".to_string(),
        )),
        (false, None) => Ok(None),
        (true, Some(grouping)) => {
            if args.order_type.is_trigger() {
                return Err(CliError::Configuration(
                    "orders create TP/SL children require --type limit or --type market; use --type stop-loss or --type take-profit only for standalone trigger orders"
                        .to_string(),
                ));
            }
            Ok(Some(AttachedTpsl {
                grouping,
                take_profit: args.take_profit,
                stop_loss: args.stop_loss,
            }))
        }
    }
}

/// The one owner of what a non-perpetual market cannot do. Perpetual markets
/// accept every order the flags can describe.
pub(super) fn validate_order_for_market(spec: &OrderSpec, market: &Market) -> Result<(), CliError> {
    if matches!(market, Market::Perp(_)) {
        return Ok(());
    }
    if let Some(margin_mode) = spec.margin_mode {
        return Err(CliError::Configuration(format!(
            "--margin-mode {margin_mode} is only supported for perpetual orders"
        )));
    }
    if spec.reduce_only {
        return Err(CliError::Unsupported(
            "orders create --reduce-only is currently supported for perpetual markets only; remove --reduce-only for spot orders"
                .to_string(),
        ));
    }
    if spec.kind.is_trigger() {
        return Err(CliError::Unsupported(
            "orders create trigger orders currently support perpetual markets only".to_string(),
        ));
    }
    if spec.tpsl.is_some() {
        return Err(CliError::Unsupported(
            "orders create TP/SL grouping currently supports perpetual markets only".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_scale_args(args: &ScaleArgs) -> Result<(), CliError> {
    positive_price(Some(args.start_price), "start-price")?;
    positive_price(Some(args.end_price), "end-price")?;
    positive_quantity(Some(args.total_size), "total-size")?;
    if args.order_count == 0 {
        return Err(CliError::Configuration(
            "orders scale requires --orders greater than zero".to_string(),
        ));
    }
    if args.order_count > MAX_BATCH_ORDER_COUNT {
        return Err(CliError::Configuration(format!(
            "orders scale supports at most {MAX_BATCH_ORDER_COUNT} orders"
        )));
    }
    if args.order_count > 1 && args.start_price == args.end_price {
        return Err(CliError::Configuration(
            "orders scale requires different start and end prices when --orders is greater than one"
                .to_string(),
        ));
    }
    Ok(())
}

pub fn validate_batch_create_args(args: &BatchCreateArgs) -> Result<(), CliError> {
    if args.orders_file.as_os_str().is_empty() {
        return Err(CliError::Configuration(
            "orders batch-create requires --orders-file".to_string(),
        ));
    }
    Ok(())
}

pub fn read_validated_batch_create_orders(
    args: &BatchCreateArgs,
) -> Result<Vec<BatchCreateOrder>, CliError> {
    let orders = read_batch_create_orders(&args.orders_file)?;
    if orders.is_empty() {
        return Err(CliError::Configuration(
            "orders batch-create requires at least one order leg".to_string(),
        ));
    }
    Ok(orders)
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ClosingSize {
    /// The whole position the account holds when the order is placed.
    CurrentPosition,
    Fixed {
        side: OrderSide,
        size: Decimal,
    },
}

/// Position TP/SL to place, in the terms the planner needs. `orders tpsl`
/// builds it with [`TpslSpec::from_flags`], which is the only place its flags
/// are validated; `orders bracket` builds it with [`TpslSpec::protecting`] for
/// the position its entry opened.
#[derive(Debug, Clone)]
pub(crate) struct TpslSpec {
    pub(super) coin: String,
    pub(super) dex: Option<String>,
    pub(super) take_profit: Option<TriggerPriceSpec>,
    pub(super) stop_loss: Option<TriggerPriceSpec>,
    pub(super) close: ClosingSize,
    pub(super) cloid: Option<Cloid>,
    pub(super) margin_mode: Option<MarginModeArg>,
}

impl TpslSpec {
    pub(crate) fn from_flags(args: &TpslArgs) -> Result<Self, CliError> {
        let cloid = args.cloid.as_deref().map(parse_cloid).transpose()?;
        validate_tpsl_triggers(
            args.take_profit.as_ref(),
            args.stop_loss.as_ref(),
            args.size,
        )?;
        let close = match (args.side, args.size) {
            (Some(side), Some(size)) => ClosingSize::Fixed { side, size },
            (None, None) => ClosingSize::CurrentPosition,
            (Some(_), None) => {
                return Err(CliError::Configuration(
                    "orders tpsl --side requires --size for fixed-size TP/SL orders".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(CliError::Configuration(
                    "orders tpsl --size requires --side so the close direction is explicit"
                        .to_string(),
                ));
            }
        };
        Ok(Self {
            coin: args.coin.clone(),
            dex: args.dex.clone(),
            take_profit: args.take_profit.clone(),
            stop_loss: args.stop_loss.clone(),
            close,
            cloid,
            margin_mode: args.margin_mode,
        })
    }

    /// The TP/SL a bracket arms against the position its entry opened. `size`
    /// is the filled size, or `None` in a dry-run preview of an `--amount`
    /// entry, where the position size is not known yet.
    pub(super) fn protecting(args: &BracketArgs, size: Option<Decimal>) -> Self {
        Self {
            coin: args.coin.clone(),
            dex: args.dex.clone(),
            take_profit: Some(args.take_profit.clone()),
            stop_loss: Some(args.stop_loss.clone()),
            close: size.map_or(ClosingSize::CurrentPosition, |size| ClosingSize::Fixed {
                side: args.side.opposite(),
                size,
            }),
            cloid: None,
            margin_mode: None,
        }
    }

    /// Fail before anything is placed when the triggers or the size cannot make
    /// a valid TP/SL.
    pub(super) fn validate(&self) -> Result<(), CliError> {
        let size = match self.close {
            ClosingSize::Fixed { size, .. } => Some(size),
            ClosingSize::CurrentPosition => None,
        };
        validate_tpsl_triggers(self.take_profit.as_ref(), self.stop_loss.as_ref(), size)
    }
}

fn validate_tpsl_triggers(
    take_profit: Option<&TriggerPriceSpec>,
    stop_loss: Option<&TriggerPriceSpec>,
    size: Option<Decimal>,
) -> Result<(), CliError> {
    validate_trigger_price_spec(take_profit, "take-profit")?;
    validate_trigger_price_spec(stop_loss, "stop-loss")?;
    positive_quantity(size, "size")?;
    if take_profit.is_none() && stop_loss.is_none() {
        return Err(CliError::Configuration(
            "orders tpsl requires --take-profit or --stop-loss.\n  \
             hyperliquid --format json --dry-run orders tpsl --coin ETH --take-profit +10% --stop-loss -5%"
                .to_string(),
        ));
    }
    if let (Some(take_profit), Some(stop_loss)) = (take_profit, stop_loss)
        && take_profit == stop_loss
    {
        return Err(CliError::Configuration(
            "orders tpsl --take-profit and --stop-loss cannot be equal".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_modify_args(args: &ModifyArgs) -> Result<(), CliError> {
    order_identifier(args.order_id, args.cloid.as_deref(), "modify", parse_cloid)?;
    positive_price(args.price, "price")?;
    positive_price(args.trigger_price, "trigger-price")?;
    positive_quantity(args.size, "size")?;
    if args.price.is_none() && args.trigger_price.is_none() && args.size.is_none() {
        return Err(CliError::Configuration(
            "orders modify requires --price, --trigger-price, or --size".to_string(),
        ));
    }
    Ok(())
}

/// Validate `orders twap-create` and return its duration in whole minutes.
pub fn validate_twap_create_args(args: &TwapCreateArgs) -> Result<u64, CliError> {
    positive_quantity(Some(args.size), "size")?;
    if args.duration < 300 {
        return Err(CliError::Configuration(
            "TWAP duration must be at least 300 seconds and a whole-minute value in seconds (for example, --duration 300, --duration 600, or --duration 3600)"
                .to_string(),
        ));
    }
    if !args.duration.is_multiple_of(60) {
        return Err(CliError::Configuration(
            "TWAP duration must be a whole-minute value in seconds; use values like --duration 300, --duration 600, or --duration 3600"
                .to_string(),
        ));
    }
    if args.duration > 7 * 24 * 60 * 60 {
        return Err(CliError::Configuration(
            "TWAP duration cannot exceed 7 days (604800 seconds)".to_string(),
        ));
    }
    Ok(args.duration / 60)
}

fn validate_create_order_type_flags(args: &CreateArgs) -> Result<(), CliError> {
    match args.order_type {
        CreateOrderType::Limit if args.amount.is_some() => Err(CliError::Configuration(
            "orders create --type limit uses --price and --size; remove --amount or use --type market --amount for amount-based market orders"
                .to_string(),
        )),
        CreateOrderType::Limit if args.trigger_price.is_some() => Err(CliError::Configuration(
            "orders create --type limit uses --price and --size; remove --trigger-price or use --type stop-limit/take-limit for trigger-limit orders"
                .to_string(),
        )),
        CreateOrderType::Market if args.trigger_price.is_some() => Err(CliError::Configuration(
            "orders create --type market uses --amount; remove --trigger-price or use --type stop-loss/take-profit for market trigger orders, or --type stop-limit/take-limit with --price and --size"
                .to_string(),
        )),
        CreateOrderType::Market if args.price.is_some() => Err(CliError::Configuration(
            "orders create --type market accepts --amount or --size; remove --price or use --type limit with --price and --size"
                .to_string(),
        )),
        CreateOrderType::StopLoss | CreateOrderType::TakeProfit if args.amount.is_some() => {
            Err(CliError::Configuration(format!(
                "orders create --type {} uses --trigger-price (or legacy --price) and --size for market trigger orders; remove --amount",
                args.order_type
            )))
        }
        CreateOrderType::StopLoss | CreateOrderType::TakeProfit
            if args.price.is_some() && args.trigger_price.is_some() =>
        {
            Err(CliError::Configuration(format!(
                "orders create --type {} uses --trigger-price or legacy --price as the trigger price; remove one price flag or use --type stop-limit/take-limit for trigger-limit orders",
                args.order_type
            )))
        }
        CreateOrderType::StopLimit | CreateOrderType::TakeLimit if args.amount.is_some() => {
            Err(CliError::Configuration(format!(
                "orders create --type {} uses --trigger-price, --price, and --size for trigger-limit orders; remove --amount",
                args.order_type
            )))
        }
        _ => Ok(()),
    }
}

/// The values of required flags, or one error naming every missing flag.
fn require_flags<const N: usize>(
    values: [(Option<Decimal>, &'static str); N],
    context: &'static str,
) -> Result<[Decimal; N], CliError> {
    let missing = values
        .iter()
        .filter_map(|(value, flag)| value.is_none().then_some(*flag))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(CliError::Configuration(format!(
            "orders create requires {} for {context}",
            missing.join(" and ")
        )));
    }
    Ok(values.map(|(value, _)| value.unwrap_or_default()))
}

fn validate_trigger_price_spec(
    value: Option<&TriggerPriceSpec>,
    name: &'static str,
) -> Result<(), CliError> {
    match value {
        Some(TriggerPriceSpec::Absolute(price)) if *price <= Decimal::ZERO => {
            Err(CliError::Configuration(format!("{name} must be positive")))
        }
        _ => Ok(()),
    }
}

fn positive_price(value: Option<Decimal>, name: &str) -> Result<(), CliError> {
    match value {
        Some(value) if value <= Decimal::ZERO => {
            Err(CliError::Configuration(format!("{name} must be positive")))
        }
        _ => Ok(()),
    }
}

fn positive_quantity(value: Option<Decimal>, name: &str) -> Result<(), CliError> {
    match value {
        Some(value) if value <= Decimal::ZERO => Err(CliError::Configuration(format!(
            "{name} must be greater than zero"
        ))),
        _ => Ok(()),
    }
}

fn validate_market_slippage(max_slippage_bps: u16) -> Result<(), CliError> {
    if !(MIN_MARKET_ORDER_SLIPPAGE_BPS..=MAX_MARKET_ORDER_SLIPPAGE_BPS).contains(&max_slippage_bps)
    {
        return Err(CliError::Configuration(format!(
            "--max-slippage-bps must be between {MIN_MARKET_ORDER_SLIPPAGE_BPS} and {MAX_MARKET_ORDER_SLIPPAGE_BPS}"
        )));
    }
    Ok(())
}

const MAX_LISTED_LEG_PROBLEMS: usize = 5;

fn read_batch_create_orders(path: &Path) -> Result<Vec<BatchCreateOrder>, CliError> {
    let value = read_json_file(path, FilePolicy::json_artifact("orders file"))?;
    let orders_value = value.get("orders").cloned().unwrap_or(value);
    let serde_json::Value::Array(items) = orders_value else {
        return Err(CliError::Configuration(format!(
            "Can't use orders file {}. Expected a JSON array, or an object with an `orders` array.",
            path.display()
        )));
    };
    if items.len() > MAX_BATCH_ORDER_COUNT {
        return Err(CliError::Configuration(format!(
            "orders batch-create supports at most {MAX_BATCH_ORDER_COUNT} order legs, {} given in {}",
            items.len(),
            path.display()
        )));
    }

    let mut orders = Vec::with_capacity(items.len());
    let mut problems = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match parse_batch_leg(index, item) {
            Ok(order) => orders.push(order),
            Err(leg_problems) => problems.extend(leg_problems),
        }
    }
    if problems.is_empty() {
        return Ok(orders);
    }

    let hidden = problems.len().saturating_sub(MAX_LISTED_LEG_PROBLEMS);
    let mut lines: Vec<String> = problems
        .iter()
        .take(MAX_LISTED_LEG_PROBLEMS)
        .map(|problem| format!("  {problem}"))
        .collect();
    if hidden > 0 {
        lines.push(format!("  and {hidden} more"));
    }
    Err(CliError::Configuration(format!(
        "Can't use orders file {}. Fix these fields:\n{}",
        path.display(),
        lines.join("\n")
    )))
}

/// One batch leg, or every invalid field of it named `orders[<index>].<field>`.
/// The final struct literal names every [`BatchCreateOrder`] field, so a new
/// field cannot be added without also being read and checked here.
fn parse_batch_leg(
    index: usize,
    item: &serde_json::Value,
) -> Result<BatchCreateOrder, Vec<String>> {
    let Some(object) = item.as_object() else {
        return Err(vec![format!("orders[{index}] must be an object")]);
    };
    let mut leg = LegReader {
        index,
        object,
        problems: Vec::new(),
    };
    let coin = leg.required::<String>("coin");
    let side = leg.required::<OrderSide>("side");
    let price = leg.required::<Decimal>("price");
    let size = leg.required::<Decimal>("size");
    let dex = leg.optional::<Option<String>>("dex").flatten();
    let tif = leg.optional::<Option<TifArg>>("tif").flatten();
    let reduce_only = leg.optional::<bool>("reduce_only").unwrap_or_default();
    let cloid = leg.optional::<Option<String>>("cloid").flatten();
    match (coin, side, price, size) {
        (Some(coin), Some(side), Some(price), Some(size)) if leg.problems.is_empty() => {
            Ok(BatchCreateOrder {
                coin,
                dex,
                side,
                price,
                size,
                tif,
                reduce_only,
                cloid,
            })
        }
        _ => Err(leg.problems),
    }
}

struct LegReader<'a> {
    index: usize,
    object: &'a serde_json::Map<String, serde_json::Value>,
    problems: Vec<String>,
}

impl LegReader<'_> {
    fn required<T: serde::de::DeserializeOwned>(&mut self, name: &str) -> Option<T> {
        match self.object.get(name) {
            Some(value) => self.parse(name, value),
            None => {
                self.problems
                    .push(format!("orders[{}].{name} is required", self.index));
                None
            }
        }
    }

    fn optional<T: serde::de::DeserializeOwned>(&mut self, name: &str) -> Option<T> {
        let value = self.object.get(name)?;
        self.parse(name, value)
    }

    fn parse<T: serde::de::DeserializeOwned>(
        &mut self,
        name: &str,
        value: &serde_json::Value,
    ) -> Option<T> {
        match serde_json::from_value(value.clone()) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                self.problems
                    .push(format!("orders[{}].{name}: {err}", self.index));
                None
            }
        }
    }
}

pub(crate) fn parse_relative_duration(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("duration cannot be empty".to_string());
    }

    let (number_part, multiplier, unit_name) = match trimmed.chars().last().unwrap() {
        's' | 'S' => (&trimmed[..trimmed.len() - 1], 1_u64, "seconds"),
        'm' | 'M' => (&trimmed[..trimmed.len() - 1], 60_u64, "minutes"),
        'h' | 'H' => (&trimmed[..trimmed.len() - 1], 60 * 60, "hours"),
        'd' | 'D' => (&trimmed[..trimmed.len() - 1], 24 * 60 * 60, "days"),
        ch if ch.is_ascii_digit() => (trimmed, 1_u64, "seconds"),
        _ => {
            return Err(
                "duration must be a positive integer with optional s/m/h/d suffix".to_string(),
            );
        }
    };

    let value = number_part
        .parse::<u64>()
        .map_err(|_| format!("duration {unit_name} must be a positive integer"))?;
    if value == 0 {
        return Err("duration must be greater than zero".to_string());
    }
    value
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .ok_or_else(|| "duration is too large".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_leg_reads_a_minimal_leg_with_defaults() {
        let leg = parse_batch_leg(
            0,
            &serde_json::json!({
                "coin": "BTC", "side": "buy", "price": "50000", "size": "0.1",
                "dex": null, "tif": null, "cloid": null
            }),
        )
        .unwrap();
        assert_eq!(leg.coin, "BTC");
        assert_eq!(leg.side, OrderSide::Buy);
        assert_eq!(leg.dex, None);
        assert_eq!(leg.tif, None);
        assert!(!leg.reduce_only);
    }

    #[test]
    fn batch_leg_names_every_invalid_field_in_one_pass() {
        let problems = parse_batch_leg(
            3,
            &serde_json::json!({
                "side": "up", "price": "x", "reduce_only": "yes", "tif": "fok"
            }),
        )
        .unwrap_err();
        let fields: Vec<_> = problems
            .iter()
            .map(|problem| problem.split([':', ' ']).next().unwrap().to_string())
            .collect();
        assert_eq!(
            fields,
            [
                "orders[3].coin",
                "orders[3].side",
                "orders[3].price",
                "orders[3].size",
                "orders[3].tif",
                "orders[3].reduce_only",
            ]
        );
        assert!(problems[0].ends_with("is required"));
        assert!(problems[5].starts_with("orders[3].reduce_only: "));
    }

    #[test]
    fn batch_leg_rejects_non_objects() {
        let problems = parse_batch_leg(1, &serde_json::json!(["BTC"])).unwrap_err();
        assert_eq!(problems, ["orders[1] must be an object"]);
    }

    #[test]
    fn twap_duration_validates_and_returns_whole_minutes() {
        let args = |duration| TwapCreateArgs {
            coin: "BTC".to_string(),
            dex: None,
            side: OrderSide::Buy,
            size: Decimal::ONE,
            duration,
            margin_mode: None,
            on_behalf_of: None,
            randomize: false,
            yes: false,
        };
        assert_eq!(validate_twap_create_args(&args(300)).unwrap(), 5);
        assert_eq!(validate_twap_create_args(&args(3600)).unwrap(), 60);
        for (duration, message) in [
            (299, "at least 300 seconds"),
            (330, "whole-minute value in seconds"),
            (7 * 24 * 60 * 60 + 60, "cannot exceed 7 days"),
        ] {
            let err = validate_twap_create_args(&args(duration)).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{duration}");
            assert!(err.to_string().contains(message), "{duration}: {err}");
        }
    }
}
