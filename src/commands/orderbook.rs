use clap::Args;
use std::time::Duration;

use hypersdk::Decimal;
use hypersdk::hypercore::{
    AssetContext, BookLevel, Candle, CandleInterval, Chain, FundingRate, Incoming, L2Book,
    Subscription,
};
use serde::Serialize;

use crate::assets::DexArg;
use crate::cli_command::CliCommand;
use crate::clock;
use crate::command_spec::{CommandSpec, InputKind, arg};
use crate::commands::common::market_input::{market_coin, parse_candle_interval, perp_coin};
use crate::commands::common::rows::{ListRow, One, Row, Rows, or_missing};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::TableData;
use crate::output::format::epoch_ms_to_iso;
use crate::stream_bounds::SnapshotWatchArgs;
use crate::stream_bounds::StreamBound;
use crate::ws::{Awaited, BookAggregationArgs, first_matching_message};

const WEBSOCKET_TIMEOUT: Duration = Duration::from_secs(15);
const FUNDING_LOOKBACK: Duration = clock::DAY.of(1);

pub fn parse_candle_limit(input: &str) -> Result<usize, String> {
    let limit = input
        .parse::<usize>()
        .map_err(|_| format!("invalid limit '{input}'. Limit must be an integer from 1 to 5000"))?;

    if (1..=5000).contains(&limit) {
        Ok(limit)
    } else {
        Err(format!(
            "invalid limit '{input}'. Limit must be an integer from 1 to 5000"
        ))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BookLevelRow {
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub depth: Decimal,
    pub orders: usize,
}

impl BookLevelRow {
    fn cells(&self, side: &str) -> Vec<String> {
        vec![
            side.to_string(),
            self.price.to_string(),
            self.size.to_string(),
            self.depth.to_string(),
            self.orders.to_string(),
        ]
    }

    fn from_levels(levels: &[BookLevel]) -> Vec<Self> {
        let mut depth = Decimal::ZERO;
        levels
            .iter()
            .map(|level| {
                depth += level.sz;
                Self {
                    price: level.px,
                    size: level.sz,
                    depth,
                    orders: level.n,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BookSnapshot {
    pub coin: String,
    pub time: u64,
    pub bids: Vec<BookLevelRow>,
    pub asks: Vec<BookLevelRow>,
}

impl From<L2Book> for BookSnapshot {
    fn from(book: L2Book) -> Self {
        Self {
            coin: book.coin.clone(),
            time: book.time,
            bids: BookLevelRow::from_levels(book.bids()),
            asks: BookLevelRow::from_levels(book.asks()),
        }
    }
}

impl TableData for BookSnapshot {
    fn headers(&self) -> Vec<&str> {
        vec!["Side", "Price", "Size", "Depth", "Orders"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.bids
            .iter()
            .map(|level| level.cells("Bid"))
            .chain(self.asks.iter().map(|level| level.cells("Ask")))
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        crate::output::serialize_row(self)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CandleRow {
    pub timestamp: u64,
    pub close_time: u64,
    pub coin: String,
    pub interval: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub open: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub high: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub low: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub close: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub volume: Decimal,
    pub num_trades: u64,
}

impl From<Candle> for CandleRow {
    fn from(candle: Candle) -> Self {
        Self {
            timestamp: candle.open_time,
            close_time: candle.close_time,
            coin: candle.coin,
            interval: candle.interval,
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
            volume: candle.volume,
            num_trades: candle.num_trades,
        }
    }
}

impl Row for CandleRow {
    const HEADERS: &'static [&'static str] = &[
        "Timestamp",
        "Coin",
        "Interval",
        "Open",
        "High",
        "Low",
        "Close",
        "Volume",
        "Trades",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            epoch_ms_to_iso(self.timestamp),
            self.coin.clone(),
            self.interval.clone(),
            self.open.to_string(),
            self.high.to_string(),
            self.low.to_string(),
            self.close.to_string(),
            self.volume.to_string(),
            self.num_trades.to_string(),
        ]
    }
}

impl ListRow for CandleRow {
    const EMPTY: &'static str = "No candles.";
}

pub type CandlesOutput = Rows<CandleRow>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SpreadRow {
    pub coin: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub bid: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub ask: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub spread: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub spread_pct: Decimal,
}

impl Row for SpreadRow {
    const HEADERS: &'static [&'static str] = &["Coin", "Bid", "Ask", "Spread", "Spread %"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.bid.to_string(),
            self.ask.to_string(),
            self.spread.to_string(),
            self.spread_pct.to_string(),
        ]
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FundingRow {
    pub coin: String,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub current_funding_rate: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub predicted_funding_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub premium: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub mark_price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub oracle_price: Option<Decimal>,
    pub last_funding_time: Option<u64>,
}

impl Row for FundingRow {
    const HEADERS: &'static [&'static str] = &[
        "Coin",
        "Current Funding",
        "Predicted Funding",
        "Premium",
        "Mark",
        "Oracle",
        "Last Funding Time",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            or_missing(self.current_funding_rate),
            self.predicted_funding_rate.to_string(),
            or_missing(self.premium),
            or_missing(self.mark_price),
            or_missing(self.oracle_price),
            or_missing(self.last_funding_time.map(epoch_ms_to_iso)),
        ]
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MidRow {
    pub name: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub mid: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidsOutput {
    mids: Vec<MidRow>,
    selected_fields: Option<Vec<String>>,
}

impl MidsOutput {
    #[must_use]
    pub fn new(mids: Vec<MidRow>, selected_fields: Option<&[String]>) -> Self {
        Self {
            mids,
            selected_fields: selected_fields.map(<[String]>::to_vec),
        }
    }
}

impl TableData for MidsOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Name", "Mid"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.mids
            .iter()
            .map(|mid| vec![mid.name.clone(), mid.mid.to_string()])
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        if let Some(fields) = &self.selected_fields {
            let rows = self
                .mids
                .iter()
                .map(|mid| {
                    let mut object = serde_json::Map::new();
                    for field in fields {
                        match field.as_str() {
                            "name" | "coin" => {
                                object.insert(field.clone(), serde_json::json!(mid.name));
                            }
                            "mid" | "price" => {
                                object.insert(field.clone(), serde_json::json!(mid.mid));
                            }
                            _ => {}
                        }
                    }
                    serde_json::Value::Object(object)
                })
                .collect::<Vec<_>>();

            serde_json::Value::Array(rows)
        } else {
            let mut object = serde_json::Map::new();
            for mid in &self.mids {
                object.insert(mid.name.clone(), serde_json::json!(mid.mid));
            }
            serde_json::Value::Object(object)
        }
    }
}

/// A HIP-3 `dex:TOKEN` input is looked up on its dex, so `xyz:XYZ100` works
/// without first loading every dex into the metadata snapshot.
async fn book(
    inv: &Invocation,
    coin: &str,
    aggregation: BookAggregationArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let coin = market_coin(inv, coin).await?;
    inv.print(&book_snapshot_for(remote.chain(), &coin, aggregation).await?);
    Ok(())
}

async fn book_snapshot_for(
    chain: Chain,
    coin: &str,
    aggregation: BookAggregationArgs,
) -> Result<BookSnapshot, anyhow::Error> {
    let book = fetch_l2_book(chain, coin, aggregation).await?;
    Ok(BookSnapshot::from(book))
}

async fn candles(
    inv: &Invocation,
    coin: &str,
    interval: CandleInterval,
    limit: usize,
) -> Result<(), anyhow::Error> {
    let client = inv.remote()?.api();
    let coin = market_coin(inv, coin).await?;
    inv.print(&candles_snapshot_for(client, &coin, interval, limit).await?);
    Ok(())
}

async fn candles_snapshot_for(
    client: &ApiClient,
    coin: &str,
    interval: CandleInterval,
    limit: usize,
) -> Result<CandlesOutput, anyhow::Error> {
    let coin = coin.to_string();
    let end_time = clock::now_ms();
    let interval_ms = interval.to_duration().as_millis() as u64;
    let lookback = interval_ms.saturating_mul(limit.saturating_add(2) as u64);
    let start_time = end_time.saturating_sub(lookback);

    let mut candles = client
        .sdk()
        .candle_snapshot(coin, interval, start_time, end_time)
        .await
        .map_err(from_sdk)?;

    candles.sort_by_key(|candle| candle.open_time);
    let skip = candles.len().saturating_sub(limit);
    let rows = candles
        .into_iter()
        .skip(skip)
        .map(CandleRow::from)
        .collect::<Vec<_>>();

    Ok(CandlesOutput::new(rows))
}

async fn spread(inv: &Invocation, coin: &str) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let coin = market_coin(inv, coin).await?;
    let book = fetch_l2_book(remote.chain(), &coin, BookAggregationArgs::default()).await?;
    inv.print(&One::new(spread_row_from_book(&coin, &book)?));
    Ok(())
}

async fn funding(inv: &Invocation, coin: &str) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let coin = perp_coin(inv, coin).await?;
    let end_time = clock::now_ms();
    let start_time = end_time.saturating_sub(FUNDING_LOOKBACK.as_millis() as u64);

    let latest_history = remote
        .api()
        .sdk()
        .funding_history(coin.clone(), start_time, Some(end_time))
        .await
        .map_err(from_sdk)?
        .into_iter()
        .max_by_key(|rate| rate.time);

    let active_ctx = fetch_active_asset_ctx(remote.chain(), &coin).await?;
    inv.print(&One::new(FundingRow::from_history_and_context(
        &coin,
        latest_history,
        active_ctx,
    )));
    Ok(())
}

async fn mids(inv: &Invocation, dex: Option<&DexArg>) -> Result<(), anyhow::Error> {
    let selected = inv.session().json_options().selected_fields();
    let client = inv.remote()?.api();
    let dex = inv.dex(dex).await?.map(|dex| dex.name.clone());
    inv.print(&mids_snapshot(client, dex.as_deref(), selected).await?);
    Ok(())
}

/// `selected` is `--select`: it switches the JSON from a name-to-mid map to rows of the
/// named fields.
async fn mids_snapshot(
    client: &ApiClient,
    dex: Option<&str>,
    selected: Option<&[String]>,
) -> Result<MidsOutput, anyhow::Error> {
    let mut mids = client
        .sdk()
        .all_mids(dex.map(str::to_string))
        .await
        .map_err(from_sdk)?
        .into_iter()
        .map(|(name, mid)| MidRow { name, mid })
        .collect::<Vec<_>>();

    mids.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(MidsOutput::new(mids, selected))
}

impl FundingRow {
    fn from_history_and_context(
        coin: &str,
        latest_history: Option<FundingRate>,
        ctx: AssetContext,
    ) -> Self {
        Self {
            coin: coin.to_string(),
            current_funding_rate: latest_history.as_ref().map(|rate| rate.funding_rate),
            predicted_funding_rate: ctx.funding,
            premium: ctx.premium,
            mark_price: ctx.mark_px,
            oracle_price: ctx.oracle_px,
            last_funding_time: latest_history.map(|rate| rate.time),
        }
    }
}

fn spread_row_from_book(coin: &str, book: &L2Book) -> Result<SpreadRow, CliError> {
    let best_bid = book.best_bid().ok_or_else(|| {
        CliError::Unavailable("Order book did not contain any bid levels.".to_string())
    })?;
    let best_ask = book.best_ask().ok_or_else(|| {
        CliError::Unavailable("Order book did not contain any ask levels.".to_string())
    })?;
    let spread = best_ask.px - best_bid.px;
    let spread_pct = if best_bid.px.is_zero() {
        Decimal::ZERO
    } else {
        (spread / best_bid.px * Decimal::from(100)).round_dp(6)
    };
    Ok(SpreadRow {
        coin: coin.to_string(),
        bid: best_bid.px,
        ask: best_ask.px,
        spread,
        spread_pct,
    })
}

async fn fetch_l2_book(
    chain: Chain,
    coin: &str,
    aggregation: BookAggregationArgs,
) -> Result<L2Book, CliError> {
    first_matching_message(
        chain,
        aggregation.subscription(coin),
        WEBSOCKET_TIMEOUT,
        &Awaited {
            closed: "an order book snapshot",
            timed_out: "order book snapshot",
        },
        |message| match message {
            Incoming::L2Book(book) if book.coin.eq_ignore_ascii_case(coin) => Some(book),
            _ => None,
        },
    )
    .await
}

async fn fetch_active_asset_ctx(chain: Chain, coin: &str) -> Result<AssetContext, CliError> {
    first_matching_message(
        chain,
        Subscription::ActiveAssetCtx {
            coin: coin.to_string(),
        },
        WEBSOCKET_TIMEOUT,
        &Awaited {
            closed: "funding context",
            timed_out: "funding context",
        },
        |message| match message {
            Incoming::ActiveAssetCtx {
                coin: event_coin,
                ctx,
            } if event_coin.eq_ignore_ascii_case(coin) => Some(ctx),
            _ => None,
        },
    )
    .await
}

#[derive(Args, Debug, Clone)]
pub struct CoinArgs {
    /// Asset name (e.g., BTC)
    pub coin: String,
}

#[derive(Args, Debug, Clone)]
pub struct BookArgs {
    /// Asset name (e.g., BTC)
    pub coin: String,
    #[command(flatten)]
    pub aggregation: BookAggregationArgs,
    #[command(flatten)]
    pub watch: SnapshotWatchArgs,
}

#[derive(Args, Debug, Clone)]
pub struct MidsArgs {
    #[command(flatten)]
    pub watch: SnapshotWatchArgs,
    /// Perp dex name (HIP-3). Defaults to the first dex.
    #[arg(long)]
    pub dex: Option<DexArg>,
}

#[derive(Args, Debug, Clone)]
pub struct CandlesArgs {
    /// Asset name (e.g., BTC)
    pub coin: String,
    /// Candle interval (1m, 3m, 5m, 15m, 30m, 1h, 2h, 4h, 8h, 12h, 1d, 3d, 1w, 1M)
    #[arg(
        long,
        default_value = "1h",
        value_parser = parse_candle_interval
    )]
    pub interval: hypersdk::hypercore::CandleInterval,
    /// Number of candles to return
    #[arg(
        long,
        default_value = "100",
        value_parser = parse_candle_limit
    )]
    pub limit: usize,
    #[command(flatten)]
    pub watch: SnapshotWatchArgs,
}

pub struct Book;

impl CliCommand for Book {
    type Args = BookArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["book"],
        "market",
        "Show the L2 order book",
        "Show an L2 order book snapshot.",
    )
    .stream(StreamBound::SnapshotWatch)
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Asset name, for example BTC.")
            .resource_id("coin"),
        arg("n_sig_figs")
            .describe("Aggregate price levels to this many significant figures (2-5). Omit for full precision."),
        arg("mantissa")
            .describe("Further aggregation (1, 2 or 5); only valid with n_sig_figs 5."),
        arg("watch")
            .kind(InputKind::Boolean)
            .describe("Watch live snapshots. Use max_ticks to bound automation calls."),
        arg("max_ticks").describe("Stop after this many watch snapshots."),
    ]);

    fn validate(args: &BookArgs) -> Result<(), CliError> {
        args.aggregation.validate()
    }

    async fn run(inv: &Invocation, args: &BookArgs) -> Result<(), anyhow::Error> {
        if args.watch.watch {
            watch_book(inv, &args.coin, args.aggregation, args.watch.max_ticks).await
        } else {
            book(inv, &args.coin, args.aggregation).await
        }
    }
}

pub struct Mids;

impl CliCommand for Mids {
    type Args = MidsArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["mids"],
        "market",
        "Show mid prices",
        "Show all mid prices.",
    )
    .stream(StreamBound::SnapshotWatch)
    .args(&[
        arg("watch")
            .kind(InputKind::Boolean)
            .describe("Watch live snapshots. Use max_ticks to bound automation calls."),
        arg("max_ticks").describe("Stop after this many watch snapshots."),
        arg("dex").describe("Perp dex name (HIP-3). Defaults to the first dex."),
    ]);

    async fn run(inv: &Invocation, args: &MidsArgs) -> Result<(), anyhow::Error> {
        if args.watch.watch {
            watch_mids(inv, args.dex.as_ref(), args.watch.max_ticks).await
        } else {
            mids(inv, args.dex.as_ref()).await
        }
    }
}

pub struct Candles;

impl CliCommand for Candles {
    type Args = CandlesArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["candles"],
        "market",
        "Show candle history",
        "Show candle history.",
    )
    .stream(StreamBound::SnapshotWatch)
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Asset name, for example BTC.")
            .resource_id("coin"),
        arg("interval").describe("Candle interval such as 1m, 5m, 1h, or 1d."),
        arg("limit"),
        arg("watch")
            .kind(InputKind::Boolean)
            .describe("Watch live snapshots. Use max_ticks to bound automation calls."),
        arg("max_ticks").describe("Stop after this many watch snapshots."),
    ]);

    async fn run(inv: &Invocation, args: &CandlesArgs) -> Result<(), anyhow::Error> {
        if args.watch.watch {
            watch_candles(
                inv,
                &args.coin,
                args.interval,
                args.limit,
                args.watch.max_ticks,
            )
            .await
        } else {
            candles(inv, &args.coin, args.interval, args.limit).await
        }
    }
}

pub struct Spread;

impl CliCommand for Spread {
    type Args = CoinArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["spread"],
        "market",
        "Show the bid-ask spread",
        "Show bid, ask, and spread.",
    )
    .args(&[arg("coin")
        .kind(InputKind::MarketSymbol)
        .describe("Asset name, for example BTC.")
        .resource_id("coin")]);

    async fn run(inv: &Invocation, args: &CoinArgs) -> Result<(), anyhow::Error> {
        spread(inv, &args.coin).await
    }
}

pub struct Funding;

impl CliCommand for Funding {
    type Args = CoinArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["funding"],
        "market",
        "Show the current funding rate",
        "Show current funding rate.",
    )
    .args(&[arg("coin")
        .kind(InputKind::MarketSymbol)
        .describe("Asset name, for example BTC.")
        .resource_id("coin")]);

    async fn run(inv: &Invocation, args: &CoinArgs) -> Result<(), anyhow::Error> {
        funding(inv, &args.coin).await
    }
}

async fn watch_book(
    inv: &Invocation,
    coin: &str,
    aggregation: BookAggregationArgs,
    max_ticks: Option<usize>,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let chain = remote.chain();
    let subscription_coin = market_coin(inv, coin).await?;

    crate::watch::run_snapshot_watch(
        inv,
        &format!("Live order book: {subscription_coin}"),
        chain,
        vec![aggregation.subscription(subscription_coin.clone())],
        max_ticks,
        move || {
            let coin = subscription_coin.clone();
            async move { book_snapshot_for(chain, &coin, aggregation).await }
        },
    )
    .await
}

async fn watch_mids(
    inv: &Invocation,
    dex: Option<&DexArg>,
    max_ticks: Option<usize>,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let client = remote.api();
    let selected = inv.session().json_options().selected_fields();
    let dex = inv.dex(dex).await?.map(|dex| dex.name.clone());

    crate::watch::run_snapshot_watch(
        inv,
        "Live mid prices",
        remote.chain(),
        vec![Subscription::AllMids { dex: dex.clone() }],
        max_ticks,
        move || {
            let dex = dex.clone();
            async move { mids_snapshot(client, dex.as_deref(), selected).await }
        },
    )
    .await
}

async fn watch_candles(
    inv: &Invocation,
    coin: &str,
    interval: hypersdk::hypercore::CandleInterval,
    limit: usize,
    max_ticks: Option<usize>,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let client = remote.api();
    let subscription_coin = market_coin(inv, coin).await?;

    crate::watch::run_snapshot_watch(
        inv,
        &format!("Live candles: {subscription_coin} {interval}"),
        remote.chain(),
        vec![Subscription::Candle {
            coin: subscription_coin.clone(),
            interval: interval.to_string(),
        }],
        max_ticks,
        move || {
            let coin = subscription_coin.clone();
            async move { candles_snapshot_for(client, &coin, interval, limit).await }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(input: &str) -> Decimal {
        Decimal::from_str(input).unwrap()
    }

    #[test]
    fn candle_limit_accepts_only_integers_from_1_to_5000() {
        for accepted in ["1", "10", "5000"] {
            assert_eq!(parse_candle_limit(accepted), Ok(accepted.parse().unwrap()));
        }
        for rejected in ["0", "5001", "-1", "1.5", "ten", ""] {
            assert!(parse_candle_limit(rejected).is_err(), "{rejected:?}");
        }
    }

    #[test]
    fn book_level_rows_include_cumulative_depth() {
        let levels = vec![
            BookLevel {
                px: dec("100"),
                sz: dec("1"),
                n: 1,
            },
            BookLevel {
                px: dec("99"),
                sz: dec("2.5"),
                n: 4,
            },
        ];

        let rows = BookLevelRow::from_levels(&levels);

        assert_eq!(rows[0].depth, dec("1"));
        assert_eq!(rows[1].depth, dec("3.5"));
    }

    #[test]
    fn funding_row_renders_missing_market_data_as_null_in_json_and_na_in_tables() {
        let complete = FundingRow {
            coin: "BTC".to_string(),
            current_funding_rate: Some(dec("0.0001")),
            predicted_funding_rate: dec("0.0002"),
            premium: Some(dec("0.00005")),
            mark_price: Some(dec("100")),
            oracle_price: Some(dec("99")),
            last_funding_time: Some(123),
        };
        let complete_output = One::new(complete.clone());
        let complete_row = complete_output.rows().remove(0);
        assert_eq!(complete_row[1], "0.0001");
        assert_eq!(complete_row[6], "1970-01-01T00:00:00.123Z");
        assert_eq!(complete_output.to_json_value()["last_funding_time"], 123);

        let sparse = FundingRow {
            current_funding_rate: None,
            premium: None,
            mark_price: None,
            oracle_price: None,
            last_funding_time: None,
            ..complete
        };
        let sparse = One::new(sparse);
        let sparse_row = sparse.rows().remove(0);
        assert_eq!(sparse_row[1], "n/a");
        assert_eq!(sparse_row[6], "n/a");
        let json = sparse.to_json_value();
        assert!(json["current_funding_rate"].is_null());
        assert!(json["last_funding_time"].is_null());
        assert_eq!(json["predicted_funding_rate"], "0.0002");
    }

    #[test]
    fn mids_json_defaults_to_object_mapping_names_to_prices() {
        let output = MidsOutput::new(
            vec![
                MidRow {
                    name: "BTC".to_string(),
                    mid: dec("100"),
                },
                MidRow {
                    name: "ETH".to_string(),
                    mid: dec("50"),
                },
            ],
            None,
        );
        let json = output.to_json_value();

        assert!(json.is_object());
        assert_eq!(json["BTC"], "100");
        assert_eq!(json["ETH"], "50");
    }

    #[test]
    fn mids_select_filters_json_to_requested_fields() {
        let output = MidsOutput::new(
            vec![MidRow {
                name: "BTC".to_string(),
                mid: dec("100"),
            }],
            Some(&["name".to_string(), "mid".to_string()]),
        );
        let json = output.to_json_value();
        let object = json[0].as_object().unwrap();

        assert_eq!(object.len(), 2);
        assert_eq!(object["name"], "BTC");
        assert_eq!(object["mid"], "100");
    }
}
