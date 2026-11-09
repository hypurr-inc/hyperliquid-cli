use std::sync::Mutex;
use std::time::{Duration, Instant};

use hypersdk::hypercore::{Chain, HttpClient, PerpMarket, SpotMarket, SpotToken};
use serde::Deserialize;

use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

use super::asset_id;
use super::dexes::{DexMeta, PerpDex, PerpDexList, USDC_NAME};
use super::market::{MarginRule, PerpAsset, SpotAsset};
use super::query::{eq_asset_name, strip_dex_prefix};

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
        let perps = api.sdk().perps().await.map_err(from_sdk)?;
        let spots = load_spot_markets(api).await?;
        Self::from_listings(&perps, spots)
    }

    /// The metadata of markets a command already loaded, so it resolves a name
    /// against them without reading them again.
    pub fn from_listings(perps: &[PerpMarket], spots: Vec<SpotListing>) -> Result<Self, CliError> {
        Ok(Self {
            perps: perps
                .iter()
                .map(|market| perp_asset_from_market(market, None))
                .collect::<Result<_, _>>()?,
            spots: spots
                .into_iter()
                .map(spot_asset_from_listing)
                .collect::<Result<_, _>>()?,
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
        let default_meta = DexMeta::load(api, None).await?;
        let spot_meta = load_raw_spot_meta(api).await?;

        let default_collateral = spot_meta.collateral_name(default_meta.collateral_token);
        let mut perps = default_meta.into_assets(None, &default_collateral);

        let mut unavailable_dexes = Vec::new();
        let dexes = PerpDexList::load(api).await?;
        for (dex, meta) in dexes.iter().zip(dexes.read_metas(api).await) {
            match meta {
                Ok(meta) => {
                    let collateral = spot_meta.collateral_name(meta.collateral_token);
                    perps.extend(meta.into_assets(Some(dex), &collateral));
                }
                Err(_) => unavailable_dexes.push(dex.name.clone()),
            }
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
    market: &PerpMarket,
    dex: Option<String>,
) -> Result<PerpAsset, CliError> {
    let name = match dex.as_deref() {
        Some(dex_name) => strip_dex_prefix(&market.name, dex_name).to_string(),
        None => market.name.clone(),
    };
    Ok(PerpAsset {
        sz_decimals: sz_decimals(market.sz_decimals, &market.name)?,
        margin_rule: MarginRule::of_market(market),
        name,
        index: market.index,
        dex,
        collateral: market.collateral.name.clone(),
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

    /// Name of a perp collateral token, defaulting to USDC when it is unknown.
    fn collateral_name(&self, collateral_token: u32) -> String {
        self.token(collateral_token)
            .or_else(|| self.tokens.get(collateral_token as usize))
            .map_or_else(|| USDC_NAME.to_string(), |token| token.name.clone())
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

/// Look up one market of `dex` live through the SDK client.
///
/// `Ok(None)` means the DEX exists but lists no such market.
pub(crate) async fn load_hip3_market(
    client: &HttpClient,
    dex: &PerpDex,
    token: &str,
) -> Result<Option<PerpMarket>, CliError> {
    Ok(client
        .perps_from(dex.sdk())
        .await
        .map_err(from_sdk)?
        .into_iter()
        .find(|market| {
            let display_token = strip_dex_prefix(&market.name, &dex.name);
            display_token.eq_ignore_ascii_case(token) || market.name.eq_ignore_ascii_case(token)
        }))
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

        assert_eq!(raw.collateral_name(360), "USDH");
        assert_eq!(raw.collateral_name(1), "USDH");
        assert_eq!(raw.collateral_name(999), "USDC");
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
