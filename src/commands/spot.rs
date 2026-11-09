use clap::Args;
use hypersdk::Decimal;
use hypersdk::hypercore::PriceTick;
use serde::Serialize;

use crate::assets::{AssetQuery, SpotListing, eq_asset_name, load_spot_markets, parse_asset_query};
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, NoArgs, arg};
use crate::commands::rows::{ListRow, One, Row, Rows, or_missing};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;

#[derive(Args, Debug)]
pub struct SpotGetArgs {
    /// Spot pair (e.g., PURR/USDC, HYPE/USDC)
    pub pair: String,
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

impl From<SpotListing> for SpotMarketRow {
    fn from(market: SpotListing) -> Self {
        Self {
            symbol: market.symbol,
            index: market.index,
            base: market.base,
            quote: market.quote,
            base_sz_decimals: market.base_sz_decimals,
            quote_sz_decimals: market.quote_sz_decimals,
            tick_size_at_price_1: PriceTick::for_spot(market.base_sz_decimals)
                .tick_for(Decimal::from(1))
                .map(|tick| tick.to_string()),
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
    // The list already holds the market, so a hit needs no resolver. A miss
    // asks the resolver why, for its suggestion or its outcome-notation error.
    let listed = match parse_asset_query(pair) {
        AssetQuery::Spot(symbol) => load_spot_market_rows(inv.remote()?.api())
            .await?
            .into_iter()
            .find(|market| eq_asset_name(&market.symbol, &symbol)),
        _ => None,
    };
    let market = match listed {
        Some(market) => market,
        None => {
            return Err(match inv.asset_resolver().await?.resolve_spot(pair) {
                Err(err) => CliError::from(err),
                Ok(_) => CliError::AssetNotFoundNoSuggestion {
                    asset: pair.to_string(),
                },
            }
            .into());
        }
    };

    inv.print(&One::new(market));
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
        "Get details for a spot pair.",
    )
    .args(&[arg("pair")
        .kind(InputKind::MarketSymbol)
        .describe("Spot pair, for example PURR/USDC.")
        .resource_id("spot pair")]);

    async fn run(inv: &Invocation, args: &SpotGetArgs) -> Result<(), anyhow::Error> {
        get(inv, &args.pair).await
    }
}
