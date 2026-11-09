//! Planning: turn validated arguments into the orders and actions to send.
//!
//! A plan is built once and read by every later stage: the dry-run preview
//! renders it, the confirmation prompt summarizes it, and submission sends it.

use std::collections::HashMap;

use chrono::Utc;
use hypersdk::hypercore::types::{
    Action, AssetPosition, BasicOrder, BatchCancel, BatchCancelCloid, BatchModify, BatchOrder,
    Cancel, CancelByCloid, Modify, OrderGrouping, OrderRequest, OrderTypePlacement, Side,
    TimeInForce, TpSl,
};
use hypersdk::hypercore::{Cloid, HttpClient};
use hypersdk::{Address, Decimal};

use super::rendering::ModifyConfirmation;
use super::validation::{
    AttachedTpsl, BuilderRequest, MarketPrecision, MarketSizing, OrderKind, OrderSpec,
    validate_order_for_market, validate_twap_create_args,
};
use super::{
    BatchCreateArgs, BatchCreateOrder, CancelAllArgs, CancelArgs, CreateArgs, CreateOrderType,
    MarginModeArg, ModifyArgs, OrderBuilderFee, OrderIdentifier, OrderSide, PreparedOrder,
    PreparedOrderBatch, PreparedOrderLeg, PreparedTpslBatch, PreparedTpslLeg, ScaleArgs,
    ScheduleCancelArgs, TifArg, TpslArgs, TpslGroupingArg, TpslLegKind, TriggerPriceSpec,
    TwapCancelArgs, TwapCreateArgs, TwapExchangeAction, TwapOrderAction, cloid_from_wire,
    filter_orders_by_coin, lookup_order, order_type_placement, parse_cloid, parse_lookup_cloid,
    resolve_cancel_all_coin_filter, wire_cloid,
};
use crate::assets::asset_id::{parse_spot_coin_key, spot_asset_index, spot_coin_key};
use crate::assets::{
    AssetQuery, AssetResolver, Market, OutcomeAsset, PerpAsset, parse_asset_query,
    qualify_dex_asset,
};
use crate::commands::builder;
use crate::errors::{CliError, from_sdk};
use crate::settings::Settings;

pub(super) struct OrderDryRunPlan {
    command: &'static str,
    would_execute: &'static str,
    args: serde_json::Value,
}

impl OrderDryRunPlan {
    pub(super) fn new(
        command: &'static str,
        would_execute: &'static str,
        args: serde_json::Value,
    ) -> Self {
        Self {
            command,
            would_execute,
            args,
        }
    }

    /// Record the acting account the command was given.
    fn acting_for(mut self, on_behalf_of: &Option<String>) -> Self {
        if let Some(object) = self.args.as_object_mut() {
            object.insert("on_behalf_of".to_string(), serde_json::json!(on_behalf_of));
        }
        self
    }

    #[must_use]
    pub(super) fn command(&self) -> &'static str {
        self.command
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

pub(super) struct CancelOrderPlan {
    pub(super) action: Action,
    pub(super) coin: String,
    pub(super) order_id: Option<u64>,
    pub(super) cloid: Option<String>,
}

pub(super) struct CancelAllOrdersPlan {
    pub(super) action: Option<Action>,
    pub(super) cancelled_orders: usize,
    pub(super) summary_coin: String,
}

pub(super) struct ModifyOrderPlan {
    pub(super) action: Action,
    pub(super) confirmation: ModifyConfirmation,
}

/// A planned `orders create`: one order, or an entry order with attached TP/SL.
pub(super) enum CreateOrderSubmission {
    Single(PreparedOrder),
    NormalTpsl(NormalTpslOrder),
}

pub(super) struct NormalTpslOrder {
    pub(super) parent: PreparedOrder,
    pub(super) batch: PreparedTpslBatch,
}

impl CreateOrderSubmission {
    /// The entry order, which sizes the HIP-3 margin helper and names the market.
    pub(super) fn parent(&self) -> &PreparedOrder {
        match self {
            Self::Single(prepared) => prepared,
            Self::NormalTpsl(order) => &order.parent,
        }
    }
}

pub(super) struct TwapCreatePlan {
    pub(super) action: TwapExchangeAction,
    pub(super) perp: PerpAsset,
    pub(super) coin: String,
    pub(super) side: String,
    pub(super) size: Decimal,
    pub(super) duration_seconds: u64,
    pub(super) duration_minutes: u64,
    pub(super) randomize: bool,
    /// The margin mode the account's position must already use.
    pub(super) margin_mode: Option<MarginModeArg>,
}

pub(super) struct TwapCancelPlan {
    pub(super) action: TwapExchangeAction,
    pub(super) coin: String,
    pub(super) twap_id: u64,
}

/// `orders schedule-cancel`: arm the dead man's switch a duration from now, or clear it.
pub(super) enum ScheduleCancelPlan {
    Set {
        delay: chrono::Duration,
        in_seconds: u64,
    },
    Clear,
}

/// The moment a [`ScheduleCancelPlan::Set`] fires when sent at a given time.
pub(super) struct ScheduledCancel {
    pub(super) at: chrono::DateTime<Utc>,
    pub(super) in_seconds: u64,
}

impl ScheduledCancel {
    pub(super) fn at_ms(&self) -> u64 {
        self.at.timestamp_millis() as u64
    }
}

impl ScheduleCancelPlan {
    /// When the cancel fires if the plan is sent at `now`; `None` when clearing.
    pub(super) fn scheduled(&self, now: chrono::DateTime<Utc>) -> Option<ScheduledCancel> {
        match self {
            Self::Set { delay, in_seconds } => Some(ScheduledCancel {
                at: now + *delay,
                in_seconds: *in_seconds,
            }),
            Self::Clear => None,
        }
    }
}

fn insert_margin_mode(preview: &mut serde_json::Value, margin_mode: Option<MarginModeArg>) {
    if let Some(object) = preview.as_object_mut() {
        object.insert(
            "margin_mode".to_string(),
            serde_json::json!(margin_mode.unwrap_or(MarginModeArg::Cross).to_string()),
        );
    }
}

/// The perp for `--coin` / `--dex`, fetching a HIP-3 market the resolver does
/// not list yet. Command code obtains a perp only here, and planners take the
/// resulting [`PerpAsset`], so none can run against an unresolved market.
pub(super) async fn resolve_perp(
    client: &HttpClient,
    resolver: &AssetResolver,
    dex: Option<&str>,
    coin: &str,
) -> Result<PerpAsset, CliError> {
    Ok(resolver
        .resolve_perp_or_fetch(client, &qualify_dex_asset(dex, coin))
        .await?)
}

/// The market `spec` trades, fetching a HIP-3 market the resolver does not list yet.
async fn resolve_tradable_asset(
    client: &HttpClient,
    resolver: &AssetResolver,
    spec: &OrderSpec,
) -> Result<Market, CliError> {
    if let AssetQuery::Outcome(notation) = parse_asset_query(&spec.coin) {
        if spec.dex.is_some() {
            return Err(CliError::Unsupported(format!(
                "outcome notation '{notation}' cannot be used with --dex; omit --dex for outcome orders"
            )));
        }
        return Ok(Market::Outcome(OutcomeAsset::new(notation)));
    }

    let query = qualify_dex_asset(spec.dex.as_deref(), &spec.coin);
    let perp_err = match resolver.resolve_perp_or_fetch(client, &query).await {
        Ok(perp) => return Ok(Market::Perp(perp)),
        Err(err) => err,
    };
    if !matches!(parse_asset_query(&query), AssetQuery::Hip3 { .. })
        && let Ok(spot) = resolver.resolve_spot(&query)
    {
        return Ok(Market::Spot(spot));
    }
    Err(perp_err.into())
}

/// Per-market facts every order on it reads.
struct Venue {
    coin: String,
    index: usize,
    precision: MarketPrecision,
    quote: String,
}

fn venue(market: &Market) -> Result<Venue, CliError> {
    let (coin, index, quote) = match market {
        Market::Perp(perp) => (perp.display_name(), perp.index, perp.collateral.clone()),
        Market::Spot(spot) => (spot.symbol.clone(), spot.index, spot.quote.clone()),
        Market::Outcome(outcome) => (
            outcome.notation().to_string(),
            outcome.asset_index()?,
            "USDH".to_string(),
        ),
    };
    Ok(Venue {
        coin,
        index,
        precision: MarketPrecision::of(market),
        quote,
    })
}

async fn market_mid_price(
    client: &HttpClient,
    market: &Market,
    spec: &OrderSpec,
) -> Result<Decimal, CliError> {
    let (key, dex, label) = match market {
        Market::Perp(perp) => (
            perp.display_name(),
            perp.dex.clone(),
            qualify_dex_asset(spec.dex.as_deref(), &spec.coin),
        ),
        Market::Spot(spot) => (spot_coin_key(spot.index), None, spec.coin.clone()),
        Market::Outcome(_) => {
            return Err(CliError::Unsupported(
                "outcome orders do not support --type market because outcome size decimals are not verified; use an explicit --price and --size"
                    .to_string(),
            ));
        }
    };
    let mids = client.all_mids(dex).await.map_err(from_sdk)?;
    mids.get(&key).copied().ok_or_else(|| {
        CliError::Unavailable(format!(
            "mid price for {label} was not available. Retry after market data refresh."
        ))
    })
}

/// The builder fee an order carries. Builder codes apply to spot sells and
/// both sides of perpetual orders: an explicit request on a spot buy is
/// rejected, and the configured default is skipped for it.
fn order_builder_fee(
    settings: &Settings,
    request: &BuilderRequest,
    market: &Market,
    side: OrderSide,
) -> Result<Option<OrderBuilderFee>, CliError> {
    let spot_buy = matches!(market, Market::Spot(_)) && side == OrderSide::Buy;
    let (address, fee) = match request {
        BuilderRequest::None => return Ok(None),
        BuilderRequest::Explicit { address, fee } => {
            if spot_buy {
                return Err(CliError::Unsupported(
                    "spot buy orders do not support builder fees; Hyperliquid builder codes apply to spot sells and both sides of perpetual orders"
                        .to_string(),
                ));
            }
            (*address, *fee)
        }
        BuilderRequest::Default => {
            if spot_buy {
                return Ok(None);
            }
            match builder::resolve_default_builder_fee(settings)? {
                Some(default) => default,
                None => return Ok(None),
            }
        }
    };
    if matches!(market, Market::Perp(_)) && fee > 100 {
        return Err(CliError::Unsupported(
            "perp order builder fee rate cannot exceed 0.1%".to_string(),
        ));
    }
    Ok(Some(OrderBuilderFee {
        // Hyperliquid's Python SDK lowercases builder addresses before hashing/signing
        // L1 order actions. Checksum casing can change the msgpack action hash and
        // recover a random nonexistent signer on exchange validation.
        b: address.to_string().to_ascii_lowercase(),
        f: fee,
    }))
}

pub(super) async fn prepare_order(
    client: &HttpClient,
    resolver: &AssetResolver,
    settings: &Settings,
    spec: &OrderSpec,
) -> Result<PreparedOrder, CliError> {
    let market = resolve_tradable_asset(client, resolver, spec).await?;
    validate_order_for_market(spec, &market)?;
    let builder = order_builder_fee(settings, &spec.builder, &market, spec.side)?;
    prepare_order_in(client, &market, spec, builder).await
}

/// Plan one order, already validated for `market`, with its builder fee.
async fn prepare_order_in(
    client: &HttpClient,
    market: &Market,
    spec: &OrderSpec,
    builder: Option<OrderBuilderFee>,
) -> Result<PreparedOrder, CliError> {
    let Venue {
        coin,
        index,
        precision,
        quote,
    } = venue(market)?;

    let (price, trigger_price, size, amount, warning, placement, tif) = match spec.kind {
        OrderKind::Limit { price, size, tif } => (
            precision.price(price, &coin)?,
            None,
            precision.size(size, &coin)?,
            None,
            None,
            OrderTypePlacement::Limit { tif: tif.into() },
            Some(tif),
        ),
        OrderKind::Trigger {
            tpsl,
            trigger_price,
            limit_price,
            size,
        } => {
            let trigger_px = precision.price(trigger_price, &coin)?;
            let price = match limit_price {
                Some(limit_price) => precision.price(limit_price, &coin)?,
                None => trigger_px,
            };
            (
                price,
                Some(trigger_px),
                precision.size(size, &coin)?,
                None,
                None,
                OrderTypePlacement::Trigger {
                    is_market: limit_price.is_none(),
                    trigger_px,
                    tpsl,
                },
                None,
            )
        }
        OrderKind::Market {
            sizing,
            max_slippage_bps,
        } => {
            let mid = market_mid_price(client, market, spec).await?;
            let price = precision.marketable_price(mid, spec.side, max_slippage_bps)?;
            let warning = Some(format!(
                "Slippage warning: market order uses a {max_slippage_bps} bps protective price around mid {mid}"
            ));
            let (size, amount) = match sizing {
                MarketSizing::Size(size) => (precision.size(size, &coin)?, None),
                MarketSizing::Amount(amount) => {
                    let size = precision.size(amount / mid, &coin).map_err(|_| {
                        CliError::Configuration(format!(
                            "amount is too small for {coin}; derived size rounds to zero"
                        ))
                    })?;
                    (size, Some(amount))
                }
            };
            (
                price,
                None,
                size,
                amount,
                warning,
                OrderTypePlacement::Limit {
                    tif: TimeInForce::FrontendMarket,
                },
                None,
            )
        }
    };

    let implicit_reduce_only = matches!(
        spec.kind,
        OrderKind::Trigger {
            limit_price: None,
            ..
        }
    );
    Ok(PreparedOrder {
        request: OrderRequest {
            asset: index,
            is_buy: spec.side.is_buy(),
            limit_px: price,
            sz: size,
            reduce_only: spec.reduce_only || implicit_reduce_only,
            order_type: placement,
            cloid: wire_cloid(spec.cloid),
        },
        market: market.clone(),
        precision,
        coin,
        side: spec.side,
        order_type: spec.kind.order_type(),
        tif,
        price,
        trigger_price,
        size,
        amount,
        amount_unit: quote,
        warning,
        builder,
        margin_mode: spec.margin_mode,
    })
}

/// Plan an order and its attached TP/SL children, if any.
pub(super) async fn prepare_create_order(
    client: &HttpClient,
    resolver: &AssetResolver,
    settings: &Settings,
    spec: &OrderSpec,
) -> Result<CreateOrderSubmission, CliError> {
    let parent = prepare_order(client, resolver, settings, spec).await?;
    match &spec.tpsl {
        Some(tpsl) => {
            let batch = prepare_normal_tpsl_batch(&parent, tpsl)?;
            Ok(CreateOrderSubmission::NormalTpsl(NormalTpslOrder {
                parent,
                batch,
            }))
        }
        None => Ok(CreateOrderSubmission::Single(parent)),
    }
}

/// [`prepare_create_order`] for orders the CLI generates from `CreateArgs`.
pub(super) async fn prepare_create_order_plan(
    client: &HttpClient,
    resolver: &AssetResolver,
    settings: &Settings,
    args: &CreateArgs,
) -> Result<CreateOrderSubmission, CliError> {
    prepare_create_order(client, resolver, settings, &OrderSpec::from_create(args)?).await
}

pub(super) async fn prepare_scale_batch(
    client: &HttpClient,
    resolver: &AssetResolver,
    args: &ScaleArgs,
) -> Result<PreparedOrderBatch, CliError> {
    let prices = scale_prices(args.start_price, args.end_price, args.order_count)?;
    let sizes = scale_sizes(args.total_size, args.order_count)?;
    let specs = prices
        .into_iter()
        .zip(sizes)
        .map(|(price, size)| {
            OrderSpec::limit_leg(
                &args.coin,
                args.dex.as_deref(),
                args.side,
                price,
                size,
                args.tif,
            )
            .map(|spec| {
                spec.reduce_only(args.reduce_only)
                    .with_margin_mode(args.margin_mode)
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = prepare_order_batch(client, resolver, specs).await?;

    let placed_size: Decimal = prepared.legs.iter().map(|leg| leg.order.size).sum();
    if placed_size != args.total_size {
        return Err(CliError::Configuration(
            "orders scale total size cannot be evenly distributed across requested orders at the market's lot size"
                .to_string(),
        ));
    }
    if prepared
        .legs
        .windows(2)
        .any(|pair| pair[0].order.price == pair[1].order.price)
    {
        return Err(CliError::Configuration(format!(
            "orders scale prices collapse onto the same tick for {}; use fewer --orders or a wider price range",
            prepared.legs[0].order.coin
        )));
    }
    Ok(prepared)
}

pub(super) async fn prepare_batch_create_orders(
    client: &HttpClient,
    resolver: &AssetResolver,
    orders: Vec<BatchCreateOrder>,
) -> Result<PreparedOrderBatch, CliError> {
    let specs = orders
        .into_iter()
        .map(|order| {
            let cloid = order.cloid.as_deref().map(parse_cloid).transpose()?;
            Ok(OrderSpec::limit_leg(
                &order.coin,
                order.dex.as_deref(),
                order.side,
                order.price,
                order.size,
                order.tif.unwrap_or(TifArg::Gtc),
            )?
            .reduce_only(order.reduce_only)
            .with_cloid(cloid))
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    prepare_order_batch(client, resolver, specs).await
}

/// Plan generated legs, resolving each distinct market once.
async fn prepare_order_batch(
    client: &HttpClient,
    resolver: &AssetResolver,
    specs: Vec<OrderSpec>,
) -> Result<PreparedOrderBatch, CliError> {
    let mut markets: HashMap<(String, Option<String>), Market> = HashMap::new();
    let mut legs = Vec::with_capacity(specs.len());
    for (leg_index, spec) in specs.iter().enumerate() {
        let key = (spec.coin.clone(), spec.dex.clone());
        if !markets.contains_key(&key) {
            let market = resolve_tradable_asset(client, resolver, spec).await?;
            markets.insert(key.clone(), market);
        }
        validate_order_for_market(spec, &markets[&key])?;
        let order = prepare_order_in(client, &markets[&key], spec, None).await?;
        legs.push(PreparedOrderLeg { leg_index, order });
    }
    let batch = BatchOrder {
        orders: legs.iter().map(|leg| leg.order.request.clone()).collect(),
        grouping: OrderGrouping::Na,
        builder: None,
    };
    Ok(PreparedOrderBatch { batch, legs })
}

fn scale_prices(
    start_price: Decimal,
    end_price: Decimal,
    order_count: usize,
) -> Result<Vec<Decimal>, CliError> {
    if order_count == 0 {
        return Err(CliError::Configuration(
            "orders scale requires --orders greater than zero".to_string(),
        ));
    }
    if order_count == 1 {
        return Ok(vec![start_price]);
    }
    let steps = Decimal::from(order_count - 1);
    let step = (end_price - start_price) / steps;
    Ok((0..order_count)
        .map(|index| start_price + step * Decimal::from(index))
        .collect())
}

pub(super) fn scale_sizes(
    total_size: Decimal,
    order_count: usize,
) -> Result<Vec<Decimal>, CliError> {
    if order_count == 0 {
        return Err(CliError::Configuration(
            "orders scale requires --orders greater than zero".to_string(),
        ));
    }
    let count = Decimal::from(order_count);
    let per_leg = total_size / count;
    if per_leg <= Decimal::ZERO {
        return Err(CliError::Configuration(
            "orders scale total size is too small to distribute across requested orders"
                .to_string(),
        ));
    }
    if per_leg * count != total_size {
        return Err(CliError::Configuration(
            "orders scale total size cannot be evenly distributed across requested orders"
                .to_string(),
        ));
    }
    Ok(vec![per_leg; order_count])
}

pub(super) fn prepare_normal_tpsl_batch(
    parent: &PreparedOrder,
    tpsl: &AttachedTpsl,
) -> Result<PreparedTpslBatch, CliError> {
    let close_side = parent.side.opposite();
    let truncate = |price: Decimal| parent.precision.price(price, &parent.coin);
    let take_profit = tpsl.take_profit.map(truncate).transpose()?;
    let stop_loss = tpsl.stop_loss.map(truncate).transpose()?;
    validate_tpsl_price_ordering(close_side, take_profit, stop_loss, "orders create")?;

    let mut legs = vec![PreparedTpslLeg {
        request: parent.request.clone(),
        leg: TpslLegKind::Parent,
        coin: parent.coin.clone(),
        side: parent.side,
        order_type: parent.order_type,
        price: parent.price,
        size: parent.size,
        tif: parent.tif,
        reduce_only: parent.request.reduce_only,
        warning: parent.warning.clone(),
    }];
    let children = TpslChildren {
        asset: parent.request.asset,
        coin: &parent.coin,
        size: parent.request.sz,
        side: close_side,
    };
    children.push_legs(&mut legs, take_profit, stop_loss, None);
    Ok(batch_from_legs(tpsl.grouping.into(), legs))
}

/// What a TP/SL child order is for: the market, the size, and the side that closes it.
struct TpslChildren<'a> {
    asset: usize,
    coin: &'a str,
    size: Decimal,
    side: OrderSide,
}

impl TpslChildren<'_> {
    /// Append the take-profit then the stop-loss leg, for whichever is set.
    /// `cloid` goes to the first leg appended.
    fn push_legs(
        &self,
        legs: &mut Vec<PreparedTpslLeg>,
        take_profit: Option<Decimal>,
        stop_loss: Option<Decimal>,
        cloid: Option<Cloid>,
    ) {
        let mut cloids = cloid.into_iter();
        for (tpsl, price) in [(TpSl::Tp, take_profit), (TpSl::Sl, stop_loss)] {
            if let Some(price) = price {
                legs.push(self.leg(tpsl, price, cloids.next()));
            }
        }
    }

    fn leg(&self, tpsl: TpSl, price: Decimal, cloid: Option<Cloid>) -> PreparedTpslLeg {
        PreparedTpslLeg {
            request: OrderRequest {
                asset: self.asset,
                is_buy: self.side.is_buy(),
                limit_px: price,
                sz: self.size,
                reduce_only: true,
                order_type: OrderTypePlacement::Trigger {
                    is_market: true,
                    trigger_px: price,
                    tpsl,
                },
                cloid: wire_cloid(cloid),
            },
            leg: TpslLegKind::of(tpsl),
            coin: self.coin.to_string(),
            side: self.side,
            order_type: CreateOrderType::market_trigger(tpsl),
            price,
            size: self.size,
            tif: None,
            reduce_only: true,
            warning: None,
        }
    }
}

fn batch_from_legs(grouping: TpslGroupingArg, legs: Vec<PreparedTpslLeg>) -> PreparedTpslBatch {
    let batch = BatchOrder {
        orders: legs.iter().map(|leg| leg.request.clone()).collect(),
        grouping: grouping.into(),
        builder: None,
    };
    PreparedTpslBatch {
        batch,
        grouping,
        legs,
    }
}

/// Reduce-only TP/SL orders that close `size` of a position on `perp`.
pub(super) fn position_tpsl_batch(
    perp: &PerpAsset,
    close_side: OrderSide,
    size: Decimal,
    take_profit: Option<Decimal>,
    stop_loss: Option<Decimal>,
    cloid: Option<Cloid>,
) -> PreparedTpslBatch {
    let coin = perp.display_name();
    let children = TpslChildren {
        asset: perp.index,
        coin: &coin,
        size,
        side: close_side,
    };
    let mut legs = Vec::new();
    children.push_legs(&mut legs, take_profit, stop_loss, cloid);
    batch_from_legs(TpslGroupingArg::PositionTpsl, legs)
}

/// TP/SL trigger prices truncated to the market's tick grid.
fn truncate_triggers(
    perp: &PerpAsset,
    take_profit: Option<Decimal>,
    stop_loss: Option<Decimal>,
) -> Result<(Option<Decimal>, Option<Decimal>), CliError> {
    let precision = MarketPrecision::perp(perp.sz_decimals);
    let coin = perp.display_name();
    let truncate = |price: Decimal| precision.price(price, &coin);
    Ok((
        take_profit.map(truncate).transpose()?,
        stop_loss.map(truncate).transpose()?,
    ))
}

/// The TP/SL trigger prices of `args` as they would be sent to close a
/// position with `entry` as its entry price. The shared rule behind both the
/// live order and its dry-run preview.
fn position_tpsl_triggers(
    perp: &PerpAsset,
    args: &TpslArgs,
    close_side: OrderSide,
    entry: Option<Decimal>,
) -> Result<(Option<Decimal>, Option<Decimal>), CliError> {
    let (take_profit, stop_loss) = resolve_trigger_prices(
        args.take_profit.as_ref(),
        args.stop_loss.as_ref(),
        entry,
        close_side.opposite(),
    )?;
    let (take_profit, stop_loss) = truncate_triggers(perp, take_profit, stop_loss)?;
    validate_tpsl_price_ordering(close_side, take_profit, stop_loss, "orders tpsl")?;
    Ok((take_profit, stop_loss))
}

/// The user's position on `perp`, if they hold one.
async fn fetch_position(
    client: &HttpClient,
    user: Address,
    perp: &PerpAsset,
) -> Result<Option<AssetPosition>, CliError> {
    Ok(client
        .clearinghouse_state(user, perp.dex.clone())
        .await
        .map_err(from_sdk)?
        .asset_positions
        .into_iter()
        .find(|position| position.position.coin.eq_ignore_ascii_case(&perp.name)))
}

fn position_close_side_and_size(
    position: &AssetPosition,
) -> Result<(OrderSide, Decimal), CliError> {
    let szi = position.position.szi;
    if szi > Decimal::ZERO {
        Ok((OrderSide::Sell, szi))
    } else if szi < Decimal::ZERO {
        Ok((OrderSide::Buy, szi.abs()))
    } else {
        Err(CliError::Configuration(format!(
            "orders tpsl requires a non-zero {} position",
            position.position.coin
        )))
    }
}

pub(super) async fn prepare_position_tpsl_batch(
    client: &HttpClient,
    perp: &PerpAsset,
    user: Address,
    args: &TpslArgs,
) -> Result<PreparedTpslBatch, CliError> {
    let (close_side, size, entry) = match (args.side, args.size) {
        (Some(side), Some(size)) => {
            let entry =
                if trigger_specs_need_entry(args.take_profit.as_ref(), args.stop_loss.as_ref()) {
                    let position = fetch_position(client, user, perp).await?.ok_or_else(|| {
                        CliError::Unsupported(format!(
                            "percent and entry TP/SL require an open {} position",
                            perp.name
                        ))
                    })?;
                    position.position.entry_px
                } else {
                    None
                };
            (side, size, entry)
        }
        (None, None) => {
            let position = fetch_position(client, user, perp).await?.ok_or_else(|| {
                CliError::Configuration(format!(
                    "orders tpsl requires an open {} position or both --side and --size for a fixed-size TP/SL",
                    perp.name
                ))
            })?;
            let (side, size) = position_close_side_and_size(&position)?;
            (side, size, position.position.entry_px)
        }
        _ => {
            return Err(CliError::Configuration(
                "orders tpsl needs --side and --size together".to_string(),
            ));
        }
    };
    let size = MarketPrecision::perp(perp.sz_decimals).size(size, &perp.display_name())?;
    let (take_profit, stop_loss) = position_tpsl_triggers(perp, args, close_side, entry)?;
    let cloid = args.cloid.as_deref().map(parse_cloid).transpose()?;
    Ok(position_tpsl_batch(
        perp,
        close_side,
        size,
        take_profit,
        stop_loss,
        cloid,
    ))
}

fn trigger_specs_need_entry(
    take_profit: Option<&TriggerPriceSpec>,
    stop_loss: Option<&TriggerPriceSpec>,
) -> bool {
    [take_profit, stop_loss]
        .into_iter()
        .flatten()
        .any(|spec| matches!(spec, TriggerPriceSpec::Percent(_) | TriggerPriceSpec::Entry))
}

pub(super) fn resolve_trigger_prices(
    take_profit: Option<&TriggerPriceSpec>,
    stop_loss: Option<&TriggerPriceSpec>,
    entry: Option<Decimal>,
    position_side: OrderSide,
) -> Result<(Option<Decimal>, Option<Decimal>), CliError> {
    if trigger_specs_need_entry(take_profit, stop_loss) && entry.is_none() {
        return Err(CliError::Unsupported(
            "percent and entry TP/SL require an open position with an entry price".to_string(),
        ));
    }
    let entry = entry.unwrap_or(Decimal::ONE);
    Ok((
        take_profit
            .map(|spec| spec.resolve(entry, position_side))
            .transpose()?,
        stop_loss
            .map(|spec| spec.resolve(entry, position_side))
            .transpose()?,
    ))
}

fn absolute_trigger_prices(
    take_profit: Option<&TriggerPriceSpec>,
    stop_loss: Option<&TriggerPriceSpec>,
) -> (Option<Decimal>, Option<Decimal>) {
    (
        match take_profit {
            Some(TriggerPriceSpec::Absolute(price)) => Some(*price),
            _ => None,
        },
        match stop_loss {
            Some(TriggerPriceSpec::Absolute(price)) => Some(*price),
            _ => None,
        },
    )
}

pub(super) fn validate_tpsl_price_ordering(
    close_side: OrderSide,
    take_profit: Option<Decimal>,
    stop_loss: Option<Decimal>,
    command: &'static str,
) -> Result<(), CliError> {
    let (Some(take_profit), Some(stop_loss)) = (take_profit, stop_loss) else {
        return Ok(());
    };
    let valid = match close_side {
        OrderSide::Sell => take_profit > stop_loss,
        OrderSide::Buy => take_profit < stop_loss,
    };
    if valid {
        Ok(())
    } else {
        let hint = match close_side {
            OrderSide::Sell => {
                "closing a long position requires --take-profit greater than --stop-loss"
            }
            OrderSide::Buy => {
                "closing a short position requires --take-profit less than --stop-loss"
            }
        };
        Err(CliError::Configuration(format!("{command}: {hint}")))
    }
}

/// How an order shows up in a cancel or modify: its identifier, parsed once.
pub(super) fn order_identifier(
    order_id: Option<u64>,
    cloid: Option<&str>,
    command: &str,
    parse: fn(&str) -> Result<Cloid, CliError>,
) -> Result<OrderIdentifier, CliError> {
    if let Some(order_id) = order_id {
        return Ok(OrderIdentifier::Oid(order_id));
    }
    let raw = cloid.ok_or_else(|| {
        CliError::Configuration(format!("orders {command} requires ORDER_ID or --cloid"))
    })?;
    Ok(OrderIdentifier::Cloid {
        raw: raw.to_string(),
        parsed: parse(raw)?,
    })
}

pub(super) async fn prepare_cancel_order_plan(
    client: &HttpClient,
    resolver: &AssetResolver,
    user: Address,
    args: &CancelArgs,
) -> Result<CancelOrderPlan, CliError> {
    let identifier = cancel_identifier(args)?;
    let order = lookup_order(client, user, &identifier).await?;
    let asset = order_asset(client, resolver, &order).await?.index;

    let action = match &identifier {
        OrderIdentifier::Oid(oid) => Action::Cancel(BatchCancel {
            cancels: vec![Cancel { asset, oid: *oid }],
            fast: false,
        }),
        OrderIdentifier::Cloid { parsed, .. } => Action::CancelByCloid(BatchCancelCloid {
            cancels: vec![CancelByCloid {
                asset: asset.try_into().map_err(|_| {
                    CliError::Internal(anyhow::anyhow!(
                        "asset index {asset} does not fit CLOID cancel request"
                    ))
                })?,
                cloid: *parsed,
            }],
            fast: false,
        }),
    };

    Ok(CancelOrderPlan {
        action,
        coin: resolver.display_coin(&order.coin),
        order_id: args.order_id,
        cloid: args.cloid.clone(),
    })
}

/// A cancel looks the order up leniently: a short or unprefixed CLOID is padded.
fn cancel_identifier(args: &CancelArgs) -> Result<OrderIdentifier, CliError> {
    order_identifier(
        args.order_id,
        args.cloid.as_deref(),
        "cancel",
        parse_lookup_cloid,
    )
}

/// The protocol asset an existing order trades, and how its prices and sizes are
/// constrained when the market is known.
struct OrderAsset {
    index: usize,
    precision: Option<MarketPrecision>,
}

impl OrderAsset {
    fn price(&self, price: Decimal, coin: &str) -> Result<Decimal, CliError> {
        self.precision
            .map_or(Ok(price), |precision| precision.price(price, coin))
    }

    fn size(&self, size: Decimal, coin: &str) -> Result<Decimal, CliError> {
        self.precision
            .map_or(Ok(size), |precision| precision.size(size, coin))
    }
}

async fn order_asset(
    client: &HttpClient,
    resolver: &AssetResolver,
    order: &BasicOrder,
) -> Result<OrderAsset, CliError> {
    if let Some(internal_id) = parse_spot_coin_key(&order.coin) {
        return Ok(
            match resolver.spot_by_protocol_asset_id(spot_asset_index(internal_id)) {
                Some(spot) => OrderAsset {
                    index: spot.index,
                    precision: Some(MarketPrecision::spot(spot.base_sz_decimals)),
                },
                None => OrderAsset {
                    index: internal_id,
                    precision: None,
                },
            },
        );
    }

    let market = resolver.resolve_or_fetch(client, &order.coin).await?;
    Ok(OrderAsset {
        index: market.asset_index()?,
        precision: Some(MarketPrecision::of(&market)),
    })
}

pub(super) async fn prepare_modify_order_plan(
    client: &HttpClient,
    resolver: &AssetResolver,
    user: Address,
    args: &ModifyArgs,
) -> Result<ModifyOrderPlan, CliError> {
    let identifier = order_identifier(args.order_id, args.cloid.as_deref(), "modify", parse_cloid)?;
    let order = lookup_order(client, user, &identifier).await?;
    let asset = order_asset(client, resolver, &order).await?;
    let coin = resolver.display_coin(&order.coin);
    let new_price = args
        .price
        .map(|price| asset.price(price, &coin))
        .transpose()?;
    let new_trigger = args
        .trigger_price
        .map(|price| asset.price(price, &coin))
        .transpose()?;
    let new_size = args.size.map(|size| asset.size(size, &coin)).transpose()?;
    let price = new_price.unwrap_or(order.limit_px);
    let size = new_size.unwrap_or(order.sz);
    let order_type = order_type_placement(&order, new_trigger, new_price)?;
    // The exchange rejects an action hashed with `a: false` unless the
    // replacement is a non-trigger ALO or a non-executable GTC order, so fail
    // here rather than after the cancel succeeds.
    if !args.always_place {
        let needs_always_place = match &order_type {
            OrderTypePlacement::Trigger { .. } => true,
            OrderTypePlacement::Limit { tif } => {
                matches!(tif, TimeInForce::Ioc | TimeInForce::FrontendMarket)
            }
        };
        if needs_always_place {
            return Err(CliError::Configuration(
                "orders modify without --always-place can only place an ALO or non-executable GTC replacement; add --always-place to modify trigger orders or place an IOC replacement"
                    .to_string(),
            ));
        }
    }
    let replacement = OrderRequest {
        asset: asset.index,
        is_buy: matches!(order.side, Side::Bid),
        limit_px: price,
        sz: size,
        reduce_only: order.reduce_only,
        order_type,
        cloid: wire_cloid(order.cloid),
    };
    Ok(ModifyOrderPlan {
        action: Action::BatchModify(BatchModify {
            modifies: vec![Modify {
                oid: identifier.to_status_identifier(),
                order: replacement,
            }],
            always_place: args.always_place,
        }),
        confirmation: ModifyConfirmation {
            coin,
            status: "modified".to_string(),
            order_id: Some(order.oid),
            cloid: args
                .cloid
                .clone()
                .or_else(|| order.cloid.map(|cloid| format!("{cloid:#x}"))),
            price: price.to_string(),
            size: size.to_string(),
        },
    })
}

pub(super) async fn prepare_cancel_all_orders_plan(
    client: &HttpClient,
    resolver: &AssetResolver,
    user: Address,
    args: &CancelAllArgs,
) -> Result<CancelAllOrdersPlan, CliError> {
    let open_orders = client.open_orders(user, None).await.map_err(from_sdk)?;
    let coin_filter =
        resolve_cancel_all_coin_filter(resolver, args.coin.as_deref(), args.dex.as_deref())?;
    let orders = filter_orders_by_coin(open_orders, coin_filter.as_deref());

    let action = if orders.is_empty() {
        None
    } else {
        let mut cancels = Vec::with_capacity(orders.len());
        for order in &orders {
            cancels.push(Cancel {
                asset: order_asset(client, resolver, order).await?.index,
                oid: order.oid,
            });
        }
        Some(Action::Cancel(BatchCancel {
            cancels,
            fast: false,
        }))
    };

    Ok(CancelAllOrdersPlan {
        action,
        cancelled_orders: orders.len(),
        summary_coin: args.coin.clone().unwrap_or_else(|| "ALL".to_string()),
    })
}

pub(super) fn prepare_twap_create_plan(
    perp: &PerpAsset,
    args: &TwapCreateArgs,
) -> Result<TwapCreatePlan, CliError> {
    let minutes = validate_twap_create_args(args)?;
    let coin = perp.display_name();
    let size = MarketPrecision::perp(perp.sz_decimals).size(args.size, &coin)?;
    Ok(TwapCreatePlan {
        action: TwapExchangeAction::TwapOrder {
            twap: TwapOrderAction {
                a: perp.index,
                b: args.side.is_buy(),
                s: size.normalize().to_string(),
                r: false,
                m: minutes,
                t: args.randomize,
            },
        },
        perp: perp.clone(),
        coin,
        side: args.side.to_string(),
        size,
        duration_seconds: args.duration,
        duration_minutes: minutes,
        randomize: args.randomize,
        margin_mode: args.margin_mode,
    })
}

pub(super) fn prepare_twap_cancel_plan(perp: &PerpAsset, args: &TwapCancelArgs) -> TwapCancelPlan {
    TwapCancelPlan {
        action: TwapExchangeAction::TwapCancel {
            a: perp.index,
            t: args.twap_id,
        },
        coin: perp.display_name(),
        twap_id: args.twap_id,
    }
}

pub(super) fn prepare_schedule_cancel_plan(
    args: &ScheduleCancelArgs,
) -> Result<ScheduleCancelPlan, CliError> {
    match (args.in_duration, args.clear) {
        (Some(in_duration), false) => {
            if in_duration.as_secs() < 5 {
                return Err(CliError::Configuration(
                    "orders schedule-cancel --in must be at least 5s".to_string(),
                ));
            }
            let delay = chrono::Duration::from_std(in_duration)
                .map_err(|err| CliError::Configuration(format!("invalid --in duration: {err}")))?;
            Ok(ScheduleCancelPlan::Set {
                delay,
                in_seconds: in_duration.as_secs(),
            })
        }
        (None, true) => Ok(ScheduleCancelPlan::Clear),
        (None, false) => Err(CliError::Configuration(
            "either --in <DURATION> or --clear is required".to_string(),
        )),
        (Some(_), true) => Err(CliError::Configuration(
            "--in and --clear cannot be used together".to_string(),
        )),
    }
}

// Dry-run: each preview renders a plan; nothing here plans anything itself.

/// `orders create` dry-run details for the planned order and its TP/SL children.
pub(super) fn create_dry_run_preview(
    submission: &CreateOrderSubmission,
    args: &CreateArgs,
) -> Result<serde_json::Value, CliError> {
    let mut preview = prepared_order_dry_run_args(submission.parent(), args)?;
    if let CreateOrderSubmission::NormalTpsl(order) = submission {
        append_tpsl_preview(&mut preview, &order.batch)?;
    }
    Ok(preview)
}

pub(super) fn create_dry_run_plan(
    submission: &CreateOrderSubmission,
    args: &CreateArgs,
) -> Result<OrderDryRunPlan, CliError> {
    Ok(OrderDryRunPlan::new(
        "orders create",
        "submit_order",
        create_dry_run_preview(submission, args)?,
    )
    .acting_for(&args.on_behalf_of))
}

pub(super) fn scale_dry_run_plan(
    prepared: &PreparedOrderBatch,
    args: &ScaleArgs,
) -> OrderDryRunPlan {
    let mut preview = order_batch_dry_run_args(prepared);
    insert_margin_mode(&mut preview, args.margin_mode);
    OrderDryRunPlan::new("orders scale", "submit_order_batch", preview)
        .acting_for(&args.on_behalf_of)
}

pub(super) fn batch_create_dry_run_plan(
    prepared: &PreparedOrderBatch,
    args: &BatchCreateArgs,
) -> OrderDryRunPlan {
    let mut preview = order_batch_dry_run_args(prepared);
    if let Some(object) = preview.as_object_mut() {
        object.insert(
            "orders_file".to_string(),
            serde_json::json!(args.orders_file),
        );
    }
    OrderDryRunPlan::new("orders batch-create", "submit_order_batch", preview)
        .acting_for(&args.on_behalf_of)
}

pub(super) fn twap_create_dry_run_plan(
    plan: &TwapCreatePlan,
    args: &TwapCreateArgs,
) -> OrderDryRunPlan {
    let mut preview = serde_json::json!({
        "coin": args.coin,
        "dex": args.dex,
        "side": plan.side,
        "size": plan.size.to_string(),
        "duration": plan.duration_seconds,
        "duration_minutes": plan.duration_minutes,
        "randomize": plan.randomize,
        "asset_id": plan.perp.index,
        "resolved_asset": plan.coin,
    });
    insert_margin_mode(&mut preview, plan.margin_mode);
    OrderDryRunPlan::new("orders twap-create", "create_twap_order", preview)
        .acting_for(&args.on_behalf_of)
}

pub(super) fn twap_cancel_dry_run_plan(
    plan: &TwapCancelPlan,
    args: &TwapCancelArgs,
) -> OrderDryRunPlan {
    OrderDryRunPlan::new(
        "orders twap-cancel",
        "cancel_twap_order",
        serde_json::json!({
            "twap_id": plan.twap_id,
            "coin": args.coin,
            "dex": args.dex,
            "resolved_asset": plan.coin,
        }),
    )
    .acting_for(&args.on_behalf_of)
}

pub(super) fn schedule_cancel_dry_run_plan(
    plan: &ScheduleCancelPlan,
    args: &ScheduleCancelArgs,
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
    OrderDryRunPlan::new(
        "orders schedule-cancel",
        "schedule_dead_mans_switch",
        preview,
    )
    .acting_for(&args.on_behalf_of)
}

pub(super) fn modify_dry_run_plan(args: &ModifyArgs) -> OrderDryRunPlan {
    OrderDryRunPlan::new(
        "orders modify",
        "modify_order",
        serde_json::json!({
            "order_id": args.order_id,
            "cloid": args.cloid,
            "on_behalf_of": args.on_behalf_of,
            "price": args.price.map(|value| value.to_string()),
            "trigger_price": args.trigger_price.map(|value| value.to_string()),
            "size": args.size.map(|value| value.to_string()),
            "always_place": args.always_place,
        }),
    )
}

pub(super) fn cancel_dry_run_plan(args: &CancelArgs) -> Result<OrderDryRunPlan, CliError> {
    let identifier = cancel_identifier(args)?;
    Ok(OrderDryRunPlan::new(
        "orders cancel",
        "cancel_order",
        serde_json::json!({
            "order_id": args.order_id,
            "cloid": args.cloid,
            "on_behalf_of": args.on_behalf_of,
            "identifier": identifier.display(),
        }),
    ))
}

pub(super) fn cancel_all_dry_run_plan(
    resolver: &AssetResolver,
    args: &CancelAllArgs,
) -> Result<OrderDryRunPlan, CliError> {
    let coin_filter =
        resolve_cancel_all_coin_filter(resolver, args.coin.as_deref(), args.dex.as_deref())?;
    Ok(OrderDryRunPlan::new(
        "orders cancel-all",
        "cancel_open_orders",
        serde_json::json!({
            "coin": args.coin,
            "dex": args.dex,
            "on_behalf_of": args.on_behalf_of,
            "resolved_coin_filter": coin_filter,
        }),
    ))
}

fn prepared_order_dry_run_args(
    prepared: &PreparedOrder,
    args: &CreateArgs,
) -> Result<serde_json::Value, CliError> {
    let (is_market, trigger_px, tpsl) = match &prepared.request.order_type {
        OrderTypePlacement::Trigger {
            is_market,
            trigger_px,
            tpsl,
        } => (
            Some(*is_market),
            Some(*trigger_px),
            Some(tpsl_to_str(*tpsl)),
        ),
        OrderTypePlacement::Limit { .. } => (None, None, None),
    };

    let mut value = serde_json::json!({
        "coin": args.coin.clone(),
        "dex": args.dex.clone(),
        "side": args.side.to_string(),
        "type": args.order_type.to_string(),
        "asset_id": prepared.request.asset,
        "resolved_asset": prepared.coin.clone(),
        "limit_px": prepared.request.limit_px.to_string(),
        "trigger_px": trigger_px.map(|value| value.to_string()),
        "is_market": is_market,
        "tpsl": tpsl,
        "size": prepared.request.sz.to_string(),
        "amount": prepared.amount.map(|value| value.to_string()),
        "amount_unit": prepared.amount.as_ref().map(|_| prepared.amount_unit.clone()),
        "tif": prepared.tif.map(|tif| tif.to_string()),
        "reduce_only": prepared.request.reduce_only,
        "cloid": args.cloid.clone(),
        "max_slippage_bps": args.max_slippage_bps,
        "take_profit": args.take_profit.map(|value| value.to_string()),
        "stop_loss": args.stop_loss.map(|value| value.to_string()),
        "grouping": args.grouping.map(|grouping| TpslGroupingArg::from(grouping).to_string()),
        "builder": prepared.builder.as_ref().map(|builder| serde_json::json!({
            "b": builder.b.to_string(),
            "f": builder.f,
        })),
        "builder_address": args.builder.clone(),
        "builder_fee_rate": args.builder_fee_rate.clone(),
    });
    match &prepared.market {
        Market::Outcome(outcome) => {
            let notation = outcome.parse()?;
            if let Some(object) = value.as_object_mut() {
                object.insert("outcome".to_string(), serde_json::json!({
                    "outcome": notation.outcome,
                    "side": notation.side,
                    "encoding": notation.encoding,
                    "asset_id": prepared.request.asset,
                    "notation": args.coin,
                    "live_submission": "enabled: outcome order encoding is verified; use explicit --price and --size"
                }));
            }
        }
        Market::Perp(_) => insert_margin_mode(&mut value, args.margin_mode),
        Market::Spot(_) => {}
    }
    Ok(value)
}

fn order_batch_dry_run_args(prepared: &PreparedOrderBatch) -> serde_json::Value {
    let legs = prepared
        .legs
        .iter()
        .map(|leg| {
            let order = &leg.order;
            let (is_market, trigger_px, tpsl) = match &order.request.order_type {
                OrderTypePlacement::Trigger {
                    is_market,
                    trigger_px,
                    tpsl,
                } => (
                    Some(*is_market),
                    Some(trigger_px.to_string()),
                    Some(tpsl_to_str(*tpsl)),
                ),
                OrderTypePlacement::Limit { .. } => (None, None, None),
            };
            serde_json::json!({
                "leg_index": leg.leg_index,
                "coin": order.coin,
                "side": order.side.to_string(),
                "type": order.order_type.to_string(),
                "asset_id": order.request.asset,
                "resolved_asset": order.coin,
                "limit_px": order.request.limit_px.to_string(),
                "trigger_px": trigger_px,
                "is_market": is_market,
                "tpsl": tpsl,
                "price": order.price.to_string(),
                "size": order.request.sz.to_string(),
                "amount": order.amount.map(|value| value.to_string()),
                "amount_unit": order.amount.as_ref().map(|_| order.amount_unit.clone()),
                "tif": order.tif.map(|tif| tif.to_string()),
                "reduce_only": order.request.reduce_only,
                "cloid": cloid_from_wire(order.request.cloid).map(|cloid| cloid.to_string()),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "grouping": "na",
        "order_count": prepared.legs.len(),
        "orders": legs,
    })
}

fn tpsl_to_str(tpsl: TpSl) -> &'static str {
    match tpsl {
        TpSl::Tp => "tp",
        TpSl::Sl => "sl",
    }
}

/// `orders tpsl` dry-run details: what the live command would send, without a
/// signer or a position lookup. The legs come from the same
/// [`position_tpsl_triggers`] and [`position_tpsl_batch`] the live order uses.
pub(super) fn tpsl_dry_run_plan(
    perp: &PerpAsset,
    args: &TpslArgs,
) -> Result<OrderDryRunPlan, CliError> {
    let (absolute_tp, absolute_sl) =
        absolute_trigger_prices(args.take_profit.as_ref(), args.stop_loss.as_ref());
    let (absolute_tp, absolute_sl) = truncate_triggers(perp, absolute_tp, absolute_sl)?;
    let inferred_side = args
        .side
        .or_else(|| infer_close_side_from_tpsl_prices(absolute_tp, absolute_sl));
    let needs_entry = trigger_specs_need_entry(args.take_profit.as_ref(), args.stop_loss.as_ref());
    let mut preview = serde_json::json!({
        "coin": args.coin,
        "dex": args.dex,
        "on_behalf_of": args.on_behalf_of,
        "take_profit": args.take_profit.as_ref().map(TriggerPriceSpec::display),
        "stop_loss": args.stop_loss.as_ref().map(TriggerPriceSpec::display),
        "grouping": TpslGroupingArg::from(args.grouping).to_string(),
        "grouping_wire": TpslGroupingArg::from(args.grouping).wire_value(),
        "resolved_asset": perp.display_name(),
        "side": inferred_side.map(|side| side.to_string()),
        "size": args.size.map(|value| value.to_string()),
        "size_mode": if args.size.is_some() { "fixed" } else { "current_position" },
    });
    insert_margin_mode(&mut preview, args.margin_mode);

    match (args.side, args.size) {
        (Some(close_side), Some(size)) => {
            // Percent and entry triggers need the position's entry price, which
            // a dry run does not read: it previews no legs for them.
            if !needs_entry {
                let size =
                    MarketPrecision::perp(perp.sz_decimals).size(size, &perp.display_name())?;
                let (take_profit, stop_loss) =
                    position_tpsl_triggers(perp, args, close_side, None)?;
                let cloid = args.cloid.as_deref().map(parse_cloid).transpose()?;
                let batch =
                    position_tpsl_batch(perp, close_side, size, take_profit, stop_loss, cloid);
                append_tpsl_preview(&mut preview, &batch)?;
            }
        }
        _ => {
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

    Ok(OrderDryRunPlan::new(
        "orders tpsl",
        "submit_position_tpsl",
        preview,
    ))
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
        let tpsl_str = tpsl_to_str(tpsl);
        let child = serde_json::json!({
            "asset": asset,
            "is_buy": inferred_side.map(OrderSide::is_buy),
            "limit_px": "0",
            "sz": null,
            "reduce_only": true,
            "trigger_px": price.to_string(),
            "is_market": true,
            "tpsl": tpsl_str,
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
                    "tpsl": tpsl_str,
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
