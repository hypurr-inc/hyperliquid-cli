use clap::Args;
use hypersdk::Decimal;
use serde::{Deserialize, Serialize};

use crate::assets::{
    AssetMetadata, AssetQuery, AssetResolver, CoinDecimal, DeployerFeeScales, PerpDexList,
    PerpListing, canonical_dex, eq_asset_name, load_hip3_market, load_perp_listings, normalize_dex,
    parse_asset_query, perp_price_tick, qualify_dex_asset,
};
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::common::paging;
use crate::commands::common::rows::{ListRow, One, Row, Rows, or_missing};
use crate::commands::common::time_range::{parse_time_millis, validate_optional_time_range};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::wire::InfoRequest;

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
    pub margin_mode: &'static str,
    pub tick_size_at_price_1: Option<String>,
    /// The exchange no longer lists the market for trading.
    pub delisted: bool,
    /// Only HIP-3 markets state one.
    #[serde(
        with = "rust_decimal::serde::str_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub deployer_fee_scale: Option<Decimal>,
}

impl TryFrom<PerpListing> for PerpMarketRow {
    type Error = CliError;

    fn try_from(listing: PerpListing) -> Result<Self, CliError> {
        let PerpListing {
            market,
            index,
            collateral,
            ..
        } = listing;
        let max_leverage = market.max_leverage.ok_or_else(|| {
            CliError::Internal(anyhow::anyhow!(
                "exchange metadata for {} has no maxLeverage",
                market.name
            ))
        })?;
        let tick_size_at_price_1 = perp_price_tick(i64::from(market.sz_decimals))
            .tick_for(Decimal::from(1))
            .map(|tick| tick.to_string());
        Ok(Self {
            margin_mode: market.margin_rule.as_str(),
            name: market.name,
            index,
            max_leverage,
            sz_decimals: i64::from(market.sz_decimals),
            collateral,
            isolated_margin: market.only_isolated,
            tick_size_at_price_1,
            delisted: market.delisted,
            deployer_fee_scale: market.deployer_fee_scale,
        })
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
        "Margin Mode",
        "Tick Size @ Price 1",
        "Delisted",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.index.to_string(),
            format!("{}x", self.max_leverage),
            self.sz_decimals.to_string(),
            self.collateral.clone(),
            self.isolated_margin.to_string(),
            self.margin_mode.to_string(),
            or_missing(self.tick_size_at_price_1.as_deref()),
            self.delisted.to_string(),
        ]
    }
}

impl ListRow for PerpMarketRow {
    const EMPTY: &'static str = "No perpetual markets.";
}

async fn list(inv: &Invocation, args: &PerpsListArgs) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let listings = if let Some(dex_name) = normalize_dex(args.dex.as_deref()) {
        let dexes = PerpDexList::load(api).await?;
        load_perp_listings(api, Some(dexes.canonical(dex_name)?)).await?
    } else {
        load_perp_listings(api, None).await?
    };
    let markets = listings
        .into_iter()
        .map(PerpMarketRow::try_from)
        .collect::<Result<_, _>>()?;
    inv.print(&PerpMarketsOutput::new(markets));
    Ok(())
}

async fn get(inv: &Invocation, args: &PerpsGetArgs) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let query = qualify_dex_asset(args.dex.as_deref(), &args.coin);
    let not_found = || CliError::AssetNotFoundNoSuggestion {
        asset: query.clone(),
    };
    let listing = if let AssetQuery::Hip3 {
        dex: dex_name,
        token,
    } = parse_asset_query(&query)
    {
        let dexes = PerpDexList::load(api).await?;
        load_hip3_market(api, dexes.canonical(&dex_name)?, &token)
            .await?
            .ok_or_else(not_found)?
    } else {
        // The resolver is built from the list the row is read from, so a miss
        // gets its suggestion or outcome-notation error without a second read.
        let listings = load_perp_listings(api, None).await?;
        let assets = listings.iter().cloned().map(PerpListing::into_asset);
        let resolver = AssetResolver::new(AssetMetadata::from_assets(assets.collect(), Vec::new()));
        let perp = resolver.resolve_perp(&query).map_err(CliError::from)?;
        listings
            .into_iter()
            .find(|listing| listing.index == perp.index)
            .ok_or_else(not_found)?
    };

    inv.print(&One::new(PerpMarketRow::try_from(listing)?));
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
        "List perpetual markets, delisted ones included (delisted flags them; nothing is hidden). margin_mode is cross_or_isolated, isolated_only (legacy onlyIsolated flag), no_cross (isolated only, margin removable) or strict_isolated (isolated only, margin not removable).",
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
        "Get details for a perpetual market. margin_mode is cross_or_isolated, isolated_only (legacy onlyIsolated flag), no_cross (isolated only, margin removable) or strict_isolated (isolated only, margin not removable).",
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

#[derive(Args, Debug)]
pub struct FundingHistoryArgs {
    /// Perpetual coin, for example BTC or xyz:XYZ100
    pub coin: String,
    /// Start time as RFC3339 or epoch milliseconds.
    #[arg(long, value_parser = parse_time_millis)]
    pub start: u64,
    /// End time as RFC3339 or epoch milliseconds.
    #[arg(long, value_parser = parse_time_millis)]
    pub end: Option<u64>,
    /// Keep reading until the window is exhausted, instead of stopping at the API's 500-rate page.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct FundingPredictedArgs {
    /// Only this coin, for example BTC
    #[arg(long)]
    pub coin: Option<String>,
}

#[derive(Args, Debug)]
pub struct AtOiCapArgs {
    /// Query a specific HIP-3 DEX. Defaults to the first dex.
    #[arg(long)]
    pub dex: Option<String>,
}

#[derive(Args, Debug)]
pub struct DexLimitsArgs {
    /// HIP-3 DEX name (the first dex has no limits to read)
    pub dex: String,
}

/// One funding-rate record of `fundingHistory`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub struct FundingRateRow {
    pub coin: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub premium: Decimal,
    pub time: u64,
}

impl Row for FundingRateRow {
    const HEADERS: &'static [&'static str] = &["Coin", "Funding Rate", "Premium", "Time"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.funding_rate.to_string(),
            self.premium.to_string(),
            self.time.to_string(),
        ]
    }
}

impl ListRow for FundingRateRow {
    const EMPTY: &'static str = "No funding history.";
}

/// A funding rate one venue predicts for a coin.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PredictedFundingRow {
    pub coin: String,
    pub venue: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding_rate: Decimal,
    pub next_funding_time: u64,
    /// Only venues that state their funding interval carry one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_interval_hours: Option<u64>,
}

impl Row for PredictedFundingRow {
    const HEADERS: &'static [&'static str] = &[
        "Coin",
        "Venue",
        "Funding Rate",
        "Next Funding Time",
        "Interval Hours",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.venue.clone(),
            self.funding_rate.to_string(),
            self.next_funding_time.to_string(),
            or_missing(self.funding_interval_hours),
        ]
    }
}

impl ListRow for PredictedFundingRow {
    const EMPTY: &'static str = "No predicted fundings.";
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VenueFunding {
    #[serde(with = "rust_decimal::serde::str")]
    funding_rate: Decimal,
    next_funding_time: u64,
    #[serde(default)]
    funding_interval_hours: Option<u64>,
}

/// `[coin, [[venue, funding | null], ...]]`, as `predictedFundings` sends it.
type PredictedFundings = Vec<(String, Vec<(String, Option<VenueFunding>)>)>;

fn predicted_rows(raw: PredictedFundings, only_coin: Option<&str>) -> Vec<PredictedFundingRow> {
    raw.into_iter()
        .filter(|(coin, _)| only_coin.is_none_or(|wanted| eq_asset_name(coin, wanted)))
        .flat_map(|(coin, venues)| {
            venues.into_iter().filter_map(move |(venue, funding)| {
                let funding = funding?;
                Some(PredictedFundingRow {
                    coin: coin.clone(),
                    venue,
                    funding_rate: funding.funding_rate,
                    next_funding_time: funding.next_funding_time,
                    funding_interval_hours: funding.funding_interval_hours,
                })
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CoinRow {
    pub coin: String,
}

impl Row for CoinRow {
    const HEADERS: &'static [&'static str] = &["Coin"];

    fn cells(&self) -> Vec<String> {
        vec![self.coin.clone()]
    }
}

impl ListRow for CoinRow {
    const EMPTY: &'static str = "No perpetuals are at their open interest cap.";
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OiCapRow {
    pub coin: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub oi_cap: Decimal,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FundingMultiplierRow {
    pub coin: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub multiplier: Decimal,
}

/// A builder-deployed perp dex as `perpDexs` lists it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DexInfoRow {
    pub name: String,
    /// The dex's slot in the exchange's dex list; it fixes the dex's asset ids.
    pub index: usize,
    pub full_name: Option<String>,
    pub deployer: Option<String>,
    pub oracle_updater: Option<String>,
    pub fee_recipient: Option<String>,
    #[serde(flatten)]
    pub fees: DeployerFeeScales,
    pub oi_caps: Vec<OiCapRow>,
    pub funding_multipliers: Vec<FundingMultiplierRow>,
}

impl Row for DexInfoRow {
    const HEADERS: &'static [&'static str] = &[
        "Name",
        "Index",
        "Full Name",
        "Deployer",
        "Fee Recipient",
        "Fee Scale",
        "OI Caps",
        "Funding Multipliers",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.index.to_string(),
            or_missing(self.full_name.as_deref()),
            or_missing(self.deployer.as_deref()),
            or_missing(self.fee_recipient.as_deref()),
            or_missing(self.fees.uniform),
            self.oi_caps.len().to_string(),
            self.funding_multipliers.len().to_string(),
        ]
    }
}

impl ListRow for DexInfoRow {
    const EMPTY: &'static str = "No builder-deployed perp dexes.";
}

/// A dex's open-interest limits and net deposits.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DexLimitsRow {
    pub dex: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_oi_cap: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub oi_sz_cap_per_perp: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_transfer_ntl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_net_deposit: Decimal,
    pub oi_caps: Vec<OiCapRow>,
}

impl Row for DexLimitsRow {
    const HEADERS: &'static [&'static str] = &[
        "Dex",
        "Total OI Cap",
        "OI Size Cap / Perp",
        "Max Transfer Notional",
        "Total Net Deposit",
        "Capped Markets",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.dex.clone(),
            self.total_oi_cap.to_string(),
            self.oi_sz_cap_per_perp.to_string(),
            self.max_transfer_ntl.to_string(),
            self.total_net_deposit.to_string(),
            self.oi_caps.len().to_string(),
        ]
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDexLimits {
    #[serde(with = "rust_decimal::serde::str")]
    total_oi_cap: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    oi_sz_cap_per_perp: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    max_transfer_ntl: Decimal,
    #[serde(default)]
    coin_to_oi_cap: Option<Vec<CoinDecimal>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDexStatus {
    #[serde(with = "rust_decimal::serde::str")]
    total_net_deposit: Decimal,
}

fn oi_caps(pairs: &[CoinDecimal]) -> Vec<OiCapRow> {
    pairs
        .iter()
        .map(|CoinDecimal(coin, oi_cap)| OiCapRow {
            coin: coin.clone(),
            oi_cap: *oi_cap,
        })
        .collect()
}

async fn funding_history(inv: &Invocation, args: &FundingHistoryArgs) -> Result<(), anyhow::Error> {
    validate_optional_time_range(Some(args.start), args.end)?;
    let api = inv.remote()?.api();
    let perp = inv
        .asset_resolver()
        .await?
        .resolve_perp_or_fetch(api, &args.coin)
        .await
        .map_err(CliError::from)?;
    let coin = perp.display_name();
    let window = paging::read_window(
        api,
        paging::FUNDING_HISTORY,
        args.start,
        args.all,
        |from| {
            InfoRequest::new("fundingHistory")
                .field("coin", coin.clone())
                .time_range(Some(from), args.end)
        },
        "loading funding history",
    )
    .await?;
    paging::announce_truncation(inv, &window);
    let rows = window
        .rows
        .into_iter()
        .map(|row| {
            serde_json::from_value::<FundingRateRow>(row).map_err(|err| {
                CliError::Internal(anyhow::anyhow!(
                    "decode failed while reading funding history: {err}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    inv.print(&Rows::new(rows));
    Ok(())
}

async fn at_oi_cap(inv: &Invocation, args: &AtOiCapArgs) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let dex = canonical_dex(api, args.dex.as_deref()).await?;
    let coins: Vec<String> = api
        .info(
            &InfoRequest::new("perpsAtOpenInterestCap").field_if("dex", dex),
            "loading perps at their open interest cap",
        )
        .await?;
    inv.print(&Rows::new(
        coins.into_iter().map(|coin| CoinRow { coin }).collect(),
    ));
    Ok(())
}

async fn dexs(inv: &Invocation) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let dexes = PerpDexList::load(api).await?;
    let rows = dexes
        .metas(api)
        .await?
        .into_iter()
        .map(|(dex, meta)| DexInfoRow {
            name: dex.name.clone(),
            index: dex.slot,
            full_name: dex.full_name.clone(),
            deployer: dex.deployer.clone(),
            oracle_updater: dex.oracle_updater.clone(),
            fee_recipient: dex.fee_recipient.clone(),
            fees: meta.fee_scales(),
            oi_caps: oi_caps(&dex.oi_caps),
            funding_multipliers: dex
                .funding_multipliers
                .iter()
                .map(|CoinDecimal(coin, multiplier)| FundingMultiplierRow {
                    coin: coin.clone(),
                    multiplier: *multiplier,
                })
                .collect(),
        })
        .collect();
    inv.print(&Rows::new(rows));
    Ok(())
}

async fn dex_limits(inv: &Invocation, args: &DexLimitsArgs) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let Some(dex) = canonical_dex(api, Some(&args.dex)).await? else {
        return Err(CliError::Configuration("dex must not be empty".to_string()).into());
    };
    let limits: Option<RawDexLimits> = api
        .info(
            &InfoRequest::new("perpDexLimits").field("dex", dex.clone()),
            "loading perp DEX limits",
        )
        .await?;
    let limits =
        limits.ok_or_else(|| CliError::Unsupported(format!("DEX {dex} has no limits to read")))?;
    let status: RawDexStatus = api
        .info(
            &InfoRequest::new("perpDexStatus").field("dex", dex.clone()),
            "loading perp DEX status",
        )
        .await?;
    inv.print(&One::new(DexLimitsRow {
        dex,
        total_oi_cap: limits.total_oi_cap,
        oi_sz_cap_per_perp: limits.oi_sz_cap_per_perp,
        max_transfer_ntl: limits.max_transfer_ntl,
        total_net_deposit: status.total_net_deposit,
        oi_caps: oi_caps(&limits.coin_to_oi_cap.unwrap_or_default()),
    }));
    Ok(())
}

pub struct PerpsFundingHistory;

impl CliCommand for PerpsFundingHistory {
    type Args = FundingHistoryArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["perps", "funding-history"],
        "market",
        "List a perpetual's historical funding rates",
        "List the funding rates applied to a perpetual since a start time, oldest first. The API returns 500 rates per request; when a page is full a notice on stderr names the start that continues, or pass all to read the whole window.",
    )
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin, for example BTC or xyz:XYZ100.")
            .resource_id("coin"),
        arg("start")
            .schema_required()
            .schema_string()
            .describe("RFC3339 or epoch-millisecond start time."),
        arg("end")
            .schema_string()
            .describe("RFC3339 or epoch-millisecond end time."),
        arg("all")
            .kind(InputKind::Boolean)
            .describe("Page through the whole window."),
    ]);

    async fn run(inv: &Invocation, args: &FundingHistoryArgs) -> Result<(), anyhow::Error> {
        funding_history(inv, args).await
    }
}

pub struct PerpsFundingPredicted;

impl CliCommand for PerpsFundingPredicted {
    type Args = FundingPredictedArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["perps", "funding-predicted"],
        "market",
        "Show predicted funding rates across venues",
        "Show the funding rate each venue (HlPerp, BinPerp, BybitPerp) predicts for each coin's next funding time, with the venue's funding interval in hours when it states one.",
    )
    .args(&[arg("coin")
        .kind(InputKind::MarketSymbol)
        .describe("Only this coin, for example BTC.")
        .resource_id("coin")]);

    async fn run(inv: &Invocation, args: &FundingPredictedArgs) -> Result<(), anyhow::Error> {
        let raw: PredictedFundings = inv
            .remote()?
            .api()
            .info(
                &InfoRequest::new("predictedFundings"),
                "loading predicted fundings",
            )
            .await?;
        inv.print(&Rows::new(predicted_rows(raw, args.coin.as_deref())));
        Ok(())
    }
}

pub struct PerpsAtOiCap;

impl CliCommand for PerpsAtOiCap {
    type Args = AtOiCapArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["perps", "at-oi-cap"],
        "market",
        "List perpetuals at their open interest cap",
        "List the perpetuals whose open interest is at its cap, so new positions that grow it are refused.",
    )
    .args(&[arg("dex")
        .kind(InputKind::VenueSelector)
        .describe("HIP-3 DEX name.")
        .resource_id("dex")]);

    async fn run(inv: &Invocation, args: &AtOiCapArgs) -> Result<(), anyhow::Error> {
        at_oi_cap(inv, args).await
    }
}

pub struct PerpsDexs;

impl CliCommand for PerpsDexs {
    type Args = crate::command_spec::NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["perps", "dexs"],
        "market",
        "List builder-deployed perp dexes",
        "List the HIP-3 perp dexes with their deployer, fee recipient, deployer fee scale, per-market open interest caps and funding multipliers.",
    );

    async fn run(
        inv: &Invocation,
        _args: &crate::command_spec::NoArgs,
    ) -> Result<(), anyhow::Error> {
        dexs(inv).await
    }
}

pub struct PerpsDexLimits;

impl CliCommand for PerpsDexLimits {
    type Args = DexLimitsArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["perps", "dex-limits"],
        "market",
        "Show a builder-deployed perp dex's limits and net deposits",
        "Show a HIP-3 dex's total open interest cap, per-perp size cap, maximum transfer notional, per-market open interest caps and total net deposit.",
    )
    .args(&[arg("dex")
        .kind(InputKind::VenueSelector)
        .describe("HIP-3 DEX name.")
        .resource_id("dex")]);

    async fn run(inv: &Invocation, args: &DexLimitsArgs) -> Result<(), anyhow::Error> {
        dex_limits(inv, args).await
    }
}
