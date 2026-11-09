use std::sync::Mutex;
use std::time::{Duration, Instant};

use hypersdk::Decimal;
use hypersdk::hypercore::{Chain, Dex, HttpClient, PerpMarket, SpotMarket, SpotToken};
use serde::Deserialize;

use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

use super::asset_id;
use super::market::{MarginRule, PerpAsset, SpotAsset};
use super::query::{eq_asset_name, normalize_dex, strip_dex_prefix};

const METADATA_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetMetadata {
    pub(super) perps: Vec<PerpAsset>,
    pub(super) spots: Vec<SpotAsset>,
    pub(super) unavailable_dexes: Vec<String>,
}

impl AssetMetadata {
    #[must_use]
    pub fn from_assets(perps: Vec<PerpAsset>, spots: Vec<SpotAsset>) -> Self {
        Self {
            perps,
            spots,
            unavailable_dexes: Vec::new(),
        }
    }

    /// HIP-3 markets are not loaded here; they are looked up live on demand.
    pub async fn load(api: &ApiClient) -> Result<Self, CliError> {
        let default_perps = api.sdk().perps().await.map_err(from_sdk)?;
        let spots = load_spot_markets(api)
            .await?
            .into_iter()
            .map(spot_asset_from_listing)
            .collect::<Result<Vec<_>, _>>()?;
        let perps = default_perps
            .into_iter()
            .map(|market| perp_asset_from_market(market, None))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            perps,
            spots,
            unavailable_dexes: Vec::new(),
        })
    }

    /// Used when the `hypersdk` loader cannot decode a network's metadata and by
    /// `asset search`/`asset decode`, which need every HIP-3 market.
    ///
    /// A HIP-3 DEX whose own metadata fails to load does not fail the whole
    /// load: it is listed in [`Self::unavailable_dexes`], and resolving one of
    /// its markets reports that failure instead of "asset not found". A failed
    /// `perpDexs` listing fails the load.
    pub async fn load_raw(api: &ApiClient) -> Result<Self, CliError> {
        #[derive(Deserialize)]
        struct MetaResponse {
            universe: Vec<MetaPerp>,
            #[serde(rename = "collateralToken")]
            collateral_token: Option<usize>,
        }

        #[derive(Deserialize)]
        struct MetaPerp {
            name: String,
            #[serde(rename = "szDecimals")]
            sz_decimals: u32,
            #[serde(rename = "onlyIsolated", default)]
            only_isolated: bool,
            #[serde(rename = "marginMode", default)]
            margin_mode: Option<String>,
        }

        impl MetaPerp {
            fn margin_rule(&self) -> MarginRule {
                MarginRule::from_wire(self.only_isolated, self.margin_mode.as_deref())
            }
        }

        let meta = api
            .info::<MetaResponse>(&InfoRequest::new("meta"), "loading perpetual metadata")
            .await?;
        let spot_meta = load_raw_spot_meta(api).await?;

        let default_collateral = spot_meta.collateral_name(meta.collateral_token);
        let mut perps = meta
            .universe
            .into_iter()
            .enumerate()
            .map(|(index, perp)| PerpAsset {
                margin_rule: perp.margin_rule(),
                name: perp.name,
                index,
                dex: None,
                sz_decimals: perp.sz_decimals,
                collateral: default_collateral.clone(),
            })
            .collect::<Vec<_>>();

        let mut unavailable_dexes = Vec::new();
        for (dex_slot, dex) in load_raw_perp_dexes(api).await?.into_iter().enumerate() {
            let Some(dex) = dex else { continue };
            let Ok(meta) = api
                .info::<MetaResponse>(
                    &InfoRequest::new("meta").field("dex", dex.name.clone()),
                    "loading HIP-3 perpetual metadata",
                )
                .await
            else {
                unavailable_dexes.push(dex.name);
                continue;
            };
            let collateral = spot_meta.collateral_name(meta.collateral_token);
            perps.extend(meta.universe.into_iter().enumerate().map(|(index, perp)| {
                let name = strip_dex_prefix(&perp.name, &dex.name).to_string();
                PerpAsset {
                    margin_rule: perp.margin_rule(),
                    name,
                    index: asset_id::hip3_asset_index(dex_slot, index),
                    dex: Some(dex.name.clone()),
                    sz_decimals: perp.sz_decimals,
                    collateral: collateral.clone(),
                }
            }));
        }

        let spots = spot_meta
            .into_listings()
            .into_iter()
            .map(spot_asset_from_listing)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            perps,
            spots,
            unavailable_dexes,
        })
    }

    #[must_use]
    pub fn perps(&self) -> &[PerpAsset] {
        &self.perps
    }

    #[must_use]
    pub fn spots(&self) -> &[SpotAsset] {
        &self.spots
    }

    /// HIP-3 DEXes whose metadata failed to load, so their markets are missing.
    #[must_use]
    pub fn unavailable_dexes(&self) -> &[String] {
        &self.unavailable_dexes
    }
}

fn sz_decimals(value: i64, market: &str) -> Result<u32, CliError> {
    u32::try_from(value).map_err(|_| {
        CliError::Internal(anyhow::anyhow!(
            "exchange metadata for {market} has an invalid size precision: {value}"
        ))
    })
}

/// Convert an SDK perp market, dropping the `dex:` prefix HIP-3 names carry.
pub(super) fn perp_asset_from_market(
    market: PerpMarket,
    dex: Option<String>,
) -> Result<PerpAsset, CliError> {
    let name = match dex.as_deref() {
        Some(dex_name) => strip_dex_prefix(&market.name, dex_name).to_string(),
        None => market.name.clone(),
    };
    Ok(PerpAsset {
        sz_decimals: sz_decimals(market.sz_decimals, &market.name)?,
        margin_rule: MarginRule::of_market(&market),
        name,
        index: market.index,
        dex,
        collateral: market.collateral.name,
    })
}

fn spot_asset_from_listing(listing: SpotListing) -> Result<SpotAsset, CliError> {
    Ok(SpotAsset {
        base_sz_decimals: sz_decimals(listing.base_sz_decimals, &listing.symbol)?,
        symbol: listing.symbol,
        index: listing.index,
        base: listing.base,
        quote: listing.quote,
    })
}

/// A spot market as listed by the exchange, whichever loader produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotListing {
    pub symbol: String,
    pub index: usize,
    pub base: String,
    pub quote: String,
    pub base_sz_decimals: i64,
    pub quote_sz_decimals: i64,
}

impl From<SpotMarket> for SpotListing {
    fn from(market: SpotMarket) -> Self {
        let [base, quote] = &market.tokens;
        Self {
            symbol: market.symbol(),
            index: market.index,
            base: base.name.clone(),
            quote: quote.name.clone(),
            base_sz_decimals: base.sz_decimals,
            quote_sz_decimals: quote.sz_decimals,
        }
    }
}

/// Load all spot markets through `hypersdk`, falling back to raw `spotMeta`
/// (which skips entries with broken token references) when the SDK cannot
/// decode it. A failed fallback reports the SDK error.
pub async fn load_spot_markets(api: &ApiClient) -> Result<Vec<SpotListing>, CliError> {
    match api.sdk().spot().await {
        Ok(markets) => Ok(markets.into_iter().map(SpotListing::from).collect()),
        Err(spot_err) => load_raw_spot_meta(api)
            .await
            .map(RawSpotMeta::into_listings)
            .map_err(|_| from_sdk(spot_err)),
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RawSpotMeta {
    universe: Vec<RawSpotUniverseItem>,
    tokens: Vec<RawSpotToken>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RawSpotUniverseItem {
    #[serde(default)]
    tokens: Vec<u32>,
    index: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RawSpotToken {
    name: String,
    index: u32,
    sz_decimals: i64,
}

impl RawSpotMeta {
    fn token(&self, token_ref: u32) -> Option<&RawSpotToken> {
        self.tokens.iter().find(|token| token.index == token_ref)
    }

    /// Name of a perp collateral token, defaulting to USDC when the exchange
    /// names none or it is unknown.
    fn collateral_name(&self, collateral_token: Option<usize>) -> String {
        collateral_token
            .and_then(|position| {
                u32::try_from(position)
                    .ok()
                    .and_then(|index| self.token(index))
                    .or_else(|| self.tokens.get(position))
            })
            .map_or_else(|| "USDC".to_string(), |token| token.name.clone())
    }

    fn into_listings(self) -> Vec<SpotListing> {
        self.universe
            .iter()
            .filter_map(|item| {
                let base = self.token(*item.tokens.first()?)?;
                let quote = self.token(*item.tokens.get(1)?)?;
                Some(SpotListing {
                    symbol: format!("{}/{}", base.name, quote.name),
                    index: asset_id::spot_asset_index(item.index),
                    base: base.name.clone(),
                    quote: quote.name.clone(),
                    base_sz_decimals: base.sz_decimals,
                    quote_sz_decimals: quote.sz_decimals,
                })
            })
            .collect()
    }
}

async fn load_raw_spot_meta(api: &ApiClient) -> Result<RawSpotMeta, CliError> {
    api.info(&InfoRequest::new("spotMeta"), "loading raw spot metadata")
        .await
}

/// One `perpDexs` entry. The dex carries no deployer fee scale: it is a
/// property of each market in the dex's own `meta` (see
/// [`load_deployer_fee_scales`]).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawPerpDex {
    pub name: String,
}

/// The deployer fee scale of each market of one HIP-3 dex that states one.
/// Markets of the same dex can differ.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DeployerFeeScales {
    markets: Vec<(String, Decimal)>,
}

impl DeployerFeeScales {
    /// The scale every market shares; `None` when the dex lists no scale or its
    /// markets disagree.
    pub(crate) fn uniform(&self) -> Option<Decimal> {
        let (_, first) = self.markets.first()?;
        self.markets
            .iter()
            .all(|(_, scale)| scale == first)
            .then_some(*first)
    }

    /// Each market's scale, as `(coin, scale)`.
    pub(crate) fn by_market(&self) -> &[(String, Decimal)] {
        &self.markets
    }
}

/// Read the deployer fee scales from `meta{dex}.universe[].deployerFeeScale`.
pub(crate) async fn load_deployer_fee_scales(
    api: &ApiClient,
    dex: &str,
) -> Result<DeployerFeeScales, CliError> {
    #[derive(Deserialize)]
    struct DexMeta {
        universe: Vec<DexMarket>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DexMarket {
        name: String,
        #[serde(default, with = "rust_decimal::serde::str_option")]
        deployer_fee_scale: Option<Decimal>,
    }

    let meta: DexMeta = api
        .info(
            &InfoRequest::new("meta").field("dex", dex),
            "loading perp DEX markets",
        )
        .await?;
    Ok(DeployerFeeScales {
        markets: meta
            .universe
            .into_iter()
            .filter_map(|market| Some((market.name, market.deployer_fee_scale?)))
            .collect(),
    })
}

/// The raw `perpDexs` list. Slot `n` is the dex at protocol position `n`, so a
/// removed dex stays in the list as `None`.
pub(crate) async fn load_raw_perp_dexes(
    api: &ApiClient,
) -> Result<Vec<Option<RawPerpDex>>, CliError> {
    api.info(&InfoRequest::new("perpDexs"), "loading perp DEX metadata")
        .await
}

/// The spot token called `name`, ignoring case and surrounding whitespace.
pub async fn require_spot_token(client: &HttpClient, name: &str) -> Result<SpotToken, CliError> {
    let name = name.trim();
    client
        .spot_tokens()
        .await
        .map_err(from_sdk)?
        .into_iter()
        .find(|token| eq_asset_name(&token.name, name))
        .ok_or_else(|| CliError::Unsupported(format!("spot token \"{name}\" not found")))
}

/// Find a HIP-3 DEX by name, case-insensitively.
pub(crate) async fn find_perp_dex(client: &HttpClient, dex_name: &str) -> Result<Dex, CliError> {
    client
        .perp_dexes()
        .await
        .map_err(from_sdk)?
        .into_iter()
        .find(|dex| dex.name().eq_ignore_ascii_case(dex_name))
        .ok_or_else(|| CliError::Unsupported(format!("Unknown DEX: {dex_name}")))
}

/// Validate a `--dex` value against the exchange and return its canonical name.
///
/// A blank value is the default dex. An unknown DEX is explicit invalid input:
/// it fails here instead of reading an empty account or an empty mid map, and
/// the canonical spelling is what every later request uses.
pub async fn canonical_dex(
    client: &HttpClient,
    dex: Option<&str>,
) -> Result<Option<String>, CliError> {
    match normalize_dex(dex) {
        Some(dex) => Ok(Some(find_perp_dex(client, dex).await?.name().to_string())),
        None => Ok(None),
    }
}

/// Look up one HIP-3 market live, returning the DEX's canonical name with it.
///
/// `Ok(None)` means the DEX exists but lists no such market.
pub(crate) async fn load_hip3_market(
    client: &HttpClient,
    dex_name: &str,
    token: &str,
) -> Result<Option<(String, PerpMarket)>, CliError> {
    let dex = find_perp_dex(client, dex_name).await?;
    let canonical_dex_name = dex.name().to_string();
    let market = client
        .perps_from(dex)
        .await
        .map_err(from_sdk)?
        .into_iter()
        .find(|market| {
            let display_token = strip_dex_prefix(&market.name, &canonical_dex_name);
            display_token.eq_ignore_ascii_case(token) || market.name.eq_ignore_ascii_case(token)
        });
    Ok(market.map(|market| (canonical_dex_name, market)))
}

#[derive(Debug, Clone)]
struct CachedMetadata {
    metadata: AssetMetadata,
    fetched_at: Instant,
}

impl CachedMetadata {
    fn is_fresh_at(&self, now: Instant) -> bool {
        now.duration_since(self.fetched_at) <= METADATA_TTL
    }
}

#[derive(Debug, Default)]
pub struct MetadataCache {
    cached: Mutex<Vec<(Chain, CachedMetadata)>>,
}

impl MetadataCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_or_load(&self, api: &ApiClient) -> Result<AssetMetadata, CliError> {
        let chain = api.chain();
        if let Some(metadata) = self.fresh_metadata_for_chain_at(chain, Instant::now()) {
            return Ok(metadata);
        }

        let metadata = AssetMetadata::load(api).await?;
        self.store_for_chain(chain, metadata.clone(), Instant::now());
        Ok(metadata)
    }

    fn store_for_chain(&self, chain: Chain, metadata: AssetMetadata, fetched_at: Instant) {
        let mut cached = self.cached.lock().expect("metadata cache lock poisoned");
        cached.retain(|(cached_chain, _)| *cached_chain != chain);
        cached.push((
            chain,
            CachedMetadata {
                metadata,
                fetched_at,
            },
        ));
    }

    fn fresh_metadata_for_chain_at(&self, chain: Chain, now: Instant) -> Option<AssetMetadata> {
        self.cached
            .lock()
            .expect("metadata cache lock poisoned")
            .iter()
            .find(|(cached_chain, cached)| *cached_chain == chain && cached.is_fresh_at(now))
            .map(|(_, cached)| cached.metadata.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::super::market::test_support::perp;
    use super::*;

    fn fixture_metadata(asset_name: &str, asset_index: usize) -> AssetMetadata {
        AssetMetadata::from_assets(vec![perp(asset_name, asset_index)], Vec::new())
    }

    fn raw_meta(value: serde_json::Value) -> RawSpotMeta {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn raw_spot_markets_skip_unresolvable_token_references() {
        let raw = raw_meta(serde_json::json!({
            "tokens": [
                {"name": "USDC", "index": 0, "szDecimals": 6},
                {"name": "PURR", "index": 1, "szDecimals": 0}
            ],
            "universe": [
                {"tokens": [1, 0], "index": 0, "name": "PURR/USDC"},
                {"tokens": [9, 0], "index": 1, "name": "BADBASE/USDC"},
                {"tokens": [1, 8], "index": 2, "name": "PURR/BADQUOTE"},
                {"tokens": [1], "index": 3, "name": "SHORT"}
            ]
        }));

        let markets = raw.into_listings();

        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0].symbol, "PURR/USDC");
        assert_eq!(markets[0].index, 10_000);
        assert_eq!(markets[0].base_sz_decimals, 0);
        assert_eq!(markets[0].quote_sz_decimals, 6);
    }

    #[test]
    fn collateral_name_prefers_token_index_then_position_then_usdc() {
        let raw = raw_meta(serde_json::json!({
            "tokens": [
                {"name": "USDC", "index": 0, "szDecimals": 6},
                {"name": "USDH", "index": 360, "szDecimals": 2}
            ],
            "universe": []
        }));

        assert_eq!(raw.collateral_name(Some(360)), "USDH");
        assert_eq!(raw.collateral_name(Some(1)), "USDH");
        assert_eq!(raw.collateral_name(Some(999)), "USDC");
        assert_eq!(raw.collateral_name(None), "USDC");
    }

    #[test]
    fn invalid_size_precision_is_an_error_not_zero() {
        assert_eq!(sz_decimals(5, "BTC").unwrap(), 5);
        let err = sz_decimals(-1, "BTC").unwrap_err();
        assert!(err.to_string().contains("BTC"));
        assert!(sz_decimals(i64::MAX, "BTC").is_err());
    }

    #[test]
    fn spot_listing_with_invalid_size_precision_fails_conversion() {
        let listing = SpotListing {
            symbol: "BAD/USDC".to_string(),
            index: 10_001,
            base: "BAD".to_string(),
            quote: "USDC".to_string(),
            base_sz_decimals: -3,
            quote_sz_decimals: 6,
        };

        assert!(spot_asset_from_listing(listing).is_err());
    }

    #[test]
    fn metadata_cache_entries_are_independent_per_chain() {
        let cache = MetadataCache::new();
        let now = Instant::now();
        let mainnet_metadata = fixture_metadata("BTC", 0);
        let testnet_metadata = fixture_metadata("TESTBTC", 10);

        cache.store_for_chain(Chain::Mainnet, mainnet_metadata.clone(), now);
        cache.store_for_chain(Chain::Testnet, testnet_metadata.clone(), now);

        assert_eq!(
            cache.fresh_metadata_for_chain_at(Chain::Mainnet, now),
            Some(mainnet_metadata)
        );
        assert_eq!(
            cache.fresh_metadata_for_chain_at(Chain::Testnet, now),
            Some(testnet_metadata)
        );
    }

    #[test]
    fn metadata_cache_ttl_is_applied_per_chain() {
        let cache = MetadataCache::new();
        let now = Instant::now();
        let stale_mainnet_fetched_at = now - METADATA_TTL - Duration::from_secs(1);
        let fresh_testnet_fetched_at = now - METADATA_TTL + Duration::from_secs(1);
        let testnet_metadata = fixture_metadata("TESTBTC", 10);

        cache.store_for_chain(
            Chain::Mainnet,
            fixture_metadata("BTC", 0),
            stale_mainnet_fetched_at,
        );
        cache.store_for_chain(
            Chain::Testnet,
            testnet_metadata.clone(),
            fresh_testnet_fetched_at,
        );

        assert_eq!(cache.fresh_metadata_for_chain_at(Chain::Mainnet, now), None);
        assert_eq!(
            cache.fresh_metadata_for_chain_at(Chain::Testnet, now),
            Some(testnet_metadata)
        );
    }
}
