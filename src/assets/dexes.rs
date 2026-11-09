//! The perp dexes the exchange lists and what each one's `meta` says: the one
//! place `perpDexs` and `meta{dex}` are decoded.
//!
//! `perpDexs` is indexed by protocol slot: the default dex is the `null` at
//! slot 0 and a removed dex stays in the list as `null`, so a dex's position is
//! its slot and the slot is kept on the entry, never recomputed by a caller. A
//! dex whose `meta` names no `collateralToken` is collateralised in USDC.

use futures::future::join_all;
use hypersdk::Decimal;
use hypersdk::hypercore::{Dex, HttpClient};
use serde::{Deserialize, Serialize};

use crate::errors::{CliError, from_sdk};
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

use super::asset_id;
use super::market::{MarginRule, PerpAsset};
use super::query::strip_dex_prefix;

/// Spot token index of USDC, the collateral of a dex that names none.
pub(crate) const USDC_TOKEN: u32 = 0;
/// Name shown for a collateral token the spot metadata does not list.
pub(super) const USDC_NAME: &str = "USDC";

/// `[coin, "decimal"]`, the pair shape of the dex cap and multiplier lists.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct CoinDecimal(
    pub String,
    #[serde(with = "rust_decimal::serde::str")] pub Decimal,
);

/// One builder-deployed perp dex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PerpDex {
    pub name: String,
    /// The dex's position in `perpDexs`; it fixes the dex's asset ids.
    pub slot: usize,
    pub full_name: Option<String>,
    pub deployer: Option<String>,
    pub oracle_updater: Option<String>,
    pub fee_recipient: Option<String>,
    pub oi_caps: Vec<CoinDecimal>,
    pub funding_multipliers: Vec<CoinDecimal>,
}

impl PerpDex {
    /// A dex of which only the name and slot are known.
    fn named(name: String, slot: usize) -> Self {
        Self {
            name,
            slot,
            full_name: None,
            deployer: None,
            oracle_updater: None,
            fee_recipient: None,
            oi_caps: Vec::new(),
            funding_multipliers: Vec::new(),
        }
    }

    /// The SDK's handle on this dex.
    pub(crate) fn sdk(&self) -> Dex {
        Dex::new(self.name.clone(), self.slot)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDex {
    name: String,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    deployer: Option<String>,
    #[serde(default)]
    oracle_updater: Option<String>,
    #[serde(default)]
    fee_recipient: Option<String>,
    #[serde(default)]
    asset_to_streaming_oi_cap: Option<Vec<CoinDecimal>>,
    #[serde(default)]
    asset_to_funding_multiplier: Option<Vec<CoinDecimal>>,
}

/// Every dex `perpDexs` lists, in slot order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PerpDexList {
    dexes: Vec<PerpDex>,
}

impl PerpDexList {
    pub(crate) async fn load(api: &ApiClient) -> Result<Self, CliError> {
        let listed: Vec<Option<RawDex>> = api
            .info(&InfoRequest::new("perpDexs"), "loading perp DEX metadata")
            .await?;
        Ok(Self {
            dexes: listed
                .into_iter()
                .enumerate()
                .filter_map(|(slot, dex)| {
                    let dex = dex?;
                    Some(PerpDex {
                        name: dex.name,
                        slot,
                        full_name: dex.full_name,
                        deployer: dex.deployer,
                        oracle_updater: dex.oracle_updater,
                        fee_recipient: dex.fee_recipient,
                        oi_caps: dex.asset_to_streaming_oi_cap.unwrap_or_default(),
                        funding_multipliers: dex.asset_to_funding_multiplier.unwrap_or_default(),
                    })
                })
                .collect(),
        })
    }

    /// Names and slots only, for callers that hold the SDK client and not the
    /// [`ApiClient`]: the dex lookups orders and the resolver's live HIP-3 read
    /// make with the `HttpClient` they thread.
    pub(crate) async fn load_sdk(client: &HttpClient) -> Result<Self, CliError> {
        let dexes = client.perp_dexes().await.map_err(from_sdk)?;
        Ok(Self {
            dexes: dexes
                .into_iter()
                .map(|dex| PerpDex::named(dex.name().to_string(), dex.index()))
                .collect(),
        })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &PerpDex> {
        self.dexes.iter()
    }

    /// The dex called `name`, ignoring case. An unknown dex is invalid input.
    pub(crate) fn canonical(&self, name: &str) -> Result<&PerpDex, CliError> {
        self.dexes
            .iter()
            .find(|dex| dex.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| CliError::Unsupported(format!("Unknown DEX: {name}")))
    }

    /// The `meta` of every dex, read concurrently, in list order. Each read
    /// stands alone, so the caller picks the failure policy: [`Self::metas`]
    /// fails on the first error, `AssetMetadata::load_raw` lists the dex as
    /// unavailable.
    pub(crate) async fn read_metas(&self, api: &ApiClient) -> Vec<Result<DexMeta, CliError>> {
        join_all(
            self.dexes
                .iter()
                .map(|dex| DexMeta::load(api, Some(&dex.name))),
        )
        .await
    }

    /// [`Self::read_metas`], failing on the first dex that cannot be read.
    pub(crate) async fn metas(
        &self,
        api: &ApiClient,
    ) -> Result<Vec<(&PerpDex, DexMeta)>, CliError> {
        self.dexes
            .iter()
            .zip(self.read_metas(api).await)
            .map(|(dex, meta)| Ok((dex, meta?)))
            .collect()
    }
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
    Ok(canonical_dexs(client, &[dex]).await?.remove(0))
}

/// [`canonical_dex`] for several values, reading `perpDexs` once for all of
/// them and not at all when every one is blank.
pub async fn canonical_dexs(
    client: &HttpClient,
    requested: &[Option<&str>],
) -> Result<Vec<Option<String>>, CliError> {
    let requested: Vec<Option<&str>> = requested
        .iter()
        .map(|dex| super::query::normalize_dex(*dex))
        .collect();
    if requested.iter().all(Option::is_none) {
        return Ok(vec![None; requested.len()]);
    }
    let list = PerpDexList::load_sdk(client).await?;
    requested
        .into_iter()
        .map(|dex| {
            dex.map(|dex| list.canonical(dex).map(|dex| dex.name.clone()))
                .transpose()
        })
        .collect()
}

/// One market of a dex's `meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DexMarket {
    /// As the exchange names it: a HIP-3 market carries its `dex:` prefix.
    pub name: String,
    pub sz_decimals: u32,
    pub margin_rule: MarginRule,
    pub deployer_fee_scale: Option<Decimal>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMeta {
    universe: Vec<RawMarket>,
    #[serde(default)]
    collateral_token: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarket {
    name: String,
    sz_decimals: u32,
    #[serde(default)]
    only_isolated: bool,
    #[serde(default)]
    margin_mode: Option<String>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    deployer_fee_scale: Option<Decimal>,
}

/// What `meta` (the default dex) or `meta{dex}` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DexMeta {
    /// Spot token index of the dex's collateral.
    pub collateral_token: u32,
    pub markets: Vec<DexMarket>,
}

impl DexMeta {
    /// `meta` of `dex`; `None` is the default dex.
    pub(crate) async fn load(api: &ApiClient, dex: Option<&str>) -> Result<Self, CliError> {
        let context = match dex {
            Some(_) => "loading HIP-3 perpetual metadata",
            None => "loading perpetual metadata",
        };
        let raw: RawMeta = api
            .info(&InfoRequest::new("meta").field_if("dex", dex), context)
            .await?;
        Ok(Self::from_raw(raw))
    }

    fn from_raw(raw: RawMeta) -> Self {
        Self {
            collateral_token: raw.collateral_token.unwrap_or(USDC_TOKEN),
            markets: raw
                .universe
                .into_iter()
                .map(|market| DexMarket {
                    margin_rule: MarginRule::from_wire(
                        market.only_isolated,
                        market.margin_mode.as_deref(),
                    ),
                    name: market.name,
                    sz_decimals: market.sz_decimals,
                    deployer_fee_scale: market.deployer_fee_scale,
                })
                .collect(),
        }
    }

    /// The markets as assets. `dex` is the HIP-3 dex they belong to, `None`
    /// for the default dex.
    pub(super) fn into_assets(self, dex: Option<&PerpDex>, collateral: &str) -> Vec<PerpAsset> {
        self.markets
            .into_iter()
            .enumerate()
            .map(|(index, market)| match dex {
                Some(dex) => PerpAsset {
                    name: strip_dex_prefix(&market.name, &dex.name).to_string(),
                    index: asset_id::hip3_asset_index(dex.slot, index),
                    dex: Some(dex.name.clone()),
                    sz_decimals: market.sz_decimals,
                    collateral: collateral.to_string(),
                    margin_rule: market.margin_rule,
                },
                None => PerpAsset {
                    name: market.name,
                    index,
                    dex: None,
                    sz_decimals: market.sz_decimals,
                    collateral: collateral.to_string(),
                    margin_rule: market.margin_rule,
                },
            })
            .collect()
    }

    /// The deployer fee scale of each market that states one.
    pub(crate) fn fee_scales(&self) -> DeployerFeeScales {
        let scales: Vec<(&str, Decimal)> = self
            .markets
            .iter()
            .filter_map(|market| Some((market.name.as_str(), market.deployer_fee_scale?)))
            .collect();
        let uniform = scales
            .first()
            .map(|(_, first)| *first)
            .filter(|first| scales.iter().all(|(_, scale)| scale == first));
        DeployerFeeScales {
            uniform,
            by_market: match uniform {
                Some(_) => Vec::new(),
                None => scales
                    .into_iter()
                    .map(|(coin, scale)| MarketFeeScale {
                        coin: coin.to_string(),
                        deployer_fee_scale: scale,
                    })
                    .collect(),
            },
        }
    }
}

/// One market's deployer fee scale.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MarketFeeScale {
    pub coin: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub deployer_fee_scale: Decimal,
}

/// The deployer fee scales of one dex, as the JSON keys `meta` and `perps
/// dexs` flatten into their dex rows. Markets of the same dex can differ.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct DeployerFeeScales {
    /// The scale all of the dex's markets share; null when it lists none or
    /// its markets differ.
    #[serde(
        rename = "deployer_fee_scale",
        with = "rust_decimal::serde::str_option"
    )]
    pub uniform: Option<Decimal>,
    /// Each market's scale, listed only when the markets of the dex differ.
    #[serde(
        rename = "deployer_fee_scale_by_market",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub by_market: Vec<MarketFeeScale>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(value: serde_json::Value) -> DexMeta {
        DexMeta::from_raw(serde_json::from_value(value).unwrap())
    }

    #[test]
    fn a_dex_that_names_no_collateral_token_is_collateralised_in_usdc() {
        let without = meta(serde_json::json!({"universe": []}));
        let with = meta(serde_json::json!({"universe": [], "collateralToken": 360}));

        assert_eq!(without.collateral_token, USDC_TOKEN);
        assert_eq!(with.collateral_token, 360);
    }

    #[test]
    fn fee_scales_are_uniform_unless_the_markets_differ() {
        let uniform = meta(serde_json::json!({"universe": [
            {"name": "xyz:A", "szDecimals": 2, "deployerFeeScale": "1"},
            {"name": "xyz:B", "szDecimals": 2, "deployerFeeScale": "1"},
            {"name": "xyz:C", "szDecimals": 2}
        ]}))
        .fee_scales();
        assert_eq!(uniform.uniform, Some(Decimal::ONE));
        assert!(uniform.by_market.is_empty());

        let mixed = meta(serde_json::json!({"universe": [
            {"name": "xyz:A", "szDecimals": 2, "deployerFeeScale": "1"},
            {"name": "xyz:B", "szDecimals": 2, "deployerFeeScale": "0.5"}
        ]}))
        .fee_scales();
        assert_eq!(mixed.uniform, None);
        assert_eq!(mixed.by_market.len(), 2);

        let none = meta(serde_json::json!({"universe": [{"name": "xyz:A", "szDecimals": 2}]}))
            .fee_scales();
        assert_eq!(none, DeployerFeeScales::default());
    }

    #[test]
    fn a_dex_market_is_an_asset_on_the_dexs_slot_without_its_prefix() {
        let dex = PerpDex::named("xyz".to_string(), 1);
        let assets = meta(serde_json::json!({"universe": [
            {"name": "xyz:A", "szDecimals": 2},
            {"name": "xyz:B", "szDecimals": 3, "onlyIsolated": true}
        ]}))
        .into_assets(Some(&dex), "USDC");

        assert_eq!(assets[1].name, "B");
        assert_eq!(assets[1].index, 110_001);
        assert_eq!(assets[1].dex.as_deref(), Some("xyz"));
        assert_eq!(assets[1].margin_rule, MarginRule::IsolatedOnly);
    }

    #[test]
    fn dexes_are_found_ignoring_case_and_an_unknown_one_is_refused() {
        let list = PerpDexList {
            dexes: vec![PerpDex::named("xyz".to_string(), 1)],
        };

        assert_eq!(list.canonical("XYZ").unwrap().slot, 1);
        let err = list.canonical("nope").unwrap_err();
        assert!(err.to_string().contains("Unknown DEX: nope"), "{err}");
    }
}
