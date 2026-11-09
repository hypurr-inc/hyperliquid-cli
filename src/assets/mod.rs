pub mod asset_id;
mod market;
mod metadata;
mod outcomes;
mod query;
mod resolver;

#[cfg(test)]
pub(crate) use market::test_support;
pub use market::{MarginRule, Market, OutcomeAsset, PerpAsset, SpotAsset};
pub use metadata::{
    AssetMetadata, MetadataCache, SpotListing, canonical_dex, load_spot_markets, require_spot_token,
};
pub(crate) use metadata::{RawPerpDex, find_perp_dex, load_hip3_market, load_raw_perp_dexes};
pub use outcomes::{OutcomeSideRow, load_outcome_sides};
pub use query::{AssetQuery, normalize_dex, parse_asset_query, qualify_dex_asset};
pub(crate) use query::{eq_asset_name, normalize_asset_name};
pub use resolver::{AssetError, AssetResolver};
