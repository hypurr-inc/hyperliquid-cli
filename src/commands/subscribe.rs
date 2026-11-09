use clap::Args;
use hypersdk::hypercore::CandleInterval;

use crate::assets::{self, canonical_dex};
use crate::command_spec::{
    ArgSpec, CliCommand, CommandSpec, GroupSpec, InputKind, Lifecycle, OwsSupport, Risk, arg,
};
use crate::commands::common::market_input::{
    parse_candle_interval, resolve_market_coin, resolve_perp_coin_or_fetch,
};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::stream::SubscribeTarget;
use crate::stream_bounds::StreamBound;
use crate::stream_bounds::StreamBoundArgs;
use crate::ws::{self, BookAggregationArgs};

pub const GROUP: GroupSpec = GroupSpec::new(&["subscribe"], "Stream real-time market and account events")
    .long_about("Stream real-time market and account events over WebSocket.\n\
\n\
Automation callers should bound streams with --max-events and/or --idle-timeout-ms so commands return. Global JSON shaping flags such as --select and --max-results apply to JSONL stream lines.\n\
\n\
User streams (fills, user-fundings, twap-slice-fills) begin with an isSnapshot:true message that replays history. Pass --skip-snapshot to drop it; it is then neither written nor counted by --max-events. Without the flag the snapshot is streamed unchanged and its data carries isSnapshot.");

const MAX_EVENTS: ArgSpec = arg("max_events").describe("Stop after this many matching events.");
const IDLE_TIMEOUT_MS: ArgSpec = arg("idle_timeout_ms")
    .describe("Stop if no matching events arrive within this many milliseconds.");
const ASSET: ArgSpec = arg("asset")
    .describe("Asset name, for example BTC.")
    .resource_id("asset");
const USER: ArgSpec = arg("user")
    .kind(InputKind::PublicUserSelector)
    .describe("User address, wallet name, or wallet id. Defaults to the selected signer.")
    .resource_id("account selector");
const SKIP_SNAPSHOT: ArgSpec = arg("skip_snapshot").kind(InputKind::Boolean).describe(
    "Drop the isSnapshot:true history message the exchange sends first; it is not counted by max_events.",
);
const DEX: ArgSpec = arg("dex")
    .describe("Perp dex name (HIP-3); repeat to subscribe to several. Defaults to the first dex.");

const fn market_stream(
    path: &'static [&'static str],
    about: &'static str,
    description: &'static str,
) -> CommandSpec {
    CommandSpec::read(path, "subscribe", about, description)
        .lifecycle(Lifecycle::Streaming)
        .stream(StreamBound::Subscribe)
}

/// A stream of one account's data, read for `--user` or the selected signer.
const fn user_stream(
    path: &'static [&'static str],
    about: &'static str,
    description: &'static str,
) -> CommandSpec {
    market_stream(path, about, description)
        .risk(Risk::AccountState)
        .ows(OwsSupport::ExperimentalFeatureGated)
        .auth()
}

#[derive(Args, Debug, Clone)]
pub struct SubscribeAssetArgs {
    /// Asset name (e.g., BTC)
    #[arg(long)]
    pub asset: String,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

#[derive(Args, Debug, Clone)]
pub struct SubscribeOrderbookArgs {
    /// Asset name (e.g., BTC)
    #[arg(long)]
    pub asset: String,
    #[command(flatten)]
    pub aggregation: BookAggregationArgs,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

#[derive(Args, Debug, Clone)]
pub struct SubscribeCandlesArgs {
    /// Asset name (e.g., BTC)
    #[arg(long)]
    pub asset: String,
    /// Candle interval
    #[arg(long, default_value = "1m", value_parser = parse_candle_interval)]
    pub interval: CandleInterval,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

#[derive(Args, Debug, Clone)]
pub struct SubscribeAllMidsArgs {
    /// Perp dex name (HIP-3). Defaults to the first dex.
    #[arg(long)]
    pub dex: Option<String>,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

#[derive(Args, Debug, Clone)]
pub struct SubscribeUserArgs {
    /// User address, wallet name, or wallet id. Defaults to the selected signer.
    #[arg(long)]
    pub user: Option<String>,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

/// A user stream that begins with an `isSnapshot: true` history message.
#[derive(Args, Debug, Clone)]
pub struct SubscribeUserSnapshotArgs {
    /// User address, wallet name, or wallet id. Defaults to the selected signer.
    #[arg(long)]
    pub user: Option<String>,
    /// Drop the isSnapshot:true history message; it is not counted by --max-events
    #[arg(long)]
    pub skip_snapshot: bool,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

#[derive(Args, Debug, Clone)]
pub struct SubscribeUserDexArgs {
    /// User address, wallet name, or wallet id. Defaults to the selected signer.
    #[arg(long)]
    pub user: Option<String>,
    /// Perp dex name (HIP-3); repeat to subscribe to several. Defaults to the first dex.
    #[arg(long)]
    pub dex: Vec<String>,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

#[derive(Args, Debug, Clone)]
pub struct SubscribeUserAssetArgs {
    /// User address, wallet name, or wallet id. Defaults to the selected signer.
    #[arg(long)]
    pub user: Option<String>,
    /// Perpetual asset name (e.g., BTC)
    #[arg(long)]
    pub asset: String,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

pub struct SubscribeTrades;

impl CliCommand for SubscribeTrades {
    type Args = SubscribeAssetArgs;

    const SPEC: CommandSpec = market_stream(
        &["subscribe", "trades"],
        "Stream real-time trades",
        "Stream real-time trades. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[ASSET, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeAssetArgs) -> Result<(), anyhow::Error> {
        let coin = market_coin(inv, &args.asset).await?;
        stream(inv, SubscribeTarget::trades(coin), &args.stream).await
    }
}

pub struct SubscribeOrderbook;

impl CliCommand for SubscribeOrderbook {
    type Args = SubscribeOrderbookArgs;

    const SPEC: CommandSpec = market_stream(
        &["subscribe", "orderbook"],
        "Stream L2 order book updates",
        "Stream L2 order book updates. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[
        ASSET,
        arg("n_sig_figs").describe(
            "Aggregate price levels to this many significant figures (2-5). Omit for full precision.",
        ),
        arg("mantissa").describe("Further aggregation (1, 2 or 5); only valid with n_sig_figs 5."),
        MAX_EVENTS,
        IDLE_TIMEOUT_MS,
    ]);

    fn validate(args: &SubscribeOrderbookArgs) -> Result<(), CliError> {
        args.aggregation.validate()
    }

    async fn run(inv: &Invocation, args: &SubscribeOrderbookArgs) -> Result<(), anyhow::Error> {
        let coin = market_coin(inv, &args.asset).await?;
        stream(
            inv,
            SubscribeTarget::orderbook(coin, args.aggregation),
            &args.stream,
        )
        .await
    }
}

pub struct SubscribeBbo;

impl CliCommand for SubscribeBbo {
    type Args = SubscribeAssetArgs;

    const SPEC: CommandSpec = market_stream(
        &["subscribe", "bbo"],
        "Stream best bid and offer updates",
        "Stream best bid and offer updates, sent when the best bid or offer changes in a block. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[ASSET, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeAssetArgs) -> Result<(), anyhow::Error> {
        let coin = market_coin(inv, &args.asset).await?;
        stream(inv, SubscribeTarget::bbo(coin), &args.stream).await
    }
}

pub struct SubscribeCandles;

impl CliCommand for SubscribeCandles {
    type Args = SubscribeCandlesArgs;

    const SPEC: CommandSpec = market_stream(
        &["subscribe", "candles"],
        "Stream candle updates",
        "Stream candle updates. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[
        ASSET,
        arg("interval").describe("Candle interval such as 1m, 5m, 1h, or 1d."),
        MAX_EVENTS,
        IDLE_TIMEOUT_MS,
    ]);

    async fn run(inv: &Invocation, args: &SubscribeCandlesArgs) -> Result<(), anyhow::Error> {
        let coin = market_coin(inv, &args.asset).await?;
        stream(
            inv,
            SubscribeTarget::candles(coin, args.interval.to_string()),
            &args.stream,
        )
        .await
    }
}

pub struct SubscribeAllMids;

impl CliCommand for SubscribeAllMids {
    type Args = SubscribeAllMidsArgs;

    const SPEC: CommandSpec = market_stream(
        &["subscribe", "all-mids"],
        "Stream all mid price updates",
        "Stream all mid-price updates. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[
        arg("dex").describe("Perp dex name (HIP-3). Defaults to the first dex."),
        MAX_EVENTS,
        IDLE_TIMEOUT_MS,
    ]);

    async fn run(inv: &Invocation, args: &SubscribeAllMidsArgs) -> Result<(), anyhow::Error> {
        let dex = canonical_dex(inv.remote()?.api(), args.dex.as_deref()).await?;
        stream(inv, SubscribeTarget::all_mids(dex), &args.stream).await
    }
}

pub struct SubscribeActiveAssetCtx;

impl CliCommand for SubscribeActiveAssetCtx {
    type Args = SubscribeAssetArgs;

    const SPEC: CommandSpec = market_stream(
        &["subscribe", "active-asset-ctx"],
        "Stream an asset's mark price, funding and open interest context",
        "Stream a market's live asset context (mark price, funding, open interest for a perpetual). automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[ASSET, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeAssetArgs) -> Result<(), anyhow::Error> {
        let coin = market_coin(inv, &args.asset).await?;
        stream(inv, SubscribeTarget::active_asset_ctx(coin), &args.stream).await
    }
}

pub struct SubscribeOrderUpdates;

impl CliCommand for SubscribeOrderUpdates {
    type Args = SubscribeUserArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "order-updates"],
        "Stream order status updates (authenticated)",
        "Stream authenticated order status updates. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeUserArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        stream(inv, SubscribeTarget::order_updates(user), &args.stream).await
    }
}

pub struct SubscribeFills;

impl CliCommand for SubscribeFills {
    type Args = SubscribeUserSnapshotArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "fills"],
        "Stream fill events (authenticated)",
        "Stream authenticated fill events. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, SKIP_SNAPSHOT, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeUserSnapshotArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        stream(
            inv,
            SubscribeTarget::fills(user).skipping_snapshot(args.skip_snapshot),
            &args.stream,
        )
        .await
    }
}

pub struct SubscribeUserEvents;

impl CliCommand for SubscribeUserEvents {
    type Args = SubscribeUserArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "user-events"],
        "Stream fills, funding, liquidation and non-user-cancel events",
        "Stream a user's account events: fills, funding payments, liquidations and non-user cancels. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeUserArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        stream(inv, SubscribeTarget::user_events(user), &args.stream).await
    }
}

pub struct SubscribeUserFundings;

impl CliCommand for SubscribeUserFundings {
    type Args = SubscribeUserSnapshotArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "user-fundings"],
        "Stream a user's funding payments",
        "Stream a user's funding payments: a snapshot of recent payments, then each payment on the hour. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, SKIP_SNAPSHOT, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeUserSnapshotArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        stream(
            inv,
            SubscribeTarget::user_fundings(user).skipping_snapshot(args.skip_snapshot),
            &args.stream,
        )
        .await
    }
}

pub struct SubscribeNotifications;

impl CliCommand for SubscribeNotifications {
    type Args = SubscribeUserArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "notifications"],
        "Stream a user's notifications",
        "Stream a user's exchange notifications. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeUserArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        stream(inv, SubscribeTarget::notifications(user), &args.stream).await
    }
}

pub struct SubscribeTwapStates;

impl CliCommand for SubscribeTwapStates {
    type Args = SubscribeUserDexArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "twap-states"],
        "Stream a user's active TWAP order states",
        "Stream a user's active TWAP order states. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, DEX, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    fn validate(args: &SubscribeUserDexArgs) -> Result<(), CliError> {
        ws::ensure_subscription_count(args.dex.len())
    }

    async fn run(inv: &Invocation, args: &SubscribeUserDexArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        let dexs = canonical_dexs(inv, &args.dex).await?;
        stream(inv, SubscribeTarget::twap_states(user, dexs), &args.stream).await
    }
}

pub struct SubscribeTwapSliceFills;

impl CliCommand for SubscribeTwapSliceFills {
    type Args = SubscribeUserSnapshotArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "twap-slice-fills"],
        "Stream a user's TWAP slice fills",
        "Stream a user's TWAP slice fills: a snapshot of recent slice fills, then each new one. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, SKIP_SNAPSHOT, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeUserSnapshotArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        stream(
            inv,
            SubscribeTarget::twap_slice_fills(user).skipping_snapshot(args.skip_snapshot),
            &args.stream,
        )
        .await
    }
}

pub struct SubscribeActiveAssetData;

impl CliCommand for SubscribeActiveAssetData {
    type Args = SubscribeUserAssetArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "active-asset-data"],
        "Stream a user's leverage and trade limits on a perpetual",
        "Stream a user's leverage and trade-size limits on one perpetual (the exchange serves perpetuals only). automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[
        USER,
        arg("asset")
            .describe("Perpetual asset name, for example BTC or xyz:XYZ100.")
            .resource_id("asset"),
        MAX_EVENTS,
        IDLE_TIMEOUT_MS,
    ]);

    async fn run(inv: &Invocation, args: &SubscribeUserAssetArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        let resolver = inv.asset_resolver().await?;
        let coin = resolve_perp_coin_or_fetch(inv.remote()?.api(), &resolver, &args.asset).await?;
        stream(
            inv,
            SubscribeTarget::active_asset_data(user, coin),
            &args.stream,
        )
        .await
    }
}

pub struct SubscribeClearinghouseState;

impl CliCommand for SubscribeClearinghouseState {
    type Args = SubscribeUserDexArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "clearinghouse-state"],
        "Stream a user's perp account state",
        "Stream a user's perp clearinghouse state (margin summary and positions) per perp dex. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, DEX, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    fn validate(args: &SubscribeUserDexArgs) -> Result<(), CliError> {
        ws::ensure_subscription_count(args.dex.len())
    }

    async fn run(inv: &Invocation, args: &SubscribeUserDexArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        let dexs = canonical_dexs(inv, &args.dex).await?;
        stream(
            inv,
            SubscribeTarget::clearinghouse_state(user, dexs),
            &args.stream,
        )
        .await
    }
}

pub struct SubscribeOpenOrders;

impl CliCommand for SubscribeOpenOrders {
    type Args = SubscribeUserDexArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "open-orders"],
        "Stream a user's open orders",
        "Stream a user's open orders per perp dex. automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, DEX, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    fn validate(args: &SubscribeUserDexArgs) -> Result<(), CliError> {
        ws::ensure_subscription_count(args.dex.len())
    }

    async fn run(inv: &Invocation, args: &SubscribeUserDexArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        let dexs = canonical_dexs(inv, &args.dex).await?;
        stream(inv, SubscribeTarget::open_orders(user, dexs), &args.stream).await
    }
}

pub struct SubscribeSpotState;

impl CliCommand for SubscribeSpotState {
    type Args = SubscribeUserArgs;

    const SPEC: CommandSpec = user_stream(
        &["subscribe", "spot-state"],
        "Stream a user's spot balances",
        "Stream a user's spot state (token balances). automation callers should set max_events or idle_timeout_ms so the call returns.",
    )
    .args(&[USER, MAX_EVENTS, IDLE_TIMEOUT_MS]);

    async fn run(inv: &Invocation, args: &SubscribeUserArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.user.as_deref()).await?;
        stream(inv, SubscribeTarget::spot_state(user), &args.stream).await
    }
}

/// An asset input as the name the exchange subscribes with: dex-qualified for HIP-3.
async fn market_coin(inv: &Invocation, asset: &str) -> Result<String, anyhow::Error> {
    let resolver = inv.asset_resolver().await?;
    Ok(resolve_market_coin(inv.remote()?.api(), &resolver, asset).await?)
}

/// Each `--dex` as the exchange's canonical spelling; no flag is the first dex.
async fn canonical_dexs(
    inv: &Invocation,
    requested: &[String],
) -> Result<Vec<Option<String>>, anyhow::Error> {
    let requested: Vec<Option<&str>> = requested.iter().map(|dex| Some(dex.as_str())).collect();
    Ok(assets::canonical_dexs(inv.remote()?.api(), &requested).await?)
}

async fn stream(
    inv: &Invocation,
    target: SubscribeTarget,
    bounds: &StreamBoundArgs,
) -> Result<(), anyhow::Error> {
    crate::stream::stream_subscription(inv, inv.remote()?.chain(), target, bounds).await
}
