use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy::primitives::B128;
use hypersdk::hypercore::types::SendToken;
use hypersdk::hypercore::{Chain, SpotToken};
use serde::Deserialize;

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

use super::asset_id;
use super::dexes::{DexMeta, PerpDex, PerpDexList, PerpListing, USDC_NAME};
use super::market::{PerpAsset, SpotAsset};
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

    /// The default dex's perps and every spot market. HIP-3 markets are not
    /// loaded here; they are looked up live on demand.
    pub async fn load(api: &ApiClient) -> Result<Self, CliError> {
        let (perps, spot_meta) = load_default_dex(api).await?;
        Ok(Self {
            perps,
            spots: spot_assets(spot_meta.into_listings())?,
            unavailable_dexes: Vec::new(),
        })
    }

    /// The metadata of markets a command already loaded, so it resolves a name
    /// against them without reading them again.
    pub fn from_listings(perps: Vec<PerpAsset>, spots: Vec<SpotListing>) -> Result<Self, CliError> {
        Ok(Self {
            perps,
            spots: spot_assets(spots)?,
            unavailable_dexes: Vec::new(),
        })
    }

    /// [`Self::load`] plus every HIP-3 market, for `asset search` and
    /// `asset decode`.
    ///
    /// A HIP-3 DEX whose own metadata fails to load does not fail the whole
    /// load: it is listed in [`Self::unavailable_dexes`], and resolving one of
    /// its markets reports that failure instead of "asset not found". A failed
    /// `perpDexs` listing fails the load.
    pub async fn load_with_hip3(api: &ApiClient) -> Result<Self, CliError> {
        let (mut perps, spot_meta) = load_default_dex(api).await?;

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

        Ok(Self {
            perps,
            spots: spot_assets(spot_meta.into_listings())?,
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

/// The default dex's perps and the spot metadata that names their collateral.
async fn load_default_dex(api: &ApiClient) -> Result<(Vec<PerpAsset>, RawSpotMeta), CliError> {
    let meta = DexMeta::load(api, None).await?;
    let spot_meta = load_raw_spot_meta(api).await?;
    let collateral = spot_meta.collateral_name(meta.collateral_token);
    Ok((meta.into_assets(None, &collateral), spot_meta))
}

/// The perp markets of `dex` (`None` is the default dex) as the exchange
/// lists them, each with its asset id and collateral.
pub(crate) async fn load_perp_listings(
    api: &ApiClient,
    dex: Option<&PerpDex>,
) -> Result<Vec<PerpListing>, CliError> {
    let meta = DexMeta::load(api, dex.map(|dex| dex.name.as_str())).await?;
    let collateral = load_raw_spot_meta(api)
        .await?
        .collateral_name(meta.collateral_token);
    Ok(meta.into_listings(dex, &collateral))
}

fn sz_decimals(value: i64, market: &str) -> Result<u32, CliError> {
    u32::try_from(value).map_err(|_| {
        CliError::Internal(anyhow::anyhow!(
            "exchange metadata for {market} has an invalid size precision: {value}"
        ))
    })
}

fn spot_assets(listings: Vec<SpotListing>) -> Result<Vec<SpotAsset>, CliError> {
    listings.into_iter().map(spot_asset_from_listing).collect()
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

/// A spot market as `spotMeta` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotListing {
    pub symbol: String,
    pub index: usize,
    pub base: String,
    pub quote: String,
    pub base_sz_decimals: i64,
    pub quote_sz_decimals: i64,
}

/// A spot token as `spotMeta` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotTokenListing {
    pub name: String,
    pub index: u32,
    pub token_id: B128,
    pub sz_decimals: i64,
    pub wei_decimals: i64,
}

impl SpotTokenListing {
    /// The token as a transfer action names it (`name:tokenId`). Only the name
    /// and id reach the wire; the EVM fields of the SDK type are not read.
    pub fn send_token(&self) -> SendToken {
        SendToken(SpotToken {
            name: self.name.clone(),
            index: self.index,
            token_id: self.token_id,
            evm_contract: None,
            cross_chain_address: None,
            sz_decimals: self.sz_decimals,
            wei_decimals: self.wei_decimals,
            evm_extra_decimals: 0,
        })
    }
}

/// Load all spot markets from `spotMeta`, skipping entries whose token
/// references do not resolve.
pub async fn load_spot_markets(api: &ApiClient) -> Result<Vec<SpotListing>, CliError> {
    load_raw_spot_meta(api)
        .await
        .map(RawSpotMeta::into_listings)
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
    /// Parsed only where a token itself is wanted, so a market list never
    /// fails on a token's id.
    #[serde(default)]
    token_id: Option<String>,
    #[serde(default)]
    wei_decimals: Option<i64>,
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

    fn into_tokens(self) -> Result<Vec<SpotTokenListing>, CliError> {
        self.tokens
            .into_iter()
            .map(|token| {
                let invalid = |what: String| {
                    CliError::Internal(anyhow::anyhow!(
                        "exchange metadata for spot token {} has {what}",
                        token.name
                    ))
                };
                let token_id = token
                    .token_id
                    .as_deref()
                    .ok_or_else(|| invalid("no tokenId".to_string()))?
                    .parse::<B128>()
                    .map_err(|_| invalid("an invalid tokenId".to_string()))?;
                Ok(SpotTokenListing {
                    token_id,
                    wei_decimals: token
                        .wei_decimals
                        .ok_or_else(|| invalid("no weiDecimals".to_string()))?,
                    name: token.name,
                    index: token.index,
                    sz_decimals: token.sz_decimals,
                })
            })
            .collect()
    }
}

async fn load_raw_spot_meta(api: &ApiClient) -> Result<RawSpotMeta, CliError> {
    api.info(&InfoRequest::new("spotMeta"), "loading spot metadata")
        .await
}

/// Every spot token `spotMeta` lists.
pub async fn load_spot_tokens(api: &ApiClient) -> Result<Vec<SpotTokenListing>, CliError> {
    load_raw_spot_meta(api).await?.into_tokens()
}

/// The spot token called `name`, ignoring case and surrounding whitespace.
pub async fn require_spot_token(api: &ApiClient, name: &str) -> Result<SpotTokenListing, CliError> {
    let name = name.trim();
    load_spot_tokens(api)
        .await?
        .into_iter()
        .find(|token| eq_asset_name(&token.name, name))
        .ok_or_else(|| CliError::Unsupported(format!("spot token \"{name}\" not found")))
}

/// Look up one market of `dex` live.
///
/// `Ok(None)` means the DEX exists but lists no such market.
pub(crate) async fn load_hip3_market(
    api: &ApiClient,
    dex: &PerpDex,
    token: &str,
) -> Result<Option<PerpListing>, CliError> {
    Ok(load_perp_listings(api, Some(dex))
        .await?
        .into_iter()
        .find(|listing| {
            let display_token = strip_dex_prefix(&listing.market.name, &dex.name);
            display_token.eq_ignore_ascii_case(token)
                || listing.market.name.eq_ignore_ascii_case(token)
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
    fn a_spot_token_carries_the_id_a_transfer_names_it_by() {
        let raw = raw_meta(serde_json::json!({
            "tokens": [
                {"name": "USDC", "index": 0, "szDecimals": 8, "weiDecimals": 8,
                 "tokenId": "0x6d1e7cde53ba9467b783cb7c530ce054", "evmContract": null},
                {"name": "PURR", "index": 1, "szDecimals": 0, "weiDecimals": 5,
                 "tokenId": "0xc1fb593aeffbeb02f85e0308e9956a90",
                 "evmContract": {"address": "0x391e5a2d5a9a3e6d2b5f1b2d7b3f7e1d3c5a8b9c", "evm_extra_wei_decimals": -2}}
            ],
            "universe": []
        }));

        let tokens = raw.into_tokens().unwrap();

        assert_eq!(tokens[1].index, 1);
        assert_eq!((tokens[1].sz_decimals, tokens[1].wei_decimals), (0, 5));
        assert_eq!(
            tokens[1].send_token().to_string(),
            "PURR:0xc1fb593aeffbeb02f85e0308e9956a90"
        );
        assert_eq!(
            tokens[0].send_token().to_string(),
            "USDC:0x6d1e7cde53ba9467b783cb7c530ce054"
        );
    }

    #[test]
    fn a_token_without_an_id_is_named_not_guessed() {
        let raw = raw_meta(serde_json::json!({
            "tokens": [{"name": "USDC", "index": 0, "szDecimals": 8}],
            "universe": []
        }));

        let err = raw.into_tokens().unwrap_err();

        assert!(err.to_string().contains("USDC") && err.to_string().contains("tokenId"));
    }

    #[test]
    fn a_malformed_token_id_spares_the_market_list_but_not_the_token_read() {
        let raw = || {
            raw_meta(serde_json::json!({
                "tokens": [
                    {"name": "USDC", "index": 0, "tokenId": "0x0", "szDecimals": 6, "weiDecimals": 6},
                    {"name": "PURR", "index": 1, "tokenId": "0x1", "szDecimals": 0, "weiDecimals": 0}
                ],
                "universe": [{"name": "PURR/USDC", "index": 0, "tokens": [1, 0]}]
            }))
        };

        assert_eq!(raw().into_listings().len(), 1);
        let err = raw().into_tokens().unwrap_err();
        assert!(err.to_string().contains("invalid tokenId"), "{err}");
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
