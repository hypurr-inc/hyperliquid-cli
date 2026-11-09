//! The public path of the asset layer: tests and every command name assets
//! through the re-exports here, so the submodules stay private and can be
//! reorganised without touching a caller.

pub mod asset_id;
mod dexes;
mod market;
mod metadata;
mod outcome_description;
mod outcome_templates;
mod outcomes;
mod query;
mod resolver;

pub(crate) use dexes::{CoinDecimal, DexMeta, PerpDexList, PerpListing};
pub use dexes::{DeployerFeeScales, MarketFeeScale, canonical_dex, canonical_dexs};
#[cfg(test)]
pub(crate) use market::test_support;
pub use market::{
    MarginRule, Market, OutcomeAsset, PerpAsset, SpotAsset, perp_price_tick, spot_price_tick,
};
pub use metadata::{
    AssetMetadata, MetadataCache, SpotListing, SpotTokenListing, load_spot_markets,
    load_spot_tokens, require_spot_token,
};
pub(crate) use metadata::{load_hip3_market, load_perp_listings};
pub use outcome_description::OutcomeDescription;
pub use outcome_templates::{OutcomeTemplate, TemplateList, TemplateRole, load_outcome_templates};
#[cfg(test)]
pub(crate) use outcomes::catalog_for_tests;
pub use outcomes::{
    Names, OutcomeCatalog, OutcomeQuestionRow, OutcomeSideRow, QuestionRole, QuestionState,
    SettledOutcomeRow, load_settled_outcome,
};
pub use query::{AssetQuery, normalize_dex, parse_asset_query, qualify_dex_asset};
pub(crate) use query::{eq_asset_name, normalize_asset_name};
pub use resolver::{AssetError, AssetResolver};
