//! The perp dexes the exchange lists and what each one's `meta` says: the one
//! place `perpDexs` and `meta{dex}` are decoded.
//!
//! `perpDexs` is indexed by protocol slot: the default dex is the `null` at
//! slot 0 and a removed dex stays in the list as `null`, so a dex's position is
//! its slot and the slot is kept on the entry, never recomputed by a caller. A
//! dex whose `meta` names no `collateralToken` is collateralised in USDC.

use std::convert::Infallible;
use std::str::FromStr;
use std::sync::Arc;

use futures::future::join_all;
use hypersdk::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

use super::asset_id;
use super::market::{MarginRule, PerpAsset};
use super::query::{normalize_dex, strip_dex_prefix};

/// Spot token index of USDC, the collateral of a dex that names none, on every
/// network. A balance row that names no token is not USDC.
pub const USDC_TOKEN: u32 = 0;
/// USDC's name: shown for a collateral token the spot metadata does not list,
/// and the unit of the funds the CLI moves.
pub const USDC_NAME: &str = "USDC";

/// `[coin, "decimal"]`, the pair shape of the dex cap and multiplier lists.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct CoinDecimal(
    pub String,
    #[serde(with = "rust_decimal::serde::str")] pub Decimal,
);

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
    #[cfg(test)]
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
    /// fails on the first error, `AssetMetadata::load_with_hip3` lists the dex as
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

    /// The spot token each dex is collateralised in.
    pub(crate) async fn collateral_tokens(
        &self,
        api: &ApiClient,
    ) -> Result<DexCollateral, CliError> {
        let default = DexMeta::load(api, None).await?;
        Ok(DexCollateral {
            default_token: default.collateral_token,
            dexes: self
                .metas(api)
                .await?
                .into_iter()
                .map(|(dex, meta)| (dex.name.clone(), meta.collateral_token))
                .collect(),
        })
    }
}

/// The spot token index of each perp dex's collateral, 0 being USDC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DexCollateral {
    /// The default dex's.
    pub default_token: u32,
    /// Each listed dex's, by name, in slot order.
    pub dexes: Vec<(String, u32)>,
}

/// A `--dex` value as typed. Only [`DexCatalogue::select`] turns it into a
/// dex, so no command can send a name the exchange has not confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DexArg(String);

impl FromStr for DexArg {
    type Err = Infallible;

    fn from_str(text: &str) -> Result<Self, Infallible> {
        Ok(Self(text.to_string()))
    }
}

impl DexArg {
    /// What the user named; `None` for a blank value, which is the default
    /// dex: the exchange treats the empty name as the first perp dex.
    fn requested(requested: Option<&Self>) -> Option<&str> {
        normalize_dex(requested.map(|dex| dex.0.as_str()))
    }
}

/// The perp dexes of one run: `perpDexs` is read the first time anything needs
/// it and never again. Clones share that one read.
#[derive(Debug, Clone, Default)]
pub struct DexCatalogue(Arc<OnceCell<PerpDexList>>);

impl DexCatalogue {
    pub(crate) async fn list(&self, api: &ApiClient) -> Result<&PerpDexList, CliError> {
        self.0.get_or_try_init(|| PerpDexList::load(api)).await
    }

    /// Validate a `--dex` value against the exchange: the dex it names in its
    /// canonical spelling (which every later request uses), or `None` for the
    /// default dex, which reads nothing. An unknown dex is explicit invalid
    /// input: it fails here instead of reading an empty account or an empty mid
    /// map.
    pub(crate) async fn select(
        &self,
        api: &ApiClient,
        requested: Option<&DexArg>,
    ) -> Result<Option<&PerpDex>, CliError> {
        match DexArg::requested(requested) {
            Some(name) => self.list(api).await?.canonical(name).map(Some),
            None => Ok(None),
        }
    }
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
    max_leverage: Option<u64>,
    #[serde(default)]
    only_isolated: bool,
    #[serde(default)]
    margin_mode: Option<String>,
    #[serde(default, alias = "isDelisted")]
    delisted: bool,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    deployer_fee_scale: Option<Decimal>,
}

/// What `meta` (the default dex) or `meta{dex}` lists.
#[derive(Debug)]
pub(crate) struct DexMeta {
    /// Spot token index of the dex's collateral.
    pub collateral_token: u32,
    markets: Vec<RawMarket>,
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
            markets: raw.universe,
        }
    }

    pub(crate) fn market_count(&self) -> usize {
        self.markets.len()
    }

    /// The markets as assets, each with its asset id and the name of the token
    /// that collateralises it. `dex` is the HIP-3 dex they belong to, `None`
    /// for the default dex; a HIP-3 name loses its `dex:` prefix.
    pub(super) fn into_assets(self, dex: Option<&PerpDex>, collateral: &str) -> Vec<PerpAsset> {
        self.markets
            .into_iter()
            .enumerate()
            .map(|(position, market)| PerpAsset {
                margin_rule: MarginRule::from_wire(
                    market.only_isolated,
                    market.margin_mode.as_deref(),
                ),
                index: match dex {
                    Some(dex) => asset_id::hip3_asset_index(dex.slot, position),
                    None => position,
                },
                name: match dex {
                    Some(dex) => strip_dex_prefix(&market.name, &dex.name).to_string(),
                    None => market.name,
                },
                dex: dex.map(|dex| dex.name.clone()),
                sz_decimals: market.sz_decimals,
                collateral: collateral.to_string(),
                max_leverage: market.max_leverage,
                only_isolated: market.only_isolated,
                delisted: market.delisted,
                deployer_fee_scale: market.deployer_fee_scale,
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
    fn an_asset_keeps_what_perps_list_shows_and_the_asset_id_of_its_slot() {
        let dex = PerpDex::named("xyz".to_string(), 2);
        let assets = meta(serde_json::json!({"universe": [
            {"name": "xyz:A", "szDecimals": 2, "maxLeverage": 10, "isDelisted": true,
             "onlyIsolated": true, "marginMode": "strictIsolated", "deployerFeeScale": "0.5"},
            {"name": "xyz:B", "szDecimals": 3, "maxLeverage": 20, "growthMode": "enabled"}
        ]}))
        .into_assets(Some(&dex), "USDH");

        assert_eq!(
            assets.iter().map(|asset| asset.index).collect::<Vec<_>>(),
            [120_000, 120_001],
            "100000 + slot * 10000 + position"
        );
        let first = &assets[0];
        assert_eq!(
            (first.name.as_str(), first.dex.as_deref()),
            ("A", Some("xyz")),
            "the dex: prefix is the dex's, not the name's"
        );
        assert_eq!(first.max_leverage, Some(10));
        assert!(first.delisted && first.only_isolated);
        assert_eq!(first.margin_rule, MarginRule::StrictIsolated);
        assert_eq!(first.deployer_fee_scale, Some(Decimal::new(5, 1)));
        assert_eq!(first.collateral, "USDH");
        assert!(!assets[1].delisted);
    }

    #[test]
    fn default_dex_assets_are_indexed_by_position() {
        let assets = meta(serde_json::json!({"universe": [
            {"name": "BTC", "szDecimals": 5, "maxLeverage": 40},
            {"name": "ETH", "szDecimals": 4, "maxLeverage": 25}
        ]}))
        .into_assets(None, "USDC");

        assert_eq!(
            assets
                .into_iter()
                .map(|asset| (asset.name, asset.index))
                .collect::<Vec<_>>(),
            [("BTC".to_string(), 0), ("ETH".to_string(), 1)]
        );
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

#[cfg(test)]
mod arg_tests {
    use super::*;

    #[test]
    fn a_blank_dex_flag_is_the_default_dex_and_a_named_one_is_trimmed() {
        let arg = |text: &str| text.parse::<DexArg>().unwrap();

        assert_eq!(DexArg::requested(Some(&arg("  "))), None);
        assert_eq!(DexArg::requested(None), None);
        assert_eq!(DexArg::requested(Some(&arg(" xyz "))), Some("xyz"));
    }
}
