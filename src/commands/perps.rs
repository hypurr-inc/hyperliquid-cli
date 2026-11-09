use clap::Args;
use hypersdk::Decimal;
use hypersdk::hypercore::PerpMarket;
use serde::Serialize;

use crate::assets::{
    AssetQuery, eq_asset_name, find_perp_dex, load_hip3_market, normalize_dex, parse_asset_query,
    qualify_dex_asset,
};
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::rows::{ListRow, One, Row, Rows, or_missing};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::invocation::Invocation;

#[derive(Args, Debug)]
pub struct PerpsListArgs {
    /// Query markets from a specific HIP-3 DEX
    #[arg(long)]
    pub dex: Option<String>,
}

#[derive(Args, Debug)]
pub struct PerpsGetArgs {
    /// Coin name (e.g., BTC, ETH, SOL)
    pub coin: String,

    /// Query a market from a specific HIP-3 DEX
    #[arg(long)]
    pub dex: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PerpMarketRow {
    pub name: String,
    pub index: usize,
    pub max_leverage: u64,
    pub sz_decimals: i64,
    pub collateral: String,
    pub isolated_margin: bool,
    pub tick_size_at_price_1: Option<String>,
}

impl From<PerpMarket> for PerpMarketRow {
    fn from(market: PerpMarket) -> Self {
        let tick_size_at_price_1 = market
            .tick_for(Decimal::from(1))
            .map(|tick| tick.to_string());
        Self {
            name: market.name,
            index: market.index,
            max_leverage: market.max_leverage,
            sz_decimals: market.sz_decimals,
            collateral: market.collateral.name,
            isolated_margin: market.isolated_margin,
            tick_size_at_price_1,
        }
    }
}

pub type PerpMarketsOutput = Rows<PerpMarketRow>;

impl Row for PerpMarketRow {
    const HEADERS: &'static [&'static str] = &[
        "Name",
        "Index",
        "Max Leverage",
        "Sz Decimals",
        "Collateral",
        "Isolated",
        "Tick Size @ Price 1",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.index.to_string(),
            format!("{}x", self.max_leverage),
            self.sz_decimals.to_string(),
            self.collateral.clone(),
            self.isolated_margin.to_string(),
            or_missing(self.tick_size_at_price_1.as_deref()),
        ]
    }
}

impl ListRow for PerpMarketRow {
    const EMPTY: &'static str = "No perpetual markets.";
}

async fn list(inv: &Invocation, args: &PerpsListArgs) -> Result<(), anyhow::Error> {
    let client = inv.remote()?.api().sdk();
    let markets = if let Some(dex_name) = normalize_dex(args.dex.as_deref()) {
        let dex = find_perp_dex(client, dex_name).await?;
        client.perps_from(dex).await.map_err(from_sdk)?
    } else {
        client.perps().await.map_err(from_sdk)?
    };
    let markets = markets.into_iter().map(PerpMarketRow::from).collect();
    inv.print(&PerpMarketsOutput::new(markets));
    Ok(())
}

async fn get(inv: &Invocation, args: &PerpsGetArgs) -> Result<(), anyhow::Error> {
    let client = inv.remote()?.api().sdk();
    let query = qualify_dex_asset(args.dex.as_deref(), &args.coin);
    let market = if let AssetQuery::Hip3 {
        dex: dex_name,
        token,
    } = parse_asset_query(&query)
    {
        load_hip3_market(client, &dex_name, &token)
            .await?
            .map(|(_, market)| market)
            .ok_or_else(|| CliError::AssetNotFoundNoSuggestion {
                asset: query.clone(),
            })?
    } else {
        // The list already holds the market, so a hit needs no resolver. A
        // miss asks the resolver why, for its suggestion or its outcome-notation
        // error.
        let symbol = query.trim();
        let listed = client
            .perps()
            .await
            .map_err(from_sdk)?
            .into_iter()
            .find(|market| eq_asset_name(&market.name, symbol));
        match listed {
            Some(market) => market,
            None => {
                return Err(match inv.asset_resolver().await?.resolve_perp(&query) {
                    Err(err) => CliError::from(err),
                    Ok(_) => CliError::AssetNotFoundNoSuggestion { asset: query },
                }
                .into());
            }
        }
    };

    inv.print(&One::new(PerpMarketRow::from(market)));
    Ok(())
}

pub const GROUP: GroupSpec =
    GroupSpec::new(&["perps"], "Query perpetual markets").aliases(&["perp"]);

pub struct PerpsList;

impl CliCommand for PerpsList {
    type Args = PerpsListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["perps", "list"],
        "market",
        "List all perpetual markets",
        "List perpetual markets.",
    )
    .cli_aliases(&["ls"])
    .args(&[arg("dex")
        .kind(InputKind::VenueSelector)
        .describe("HIP-3 DEX name.")
        .resource_id("dex")]);

    async fn run(inv: &Invocation, args: &PerpsListArgs) -> Result<(), anyhow::Error> {
        list(inv, args).await
    }
}

pub struct PerpsGet;

impl CliCommand for PerpsGet {
    type Args = PerpsGetArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["perps", "get"],
        "market",
        "Get details for a specific perpetual market",
        "Get details for a perpetual market.",
    )
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin, for example BTC.")
            .resource_id("coin"),
        arg("dex")
            .kind(InputKind::VenueSelector)
            .describe("HIP-3 DEX name.")
            .resource_id("dex"),
    ]);

    async fn run(inv: &Invocation, args: &PerpsGetArgs) -> Result<(), anyhow::Error> {
        get(inv, args).await
    }
}
