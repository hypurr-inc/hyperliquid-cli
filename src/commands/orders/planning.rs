//! Planning: turn validated arguments into the orders and actions to send.
//!
//! A plan is built once and read by every later stage: the dry-run preview
//! renders it, the confirmation prompt summarizes it, and submission sends it.

use std::collections::HashMap;

use chrono::Utc;
use hypersdk::hypercore::Cloid;
use hypersdk::hypercore::types::{
    Action, AssetPosition, OrderRequest, OrderTypePlacement, TimeInForce, TpSl, TrailingStop,
    TrailingStopRetracement, TwapOrderParams,
};
use hypersdk::{Address, Decimal};

use super::args::{
    BatchCreateOrder, ScaleArgs, ScheduleCancelArgs, TrailingStopArgs, TriggerPriceSpec,
    TwapCancelArgs, TwapCreateArgs,
};
use super::cloid::{parse_cloid, wire_cloid};
use super::limits::SCHEDULE_CANCEL_MIN_SECONDS;
use super::prepared::{
    CreateOrderSubmission, NormalTpslOrder, PreparedOrder, PreparedOrderBatch, PreparedTpslBatch,
    PreparedTpslLeg, QuoteAmount, TpslLegKind, order_batch, priority_grouping,
};
use super::preview::OrderDryRunPlan;
use super::validation::{
    AttachedTpsl, BuilderRequest, ClosingSize, MarketPrecision, MarketSizing, OrderKind, OrderSpec,
    ScaleLadder, TpslSpec, validate_order_for_market, validate_twap_create_args,
};
use super::vocab::{CreateOrderType, OrderSide, TifArg, TpslGroupingArg};
use crate::assets::{
    AssetQuery, AssetResolver, Market, OutcomeAsset, PerpAsset, parse_asset_query,
    qualify_dex_asset,
};
use crate::builder_fee::{self, BuilderFee};
use crate::commands::common::position::find_position;
use crate::errors::CliError;
use crate::hints::TRAILING_STOP_DRY_RUN;
use crate::http_api::ApiClient;
use crate::margin::MarginPlan;
use crate::quantity::{require_positive, wire_text};
use crate::settings::Settings;

pub(super) struct TwapCreatePlan {
    pub(super) action: Action,
    pub(super) perp: PerpAsset,
    pub(super) coin: String,
    pub(super) side: String,
    pub(super) size: Decimal,
    pub(super) duration_seconds: u64,
    pub(super) duration_minutes: u64,
    pub(super) randomize: bool,
    /// The margin mode of the TWAP: requested, else what the market's rule
    /// implies.
    pub(super) margin: MarginPlan,
}

pub(super) struct TwapCancelPlan {
    pub(super) action: Action,
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

/// The perp for `--coin` / `--dex`, fetching a HIP-3 market the resolver does
/// not list yet. Command code obtains a perp only here, and planners take the
/// resulting [`PerpAsset`], so none can run against an unresolved market.
pub(super) async fn resolve_perp(
    client: &ApiClient,
    resolver: &AssetResolver,
    dex: Option<&str>,
    coin: &str,
) -> Result<PerpAsset, CliError> {
    resolver
        .resolve_perp_or_fetch(client, &qualify_dex_asset(dex, coin))
        .await
}

/// The perpetual market `coin` and `dex` name, for a command that only trades
/// perpetuals. Like [`resolve_perp`], but a market of another kind is named
/// in the error, as `command` (such as `orders bracket`) refusing it.
pub(super) async fn resolve_perp_market(
    client: &ApiClient,
    resolver: &AssetResolver,
    command: &str,
    dex: Option<&str>,
    coin: &str,
) -> Result<PerpAsset, CliError> {
    match resolve_tradable_asset(client, resolver, coin, dex).await? {
        Market::Perp(perp) => Ok(perp),
        Market::Spot(_) | Market::Outcome(_) => Err(CliError::Unsupported(format!(
            "{command} supports perpetual markets only; {coin} is not a perpetual market"
        ))),
    }
}

async fn resolve_tradable_asset(
    client: &ApiClient,
    resolver: &AssetResolver,
    coin: &str,
    dex: Option<&str>,
) -> Result<Market, CliError> {
    if let AssetQuery::Outcome(notation) = parse_asset_query(coin) {
        if dex.is_some() {
            return Err(CliError::Unsupported(format!(
                "outcome notation '{notation}' cannot be used with --dex; omit --dex for outcome orders"
            )));
        }
        return Ok(Market::Outcome(OutcomeAsset::new(notation)));
    }

    resolver
        .resolve_tradable_or_fetch(client, &qualify_dex_asset(dex, coin))
        .await
}

struct Venue {
    coin: String,
    index: usize,
    precision: MarketPrecision,
    /// The asset an `--amount` is in. An outcome market has none: its orders
    /// are sized in tokens.
    quote: Option<String>,
}

fn venue(market: &Market) -> Result<Venue, CliError> {
    let (coin, index, quote) = match market {
        Market::Perp(perp) => (
            perp.display_name(),
            perp.index,
            Some(perp.collateral.clone()),
        ),
        Market::Spot(spot) => (spot.symbol.clone(), spot.index, Some(spot.quote.clone())),
        Market::Outcome(outcome) => (outcome.notation().to_string(), outcome.asset_index()?, None),
    };
    Ok(Venue {
        coin,
        index,
        precision: MarketPrecision::of(market),
        quote,
    })
}

/// The dex whose books list `market`: a HIP-3 perp's own dex, otherwise the
/// default one. Every read of the market's mids or resting orders names it.
pub(super) fn market_dex(market: &Market) -> Option<String> {
    match market {
        Market::Perp(perp) => perp.dex.clone(),
        Market::Spot(_) | Market::Outcome(_) => None,
    }
}

/// The key `allMids` lists `market` under, and the name an error calls it.
/// `coin` and `dex` are the flags that named the market.
fn mid_key(market: &Market, coin: &str, dex: Option<&str>) -> Result<(String, String), CliError> {
    match market {
        Market::Perp(perp) => Ok((perp.display_name(), qualify_dex_asset(dex, coin))),
        Market::Spot(spot) => Ok((spot.info_coin(), coin.to_string())),
        Market::Outcome(_) => Err(CliError::Unsupported(
            "outcome orders do not support --type market because outcome size decimals are not verified; use an explicit --price and --size"
                .to_string(),
        )),
    }
}

/// `coin` and `dex` are the flags that named it, for the error that says the mid was
/// not available.
pub(super) async fn market_mid_price(
    client: &ApiClient,
    market: &Market,
    coin: &str,
    dex: Option<&str>,
) -> Result<Decimal, CliError> {
    let (key, label) = mid_key(market, coin, dex)?;
    let mids = client.sdk().all_mids(market_dex(market)).await?;
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
) -> Result<Option<BuilderFee>, CliError> {
    let spot_buy = matches!(market, Market::Spot(_)) && side == OrderSide::Buy;
    let (address, fee, sub_builders) = match request {
        BuilderRequest::None => return Ok(None),
        BuilderRequest::Explicit {
            address,
            fee,
            sub_builders,
        } => {
            if spot_buy {
                return Err(CliError::Unsupported(
                    "spot buy orders do not support builder fees; Hyperliquid builder codes apply to spot sells and both sides of perpetual orders"
                        .to_string(),
                ));
            }
            (*address, *fee, sub_builders.clone())
        }
        BuilderRequest::Default => {
            if spot_buy {
                return Ok(None);
            }
            match builder_fee::resolve_default_builder_fee(settings)? {
                Some((address, fee)) => (address, fee, Vec::new()),
                None => return Ok(None),
            }
        }
    };
    let fee = BuilderFee::new(address, fee, sub_builders);
    if matches!(market, Market::Perp(_)) {
        fee.ensure_within_perp_cap()?;
    }
    Ok(Some(fee))
}

pub(super) async fn prepare_order(
    client: &ApiClient,
    resolver: &AssetResolver,
    settings: &Settings,
    spec: &OrderSpec,
) -> Result<PreparedOrder, CliError> {
    let market = resolve_tradable_asset(client, resolver, &spec.coin, spec.dex.as_deref()).await?;
    prepare_order_in(client, &market, settings, spec).await
}

/// A command that places many orders on one market resolves it once and plans each
/// order here.
pub(super) async fn prepare_order_in(
    client: &ApiClient,
    market: &Market,
    settings: &Settings,
    spec: &OrderSpec,
) -> Result<PreparedOrder, CliError> {
    let margin = validate_order_for_market(spec, market)?;
    let builder = order_builder_fee(settings, &spec.builder, market, spec.side)?;
    plan_order(client, market, spec, builder, margin).await
}

async fn plan_order(
    client: &ApiClient,
    market: &Market,
    spec: &OrderSpec,
    builder: Option<BuilderFee>,
    margin: Option<MarginPlan>,
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
            let mid = market_mid_price(client, market, &spec.coin, spec.dex.as_deref()).await?;
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
                    let unit = quote.clone().ok_or_else(|| {
                        CliError::Internal(anyhow::anyhow!(
                            "{coin} has no quote asset to size an amount in"
                        ))
                    })?;
                    (
                        size,
                        Some(QuoteAmount {
                            value: amount,
                            unit,
                        }),
                    )
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
        coin,
        side: spec.side,
        order_type: spec.kind.order_type(),
        tif,
        trigger_price,
        amount,
        warning,
        builder,
        margin,
        priority_rate: spec.priority_rate,
    })
}

pub(super) async fn prepare_create_order(
    client: &ApiClient,
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

pub(super) async fn prepare_scale_batch(
    client: &ApiClient,
    resolver: &AssetResolver,
    settings: &Settings,
    args: &ScaleArgs,
    ladder: &ScaleLadder,
) -> Result<PreparedOrderBatch, CliError> {
    let leg_size = ladder.leg_size()?;
    let specs = ladder
        .prices()
        .into_iter()
        .map(|price| {
            OrderSpec::limit_leg(
                &args.coin,
                args.dex.as_deref(),
                args.side,
                price,
                leg_size,
                args.tif,
            )
            .map(|spec| {
                spec.reduce_only(args.reduce_only)
                    .with_margin_mode(args.margin_mode)
                    .with_priority_rate(args.priority_rate)
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = prepare_order_batch(client, resolver, settings, specs).await?;

    let placed_size: Decimal = prepared.legs.iter().map(PreparedOrder::size).sum();
    if placed_size != ladder.total_size() {
        return Err(CliError::Configuration(
            "orders scale total size cannot be evenly distributed across requested orders at the market's lot size"
                .to_string(),
        ));
    }
    if prepared
        .legs
        .windows(2)
        .any(|pair| pair[0].price() == pair[1].price())
    {
        return Err(CliError::Configuration(format!(
            "orders scale prices collapse onto the same tick for {}; use fewer --orders or a wider price range",
            prepared.legs[0].coin
        )));
    }
    Ok(prepared)
}

pub(super) async fn prepare_batch_create_orders(
    client: &ApiClient,
    resolver: &AssetResolver,
    settings: &Settings,
    orders: Vec<BatchCreateOrder>,
    priority_rate: Option<u32>,
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
            .with_cloid(cloid)
            .with_priority_rate(priority_rate))
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    prepare_order_batch(client, resolver, settings, specs).await
}

async fn prepare_order_batch(
    client: &ApiClient,
    resolver: &AssetResolver,
    settings: &Settings,
    specs: Vec<OrderSpec>,
) -> Result<PreparedOrderBatch, CliError> {
    let mut markets: HashMap<(String, Option<String>), Market> = HashMap::new();
    let mut legs = Vec::with_capacity(specs.len());
    for spec in &specs {
        let key = (spec.coin.clone(), spec.dex.clone());
        if !markets.contains_key(&key) {
            let market =
                resolve_tradable_asset(client, resolver, &spec.coin, spec.dex.as_deref()).await?;
            markets.insert(key.clone(), market);
        }
        legs.push(prepare_order_in(client, &markets[&key], settings, spec).await?);
    }
    // Every leg of one batch shares the rate; it is one action's grouping.
    let priority_rate = specs.first().and_then(|spec| spec.priority_rate);
    if priority_rate.is_some() {
        // Each leg is IOC or ALO already; the exchange wants them all the same.
        let all = |tif: TifArg| legs.iter().all(|leg| leg.tif == Some(tif));
        if !all(TifArg::Ioc) && !all(TifArg::Alo) {
            return Err(CliError::Unsupported(
                "--priority-rate needs every order in the batch to be IOC, or every order to be ALO; Hyperliquid refuses a batch that mixes them"
                    .to_string(),
            ));
        }
    }
    let batch = order_batch(
        legs.iter().map(|leg| leg.request.clone()).collect(),
        priority_grouping(priority_rate),
    );
    Ok(PreparedOrderBatch { batch, legs })
}

pub(super) fn prepare_normal_tpsl_batch(
    parent: &PreparedOrder,
    tpsl: &AttachedTpsl,
) -> Result<PreparedTpslBatch, CliError> {
    let close_side = parent.side.opposite();
    let truncate = |price: Decimal| MarketPrecision::of(&parent.market).price(price, &parent.coin);
    let take_profit = tpsl.take_profit.map(truncate).transpose()?;
    let stop_loss = tpsl.stop_loss.map(truncate).transpose()?;
    validate_tpsl_price_ordering(close_side, take_profit, stop_loss, "orders create")?;

    let mut legs = vec![PreparedTpslLeg {
        request: parent.request.clone(),
        leg: TpslLegKind::Parent,
        coin: parent.coin.clone(),
        side: parent.side,
        order_type: parent.order_type,
        price: parent.price(),
        size: parent.size(),
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

struct TpslChildren<'a> {
    asset: usize,
    coin: &'a str,
    size: Decimal,
    side: OrderSide,
}

impl TpslChildren<'_> {
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
    let batch = order_batch(
        legs.iter().map(|leg| leg.request.clone()).collect(),
        grouping.into(),
    );
    PreparedTpslBatch {
        batch,
        grouping,
        legs,
    }
}

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

pub(super) fn truncate_triggers(
    perp: &PerpAsset,
    take_profit: Option<Decimal>,
    stop_loss: Option<Decimal>,
) -> Result<(Option<Decimal>, Option<Decimal>), CliError> {
    let precision = MarketPrecision::of_perp(perp);
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
pub(super) fn position_tpsl_triggers(
    perp: &PerpAsset,
    spec: &TpslSpec,
    close_side: OrderSide,
    entry: Option<Decimal>,
) -> Result<(Option<Decimal>, Option<Decimal>), CliError> {
    let (take_profit, stop_loss) = resolve_trigger_prices(
        spec.take_profit.as_ref(),
        spec.stop_loss.as_ref(),
        entry,
        close_side.opposite(),
    )?;
    let (take_profit, stop_loss) = truncate_triggers(perp, take_profit, stop_loss)?;
    validate_tpsl_price_ordering(close_side, take_profit, stop_loss, "orders tpsl")?;
    Ok((take_profit, stop_loss))
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
    client: &ApiClient,
    perp: &PerpAsset,
    user: Address,
    spec: &TpslSpec,
) -> Result<PreparedTpslBatch, CliError> {
    let (close_side, size, entry) = match spec.close {
        ClosingSize::Fixed { side, size } => {
            let entry =
                if trigger_specs_need_entry(spec.take_profit.as_ref(), spec.stop_loss.as_ref()) {
                    let position =
                        find_position(client, user, perp, true)
                            .await?
                            .ok_or_else(|| {
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
        ClosingSize::CurrentPosition => {
            let position = find_position(client, user, perp, true).await?.ok_or_else(|| {
                CliError::Configuration(format!(
                    "orders tpsl requires an open {} position or both --side and --size for a fixed-size TP/SL",
                    perp.name
                ))
            })?;
            let (side, size) = position_close_side_and_size(&position)?;
            (side, size, position.position.entry_px)
        }
    };
    let size = MarketPrecision::of_perp(perp).size(size, &perp.display_name())?;
    let (take_profit, stop_loss) = position_tpsl_triggers(perp, spec, close_side, entry)?;
    Ok(position_tpsl_batch(
        perp,
        close_side,
        size,
        take_profit,
        stop_loss,
        spec.cloid,
    ))
}

pub(super) fn trigger_specs_need_entry(
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
    let resolve = |spec: &TriggerPriceSpec| match entry {
        Some(entry) => spec.resolve(entry, position_side),
        None => spec.absolute().ok_or_else(|| {
            CliError::Unsupported(
                "percent and entry TP/SL require an open position with an entry price".to_string(),
            )
        }),
    };
    Ok((
        take_profit.map(resolve).transpose()?,
        stop_loss.map(resolve).transpose()?,
    ))
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

pub(super) fn prepare_twap_create_plan(
    perp: &PerpAsset,
    args: &TwapCreateArgs,
) -> Result<TwapCreatePlan, CliError> {
    let margin = MarginPlan::for_market(perp, args.margin_mode)?;
    let minutes = validate_twap_create_args(args)?;
    let coin = perp.display_name();
    let size = MarketPrecision::of_perp(perp).size(args.size, &coin)?;
    Ok(TwapCreatePlan {
        action: Action::TwapOrder {
            twap: TwapOrderParams {
                a: perp.index,
                b: args.side.is_buy(),
                s: size.normalize(),
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
        duration_minutes: u64::from(minutes),
        randomize: args.randomize,
        margin,
    })
}

pub(super) fn prepare_twap_cancel_plan(perp: &PerpAsset, args: &TwapCancelArgs) -> TwapCancelPlan {
    TwapCancelPlan {
        action: Action::TwapCancel {
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
            if in_duration.as_secs() < SCHEDULE_CANCEL_MIN_SECONDS {
                return Err(CliError::Configuration(format!(
                    "orders schedule-cancel --in must be at least {SCHEDULE_CANCEL_MIN_SECONDS}s"
                )));
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

/// The retracement the user asked for. Exactly one option must be set.
fn retracement_from_args(args: &TrailingStopArgs) -> Result<TrailingStopRetracement, CliError> {
    match (args.retracement_pct, args.retracement_px) {
        (Some(pct), None) => {
            if pct <= Decimal::ZERO || pct >= Decimal::ONE_HUNDRED {
                return Err(CliError::Configuration(
                    "--retracement-pct must be greater than 0 and less than 100".to_string(),
                ));
            }
            Ok(TrailingStopRetracement::Pct(pct))
        }
        (None, Some(px)) => {
            require_positive(px, "--retracement-px")?;
            Ok(TrailingStopRetracement::Px(px))
        }
        _ => Err(CliError::Configuration(format!(
            "orders trailing-stop requires exactly one of --retracement-pct or --retracement-px.\n  \
             {TRAILING_STOP_DRY_RUN}"
        ))),
    }
}

/// Validate `orders trailing-stop` arguments before auth or network access.
pub(super) fn validate_trailing_stop_args(args: &TrailingStopArgs) -> Result<(), CliError> {
    match parse_asset_query(&qualify_dex_asset(args.dex.as_deref(), &args.coin)) {
        AssetQuery::Spot(_) => {
            return Err(CliError::Configuration(format!(
                "orders trailing-stop supports perpetual markets only; {} is a spot market",
                args.coin
            )));
        }
        AssetQuery::Outcome(_) => {
            return Err(CliError::Configuration(format!(
                "orders trailing-stop supports perpetual markets only; {} is an outcome market",
                args.coin
            )));
        }
        AssetQuery::Perp(_) | AssetQuery::Hip3 { .. } => {}
    }
    require_positive(args.size, "--size")?;
    retracement_from_args(args)?;
    if let Some(activation) = args.activation_price {
        require_positive(activation, "--activation-price")?;
    }
    Ok(())
}

pub(super) struct TrailingStopPlan {
    action: TrailingStop,
    coin: String,
    side: OrderSide,
    size: Decimal,
    retracement: TrailingStopRetracement,
    activation_px: Option<Decimal>,
}

impl TrailingStopPlan {
    pub(super) fn action(&self) -> &TrailingStop {
        &self.action
    }

    pub(super) fn coin(&self) -> &str {
        &self.coin
    }

    pub(super) fn side(&self) -> OrderSide {
        self.side
    }

    pub(super) fn size(&self) -> Decimal {
        self.size
    }

    pub(super) fn activation_px(&self) -> Option<Decimal> {
        self.activation_px
    }

    pub(super) fn reduce_only(&self) -> bool {
        self.action.reduce_only
    }

    /// The retracement as the exchange spells it: `{ "pct": .. }` or `{ "px": .. }`.
    pub(super) fn retracement_json(&self) -> serde_json::Value {
        match &self.retracement {
            TrailingStopRetracement::Pct(pct) => {
                serde_json::json!({ "pct": wire_text(*pct) })
            }
            TrailingStopRetracement::Px(px) => {
                serde_json::json!({ "px": wire_text(*px) })
            }
        }
    }

    /// The retracement as a person reads it: `5%` or a price distance.
    pub(super) fn retracement_label(&self) -> String {
        match &self.retracement {
            TrailingStopRetracement::Pct(pct) => format!("{}%", wire_text(*pct)),
            TrailingStopRetracement::Px(px) => wire_text(*px),
        }
    }

    pub(super) fn dry_run_plan(&self, args: &TrailingStopArgs) -> OrderDryRunPlan {
        OrderDryRunPlan::new(
            "trailing_stop",
            serde_json::json!({
                "coin": args.coin,
                "dex": args.dex,
                "resolved_asset": self.coin,
                "asset_id": self.action.asset,
                "side": self.side.to_string(),
                "size": self.size.to_string(),
                "reduce_only": self.reduce_only(),
                "retracement": self.retracement_json(),
                "activation_px": self.activation_px.map(|price| price.to_string()),
            }),
        )
    }
}

pub(super) fn prepare_trailing_stop_plan(
    perp: &PerpAsset,
    args: &TrailingStopArgs,
) -> Result<TrailingStopPlan, CliError> {
    let retracement = retracement_from_args(args)?;
    let coin = perp.display_name();
    let precision = MarketPrecision::of_perp(perp);
    let size = precision.size(args.size, &coin)?;
    let activation_px = args
        .activation_price
        .map(|price| precision.price(price, &coin))
        .transpose()?;
    Ok(TrailingStopPlan {
        action: TrailingStop {
            asset: perp.index,
            is_buy: args.side.is_buy(),
            sz: size,
            reduce_only: args.reduce_only,
            retracement,
            activation_px,
        },
        coin,
        side: args.side,
        size,
        retracement,
        activation_px,
    })
}

#[cfg(test)]
mod tests;
