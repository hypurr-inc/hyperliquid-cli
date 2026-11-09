use std::time::Duration;

use hypersdk::hypercore::types::TimeInForce;

use super::*;
use crate::assets::test_support::hip3_perp;
use crate::assets::{AssetMetadata, MarginRule, SpotAsset};
use crate::commands::orders::args::{
    CreateTpslGroupingArg, DEFAULT_MARKET_ORDER_SLIPPAGE_BPS, PositionTpslGroupingArg, TpslArgs,
};
use crate::commands::orders::validation::{MAX_BATCH_ORDER_COUNT, validate_order_for_market};
use crate::env::EnvSource;
use crate::http_api::ApiClient;
use crate::network::Network;

fn resolver() -> AssetResolver {
    AssetResolver::new(AssetMetadata::from_assets(
        vec![
            PerpAsset {
                name: "BTC".to_string(),
                index: 0,
                dex: None,
                sz_decimals: 5,
                collateral: "USDC".to_string(),
                margin_rule: MarginRule::CrossOrIsolated,
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

fn settings() -> Settings {
    Settings::detached(EnvSource::fixed::<&str, &str>([]), None)
}

async fn prepare(args: &CreateArgs) -> Result<PreparedOrder, CliError> {
    let client = ApiClient::new(Network::Testnet, None);
    prepare_order(
        &client,
        &resolver(),
        &settings(),
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
        sub_builder: Vec::new(),
        priority_rate: None,
        cloid: None,
        yes: false,
    }
}

#[test]
fn market_limit_price_rounds_buy_up_to_perp_tick() {
    let price = MarketPrecision::of_perp(&btc())
        .marketable_price(Decimal::new(9_472_033, 2), OrderSide::Buy, 200)
        .unwrap();

    assert_eq!(price, Decimal::from(96_615));
}

#[test]
fn market_limit_price_rounds_sell_down_to_perp_tick() {
    let price = MarketPrecision::of_perp(&btc())
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
    assert_eq!(
        builder.address_wire(),
        "0x1111111111111111111111111111111111111111"
    );
    assert_eq!(builder.tenths_bps(), 10);
}

#[tokio::test]
async fn a_perp_builder_fee_above_a_tenth_of_a_percent_is_rejected() {
    let mut args = base_args();
    args.builder = Some("0x1111111111111111111111111111111111111111".to_string());

    args.builder_fee_rate = Some("0.1%".to_string());
    let prepared = prepare(&args).await.unwrap();
    assert_eq!(
        prepared.builder.unwrap().tenths_bps(),
        crate::builder_fee::FeeCap::PerpOrder.tenths_bps()
    );

    args.builder_fee_rate = Some("0.101%".to_string());
    let err = prepare(&args).await.unwrap_err();
    assert_eq!(err.exit_code(), 13);
    assert!(err.to_string().contains("cannot exceed 0.1%"), "{err}");
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
        priority_rate: None,
        yes: false,
    }
}

async fn prepare_scale(args: &ScaleArgs) -> Result<PreparedOrderBatch, CliError> {
    let ladder = ScaleLadder::from_args(args)?;
    prepare_scale_batch(
        &ApiClient::new(Network::Testnet, None),
        &resolver(),
        &settings(),
        args,
        &ladder,
    )
    .await
}

#[test]
fn scale_validation_rejects_unbounded_or_ambiguous_ladders() {
    let mut args = scale_args(
        Decimal::new(1, 1),
        Decimal::from(50_000),
        Decimal::from(50_000),
        2,
    );

    let err = ScaleLadder::from_args(&args).unwrap_err();
    assert!(err.to_string().contains("different start and end"));

    args.end_price = Decimal::from(49_000);
    args.order_count = MAX_BATCH_ORDER_COUNT + 1;
    let err = ScaleLadder::from_args(&args).unwrap_err();
    assert!(err.to_string().contains("at most"));

    args.order_count = 0;
    let err = ScaleLadder::from_args(&args).unwrap_err();
    assert!(err.to_string().contains("--orders greater than zero"));
}

#[test]
fn scale_validation_names_the_flag_that_is_not_positive() {
    let args = scale_args(
        Decimal::ZERO,
        Decimal::from(50_000),
        Decimal::from(49_000),
        2,
    );
    let err = ScaleLadder::from_args(&args).unwrap_err();
    assert!(
        err.to_string()
            .contains("total-size must be greater than zero"),
        "{err}"
    );

    let args = scale_args(Decimal::ONE, Decimal::from(-1), Decimal::from(49_000), 2);
    let err = ScaleLadder::from_args(&args).unwrap_err();
    assert!(
        err.to_string().contains("start-price must be positive"),
        "{err}"
    );
}

#[test]
fn a_ladder_spaces_prices_evenly_and_splits_the_size_equally() {
    let ladder = ScaleLadder::from_args(&scale_args(
        Decimal::new(3, 1),
        Decimal::from(50_000),
        Decimal::from(49_000),
        3,
    ))
    .unwrap();
    assert_eq!(
        ladder.prices(),
        [
            Decimal::from(50_000),
            Decimal::from(49_500),
            Decimal::from(49_000)
        ]
    );
    assert_eq!(ladder.leg_size().unwrap(), Decimal::new(1, 1));

    let single = ScaleLadder::from_args(&scale_args(
        Decimal::ONE,
        Decimal::from(50_000),
        Decimal::from(49_000),
        1,
    ))
    .unwrap();
    assert_eq!(single.prices(), [Decimal::from(50_000)]);
}

#[test]
fn a_ladder_rejects_a_total_size_that_does_not_split_evenly() {
    let ladder = ScaleLadder::from_args(&scale_args(
        Decimal::new(1, 2),
        Decimal::from(50_000),
        Decimal::from(49_000),
        3,
    ))
    .unwrap();
    let err = ladder.leg_size().unwrap_err();
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

#[tokio::test]
async fn scale_places_the_whole_total_size_across_legs() {
    let args = scale_args(
        Decimal::new(3, 1),
        Decimal::from(50_000),
        Decimal::from(49_000),
        3,
    );
    let prepared = prepare_scale(&args).await.unwrap();
    let sizes: Vec<_> = prepared.legs.iter().map(PreparedOrder::size).collect();
    assert_eq!(sizes, vec![Decimal::new(1, 1); 3]);
}

#[tokio::test]
async fn scale_rejects_legs_whose_size_would_be_lost_to_the_lot() {
    // 0.00003 / 2 = 0.000015 per leg, which BTC's 5-decimal lot truncates to
    // 0.00001: the ladder would place 0.00002 of the 0.00003 requested.
    let args = scale_args(
        Decimal::new(3, 5),
        Decimal::from(50_000),
        Decimal::from(49_000),
        2,
    );
    let err = prepare_scale(&args)
        .await
        .expect_err("size lost to the lot must be rejected");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("lot size"), "{err}");
}

#[tokio::test]
async fn scale_rejects_prices_that_collapse_onto_one_tick() {
    // 50000 and 50000.9 are both 50000 on BTC's whole-number tick grid.
    let args = scale_args(
        Decimal::new(2, 1),
        Decimal::from(50_000),
        Decimal::new(500_009, 1),
        2,
    );
    let err = prepare_scale(&args)
        .await
        .expect_err("collapsed ladder must be rejected");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("same tick"), "{err}");
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
    let plan = prepare_twap_create_plan(&btc(), &twap_args(Decimal::new(123_456_789, 9))).unwrap();
    assert_eq!(plan.size, Decimal::new(12_345, 5));
    let wire = serde_json::to_value(&plan.action).unwrap();
    assert_eq!(wire["twap"]["s"], "0.12345");
    assert_eq!(plan.duration_minutes, 5);

    let err = prepare_twap_create_plan(&btc(), &twap_args(Decimal::new(1, 8)))
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
    let margin = MarginPlan::for_market(&btc(), None).unwrap();
    let plan = tpsl_dry_run_plan(&btc(), &spec, &margin)
        .unwrap()
        .into_args();
    assert_eq!(plan["legs"][0]["size"], "0.12345");
    assert_eq!(plan["batch_order"]["orders"][1]["s"], "0.12345");
}

#[test]
fn market_precision_truncates_prices_to_the_tick_grid() {
    let perp = MarketPrecision::of_perp(&btc());
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
        MarketPrecision::of(&Market::Spot(resolver().resolve_spot("HYPE/USDC").unwrap()))
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
    let token = PerpAsset {
        name: "TOKEN".to_string(),
        index: 1,
        dex: None,
        sz_decimals: 8,
        collateral: "USDC".to_string(),
        margin_rule: MarginRule::CrossOrIsolated,
    };
    assert_eq!(
        MarketPrecision::of_perp(&token)
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
async fn a_perp_order_carries_the_margin_plan_its_preview_shows() {
    let planned = |margin_mode| {
        let mut args = base_args();
        args.margin_mode = margin_mode;
        async move { prepare(&args).await.unwrap().margin.unwrap() }
    };
    let open = planned(None).await;
    assert_eq!(open.mode(), MarginModeArg::Cross);
    assert_eq!(open.source(), crate::margin::MarginSource::MarketRule);
    let asked = planned(Some(MarginModeArg::Isolated)).await;
    assert_eq!(asked.mode(), MarginModeArg::Isolated);
    assert_eq!(asked.source(), crate::margin::MarginSource::Requested);

    let spot = resolver().resolve("HYPE/USDC").unwrap();
    let spot_order = validate_order_for_market(
        &OrderSpec::from_create(&{
            let mut args = base_args();
            args.coin = "HYPE/USDC".to_string();
            args
        })
        .unwrap(),
        &spot,
    )
    .unwrap();
    assert!(spot_order.is_none());
}

#[test]
fn a_spot_pair_is_looked_up_in_all_mids_by_its_info_coin() {
    // `allMids` lists PURR/USDC by name and every other pair as `@<index>`.
    let spot = |symbol: &str, index: usize, base: &str| {
        Market::Spot(SpotAsset {
            symbol: symbol.to_string(),
            index,
            base: base.to_string(),
            quote: "USDC".to_string(),
            base_sz_decimals: 2,
        })
    };
    let key = |market: &Market| mid_key(market, "x", None).unwrap().0;
    assert_eq!(key(&spot("PURR/USDC", 10_000, "PURR")), "PURR/USDC");
    assert_eq!(key(&spot("HYPE/USDC", 11_035, "HYPE")), "@1035");
    assert_eq!(key(&Market::Perp(btc())), "BTC");
    assert_eq!(
        mid_key(&Market::Outcome(OutcomeAsset::new("#10")), "#10", None)
            .unwrap_err()
            .exit_code(),
        13
    );
}

#[test]
fn trigger_prices_resolve_without_an_entry_only_when_every_one_is_absolute() {
    let absolute = TriggerPriceSpec::Absolute(Decimal::from(55));
    let (tp, sl) = resolve_trigger_prices(Some(&absolute), None, None, OrderSide::Buy).unwrap();
    assert_eq!((tp, sl), (Some(Decimal::from(55)), None));
    for spec in [
        TriggerPriceSpec::Entry,
        TriggerPriceSpec::Percent(Decimal::from(5)),
    ] {
        let err =
            resolve_trigger_prices(Some(&absolute), Some(&spec), None, OrderSide::Buy).unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(
            err.to_string()
                .contains("open position with an entry price"),
            "{err}"
        );
    }
    let (_, sl) = resolve_trigger_prices(
        None,
        Some(&TriggerPriceSpec::Percent(Decimal::from(-5))),
        Some(Decimal::from(100)),
        OrderSide::Buy,
    )
    .unwrap();
    assert_eq!(sl, Some(Decimal::from(95)));
}

mod trailing_stop {
    use clap::Parser;

    use super::*;
    use crate::commands::orders::args::parse_retracement_pct;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: TrailingStopArgs,
    }

    fn parse(extra: &[&str]) -> Result<TrailingStopArgs, clap::Error> {
        let mut argv = vec![
            "harness", "--coin", "BTC", "--side", "sell", "--size", "0.01",
        ];
        argv.extend_from_slice(extra);
        Harness::try_parse_from(argv).map(|harness| harness.args)
    }

    fn plan_for(args: &TrailingStopArgs) -> Result<TrailingStopPlan, CliError> {
        let perp = resolver().resolve_perp(&qualify_dex_asset(args.dex.as_deref(), &args.coin))?;
        prepare_trailing_stop_plan(&perp, args)
    }

    #[test]
    fn percent_parser_accepts_plain_and_suffixed_values() {
        assert_eq!(parse_retracement_pct("5"), Ok(Decimal::from(5)));
        assert_eq!(parse_retracement_pct("5%"), Ok(Decimal::from(5)));
        assert_eq!(
            parse_retracement_pct(" 1.234 % "),
            Ok(Decimal::new(1234, 3))
        );
        assert!(parse_retracement_pct("five").is_err());
        assert!(parse_retracement_pct("%").is_err());
    }

    #[test]
    fn percent_must_be_between_zero_and_one_hundred_exclusive() {
        for bad in ["0", "-1", "100", "250"] {
            let args = parse(&["--retracement-pct", bad]).unwrap();
            let err = validate_trailing_stop_args(&args).unwrap_err();
            assert!(matches!(err, CliError::Configuration(_)), "{bad}: {err}");
            assert_eq!(err.exit_code(), 2);
        }
        for good in ["0.1", "5", "99.99"] {
            let args = parse(&["--retracement-pct", good]).unwrap();
            validate_trailing_stop_args(&args).unwrap();
        }
    }

    #[test]
    fn price_distances_and_activation_must_be_positive() {
        let cases: [(&[&str], Decimal, &str); 4] = [
            (
                &["--retracement-px", "0"],
                Decimal::new(1, 2),
                "--retracement-px",
            ),
            (
                &["--retracement-px", "-5"],
                Decimal::new(1, 2),
                "--retracement-px",
            ),
            (
                &["--retracement-px", "5", "--activation-price", "0"],
                Decimal::new(1, 2),
                "--activation-price",
            ),
            (&["--retracement-px", "5"], Decimal::ZERO, "--size"),
        ];
        for (extra, size, flag) in cases {
            let mut args = parse(extra).unwrap();
            args.size = size;
            let err = validate_trailing_stop_args(&args).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{extra:?}");
            assert!(
                err.to_string()
                    .contains(&format!("{flag} must be greater than zero")),
                "{extra:?}: {err}"
            );
        }
    }

    #[test]
    fn plan_serializes_the_documented_wire_shape() {
        use hypersdk::hypercore::types::Action;

        let args = parse(&["--retracement-pct", "5.0", "--reduce-only"]).unwrap();
        let plan = plan_for(&args).unwrap();
        assert_eq!(
            serde_json::to_value(Action::TrailingStop(plan.action().clone())).unwrap(),
            serde_json::json!({
                "type": "trailingStop",
                "asset": 0,
                "isBuy": false,
                "sz": "0.01",
                "reduceOnly": true,
                "retracement": { "pct": "5%" },
                "activationPx": null,
            })
        );

        let args = parse(&["--retracement-px", "150", "--activation-price", "60000.50"]).unwrap();
        let plan = plan_for(&args).unwrap();
        assert_eq!(
            serde_json::to_value(Action::TrailingStop(plan.action().clone())).unwrap(),
            serde_json::json!({
                "type": "trailingStop",
                "asset": 0,
                "isBuy": false,
                "sz": "0.01",
                "reduceOnly": false,
                "retracement": { "px": "150" },
                "activationPx": "60000",
            })
        );
    }

    #[test]
    fn size_truncates_to_the_market_lot_and_rejects_zero() {
        let mut args = parse(&["--retracement-pct", "5"]).unwrap();
        args.size = Decimal::new(1_234_567, 6);
        let plan = plan_for(&args).unwrap();
        assert_eq!(plan.size(), Decimal::new(123_456, 5));

        args.size = Decimal::new(1, 8);
        let err = plan_for(&args)
            .err()
            .expect("size below the lot truncates to zero");
        assert!(err.to_string().contains("truncates to zero"), "{err}");
    }

    #[test]
    fn hip3_perps_resolve_through_dex_or_qualified_coin() {
        let mut args = parse(&["--retracement-pct", "5"]).unwrap();
        args.coin = "HYPE".to_string();
        args.dex = Some("testdex".to_string());
        let plan = plan_for(&args).unwrap();
        assert_eq!(plan.coin(), "testdex:HYPE");
        assert_eq!(plan.action().asset, 11_100);

        args.coin = "testdex:HYPE".to_string();
        args.dex = None;
        assert_eq!(plan_for(&args).unwrap().action().asset, 11_100);
    }
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
    let plan = prepare_schedule_cancel_plan(&ScheduleCancelArgs {
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
        prepare_schedule_cancel_plan(&ScheduleCancelArgs {
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
