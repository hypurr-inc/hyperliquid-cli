use clap::Args;
use hypersdk::Decimal;
use serde::{Deserialize, Serialize};

use crate::assets::{
    AssetMetadata, AssetResolver, SpotAsset, load_spot_markets, load_spot_markets_with_contexts,
    require_spot_token,
};
use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, GroupSpec, InputKind, NoArgs, arg};
use crate::commands::common::rows::{ListRow, One, Row, Rows, or_missing};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::JsonValueOutput;
use crate::wire::InfoRequest;

#[derive(Args, Debug)]
pub struct SpotGetArgs {
    /// Spot pair (e.g., PURR/USDC, HYPE/USDC)
    pub pair: String,
}

#[derive(Args, Debug)]
pub struct SpotTokenArgs {
    /// Token name (e.g. HYPE) or its 0x-prefixed 32-hex-digit token id
    pub token: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SpotMarketRow {
    pub symbol: String,
    pub index: usize,
    pub base: String,
    pub quote: String,
    pub base_sz_decimals: i64,
    pub quote_sz_decimals: i64,
    pub tick_size_at_price_1: Option<String>,
}

impl From<SpotAsset> for SpotMarketRow {
    fn from(market: SpotAsset) -> Self {
        Self {
            tick_size_at_price_1: market.tick_size_at_price_1(),
            symbol: market.symbol,
            index: market.index,
            base: market.base,
            quote: market.quote,
            base_sz_decimals: i64::from(market.base_sz_decimals),
            quote_sz_decimals: i64::from(market.quote_sz_decimals),
        }
    }
}

pub type SpotMarketsOutput = Rows<SpotMarketRow>;

impl Row for SpotMarketRow {
    const HEADERS: &'static [&'static str] = &[
        "Symbol",
        "Index",
        "Base",
        "Quote",
        "Base Sz Decimals",
        "Quote Sz Decimals",
        "Tick Size @ Price 1",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.symbol.clone(),
            self.index.to_string(),
            self.base.clone(),
            self.quote.clone(),
            self.base_sz_decimals.to_string(),
            self.quote_sz_decimals.to_string(),
            or_missing(self.tick_size_at_price_1.as_deref()),
        ]
    }
}

impl ListRow for SpotMarketRow {
    const EMPTY: &'static str = "No spot markets.";
}

pub async fn list(inv: &Invocation) -> Result<(), anyhow::Error> {
    let markets = load_spot_market_rows(inv.remote()?.api()).await?;
    inv.print(&SpotMarketsOutput::new(markets));
    Ok(())
}

pub async fn get(inv: &Invocation, pair: &str) -> Result<(), anyhow::Error> {
    // `spotMetaAndAssetCtxs` lists the spot metadata first, so one read names
    // the market, gets a miss its suggestion or outcome-notation error, and
    // carries the market's context.
    let (spots, contexts) =
        load_spot_markets_with_contexts::<SpotAssetContext>(inv.remote()?.api()).await?;
    let spot =
        AssetResolver::new(AssetMetadata::from_assets(Vec::new(), spots)).resolve_spot(pair)?;

    let info_coin = spot.info_coin();
    let ctx = contexts.into_iter().find(|ctx| ctx.coin == info_coin);
    inv.print(&One::new(SpotMarketDetailRow::new(spot.into(), ctx)));
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SpotMarketDetailRow {
    #[serde(flatten)]
    pub market: SpotMarketRow,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub mark_price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub mid_price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub prev_day_price: Option<Decimal>,
    /// Traded notional over the last 24 hours, in the quote token.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub day_notional_volume: Option<Decimal>,
    /// Traded size over the last 24 hours, in the base token.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub day_base_volume: Option<Decimal>,
}

impl SpotMarketDetailRow {
    fn new(market: SpotMarketRow, ctx: Option<SpotAssetContext>) -> Self {
        let ctx = ctx.unwrap_or_default();
        Self {
            market,
            mark_price: ctx.mark_px,
            mid_price: ctx.mid_px,
            prev_day_price: ctx.prev_day_px,
            day_notional_volume: ctx.day_ntl_vlm,
            day_base_volume: ctx.day_base_vlm,
        }
    }
}

impl Row for SpotMarketDetailRow {
    const HEADERS: &'static [&'static str] = &[
        "Symbol",
        "Index",
        "Base",
        "Quote",
        "Base Sz Decimals",
        "Quote Sz Decimals",
        "Tick Size @ Price 1",
        "Mark Price",
        "Mid Price",
        "Prev Day Price",
        "Day Notional Volume",
        "Day Base Volume",
    ];

    fn cells(&self) -> Vec<String> {
        let mut cells = self.market.cells();
        cells.extend([
            or_missing(self.mark_price),
            or_missing(self.mid_price),
            or_missing(self.prev_day_price),
            or_missing(self.day_notional_volume),
            or_missing(self.day_base_volume),
        ]);
        cells
    }
}

/// One entry of `spotMetaAndAssetCtxs`' context list. A market with no book
/// has no mid, so every price is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpotAssetContext {
    #[serde(default)]
    coin: String,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    mark_px: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    mid_px: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    prev_day_px: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    day_ntl_vlm: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    day_base_vlm: Option<Decimal>,
}

/// The token id `tokenDetails` takes: a `0x` id as given, or the id of the
/// token called `token`.
async fn token_id(inv: &Invocation, token: &str) -> Result<String, CliError> {
    let token = token.trim();
    if let Some(hex) = token.strip_prefix("0x") {
        return if hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(token.to_ascii_lowercase())
        } else {
            Err(CliError::Configuration(
                "token id must be 0x followed by 32 hex digits".to_string(),
            ))
        };
    }
    let found = require_spot_token(inv.remote()?.api(), token).await?;
    Ok(format!("{:#x}", found.token_id))
}

async fn token(inv: &Invocation, token: &str) -> Result<(), anyhow::Error> {
    let token_id = token_id(inv, token).await?;
    let details: Option<serde_json::Value> = inv
        .remote()?
        .api()
        .info(
            &InfoRequest::new("tokenDetails").field("tokenId", token_id.clone()),
            "loading token details",
        )
        .await?;
    let details =
        details.ok_or_else(|| CliError::Unsupported(format!("no token details for {token_id}")))?;
    inv.print(&JsonValueOutput::new(details, "No token details."));
    Ok(())
}

async fn load_spot_market_rows(api: &ApiClient) -> Result<Vec<SpotMarketRow>, CliError> {
    Ok(load_spot_markets(api)
        .await?
        .into_iter()
        .map(SpotMarketRow::from)
        .collect())
}

pub const GROUP: GroupSpec = GroupSpec::new(&["spot"], "Query spot markets");

pub struct SpotList;

impl CliCommand for SpotList {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["spot", "list"],
        "market",
        "List all spot markets",
        "List spot markets.",
    )
    .cli_aliases(&["ls"]);

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        list(inv).await
    }
}

pub struct SpotGet;

impl CliCommand for SpotGet {
    type Args = SpotGetArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["spot", "get"],
        "market",
        "Get details for a specific spot pair",
        "Get details for a spot pair, with its mark, mid and previous-day price and 24-hour volume.",
    )
    .args(&[arg("pair")
        .kind(InputKind::MarketSymbol)
        .describe("Spot pair, for example PURR/USDC.")
        .resource_id("spot pair")]);

    async fn run(inv: &Invocation, args: &SpotGetArgs) -> Result<(), anyhow::Error> {
        get(inv, &args.pair).await
    }
}

pub struct SpotToken;

impl CliCommand for SpotToken {
    type Args = SpotTokenArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["spot", "token"],
        "market",
        "Show a spot token's supply, prices and genesis",
        "Show a spot token's details: supply, decimals, mid, mark and previous-day price, deployer, deploy time and genesis balances. Takes the token name or its 0x token id.",
    )
    .args(&[arg("token")
        .kind(InputKind::Identifier)
        .describe("Token name, for example HYPE, or 0x token id.")
        .resource_id("spot token")]);

    async fn run(inv: &Invocation, args: &SpotTokenArgs) -> Result<(), anyhow::Error> {
        token(inv, &args.token).await
    }
}
