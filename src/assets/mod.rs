pub mod asset_id;
mod market;
mod metadata;
mod outcome_templates;
mod outcomes;
mod query;
mod resolver;

#[cfg(test)]
pub(crate) use market::test_support;
pub use market::{MarginRule, Market, OutcomeAsset, PerpAsset, SpotAsset};
pub use metadata::{
    AssetMetadata, MetadataCache, SpotListing, canonical_dex, load_spot_markets, require_spot_token,
};
pub(crate) use metadata::{
    DeployerFeeScales, find_perp_dex, load_deployer_fee_scales, load_hip3_market,
    load_raw_perp_dexes,
};
pub use outcome_templates::{OutcomeTemplate, TemplateList, load_outcome_templates};
#[cfg(test)]
pub(crate) use outcomes::catalog_for_tests;
pub use outcomes::{
    OutcomeCatalog, OutcomeQuestionRow, OutcomeSideRow, QuestionRole, SettledOutcomeRow,
    load_outcome_catalog, load_outcome_listing, load_outcome_sides, load_settled_outcome,
    report_skipped,
};
pub use query::{AssetQuery, normalize_dex, parse_asset_query, qualify_dex_asset};
pub(crate) use query::{eq_asset_name, normalize_asset_name};
pub use resolver::{AssetError, AssetResolver};
