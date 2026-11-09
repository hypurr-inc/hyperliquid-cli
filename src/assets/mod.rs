//! The public path of the asset layer: tests and every command name assets
//! through the re-exports here, so the submodules stay private and can be
//! reorganised without touching a caller.

mod asset_id;
mod dexes;
mod market;
mod metadata;
mod outcome_description;
mod outcome_templates;
mod outcomes;
mod query;
mod resolver;

pub use asset_id::{
    AssetIdLayout, MAX_OUTCOME_SIDE, classify, outcome_coin, outcome_token, parse_outcome_notation,
    split_outcome_encoding,
};
pub(crate) use dexes::{CoinDecimal, DexCollateral, DexMeta, PerpDex, PerpDexList};
pub use dexes::{DeployerFeeScales, DexArg, USDC_NAME, USDC_TOKEN};
#[cfg(test)]
pub(crate) use market::test_support;
pub use market::{
    MarginRule, Market, OutcomeAsset, PerpAsset, SpotAsset, perp_price_tick, spot_price_tick,
};
pub(crate) use metadata::load_perp_markets;
pub use metadata::{
    AssetMetadata, SpotTokenListing, load_spot_markets, load_spot_markets_with_contexts,
    load_spot_tokens, require_spot_token,
};
pub use outcome_description::OutcomeDescription;
pub use outcome_templates::{OutcomeTemplate, load_outcome_templates};
#[cfg(test)]
pub(crate) use outcomes::catalog_for_tests;
pub use outcomes::{
    Names, OutcomeCatalog, OutcomeQuestionRow, OutcomeSideRow, QuestionState, SettledOutcomeRow,
    load_settled_outcome,
};
pub use query::{AssetQuery, parse_asset_query, qualify_dex_asset};
pub(crate) use query::{eq_asset_name, normalize_asset_name};
pub use resolver::{AssetResolver, ListedSpot, MarketCache};
