use strsim::levenshtein;
use tokio::sync::OnceCell;

use crate::errors::CliError;
use crate::http_api::ApiClient;

use super::asset_id;
use super::dexes::DexCatalogue;
use super::market::{Market, OutcomeAsset, PerpAsset, SpotAsset};
use super::metadata::{AssetMetadata, UnavailableDex, load_hip3_market};
use super::query::{AssetQuery, eq_asset_name, normalize_asset_name, parse_asset_query};

/// What one run has read of the exchange's market data: the default dex and
/// spot metadata and the dex catalogue, each read at most once, however many
/// commands steps ask for it.
#[derive(Debug, Default)]
pub struct MarketCache {
    dexes: DexCatalogue,
    metadata: OnceCell<AssetMetadata>,
}

impl MarketCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn dexes(&self) -> &DexCatalogue {
        &self.dexes
    }

    /// A resolver over the run's asset metadata.
    pub async fn resolver(&self, api: &ApiClient) -> Result<AssetResolver, CliError> {
        let metadata = self
            .metadata
            .get_or_try_init(|| AssetMetadata::load(api))
            .await?;
        Ok(self.resolver_over(metadata.clone()))
    }

    /// A resolver over `metadata` a command already holds, so it resolves a
    /// name against them without reading them again. A HIP-3 market it lacks is
    /// still looked up live, through the run's dex catalogue.
    #[must_use]
    pub fn resolver_over(&self, metadata: AssetMetadata) -> AssetResolver {
        AssetResolver::new(metadata).with_dexes(self.dexes.clone())
    }
}

/// A spot pair named by the protocol key `@N` that open orders list it under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListedSpot {
    Known(SpotAsset),
    /// The key names no pair the metadata lists.
    Unknown {
        internal_id: usize,
    },
}

impl ListedSpot {
    /// The asset id an order on the pair names; an unlisted pair's is its key
    /// number.
    #[must_use]
    pub fn asset_index(&self) -> usize {
        match self {
            Self::Known(spot) => spot.index,
            Self::Unknown { internal_id } => *internal_id,
        }
    }

    /// The key info endpoints list the pair under.
    #[must_use]
    pub fn info_coin(&self) -> String {
        match self {
            Self::Known(spot) => spot.info_coin(),
            Self::Unknown { internal_id } => asset_id::spot_internal_coin_key(*internal_id),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AssetResolver {
    metadata: AssetMetadata,
    dexes: DexCatalogue,
}

impl AssetResolver {
    #[must_use]
    pub fn new(metadata: AssetMetadata) -> Self {
        Self {
            metadata,
            dexes: DexCatalogue::default(),
        }
    }

    #[must_use]
    fn with_dexes(self, dexes: DexCatalogue) -> Self {
        Self { dexes, ..self }
    }

    pub(crate) fn dexes(&self) -> &DexCatalogue {
        &self.dexes
    }

    /// Outcome notation resolves to [`Market::Outcome`] without being decoded;
    /// commands that cannot trade outcomes reject it themselves.
    pub fn resolve(&self, input: &str) -> Result<Market, CliError> {
        match parse_asset_query(input) {
            AssetQuery::Perp(_) | AssetQuery::Hip3 { .. } => {
                self.resolve_perp(input).map(Market::Perp)
            }
            AssetQuery::Spot(_) => self.resolve_spot(input).map(Market::Spot),
            AssetQuery::Outcome(notation) => Ok(Market::Outcome(OutcomeAsset::new(notation))),
        }
    }

    pub fn resolve_perp(&self, input: &str) -> Result<PerpAsset, CliError> {
        match parse_asset_query(input) {
            AssetQuery::Hip3 { dex, token } => self.resolve_hip3(input, &dex, &token),
            AssetQuery::Perp(symbol) => self.resolve_default_perp(input, &symbol),
            AssetQuery::Outcome(_) => Err(CliError::unsupported_outcome_notation(input)),
            AssetQuery::Spot(_) => Err(self.not_found(input, self.perp_candidate_names())),
        }
    }

    pub fn resolve_spot(&self, input: &str) -> Result<SpotAsset, CliError> {
        let pair = match parse_asset_query(input) {
            AssetQuery::Spot(pair) => pair,
            AssetQuery::Outcome(_) => return Err(CliError::unsupported_outcome_notation(input)),
            _ => return Err(self.not_found(input, self.spot_candidate_names())),
        };

        self.metadata
            .spots
            .iter()
            .find(|spot| eq_asset_name(&spot.symbol, &pair))
            .cloned()
            .ok_or_else(|| self.not_found(input, self.spot_candidate_names()))
    }

    /// [`Self::resolve_perp`], looking a missing HIP-3 market up live.
    pub async fn resolve_perp_or_fetch(
        &self,
        api: &ApiClient,
        input: &str,
    ) -> Result<PerpAsset, CliError> {
        match self.resolve_perp(input) {
            Ok(perp) => Ok(perp),
            Err(missed) => self.fetch_hip3_perp(api, input, missed).await,
        }
    }

    /// [`Self::resolve`], looking a missing HIP-3 market up live.
    pub async fn resolve_or_fetch(&self, api: &ApiClient, input: &str) -> Result<Market, CliError> {
        match parse_asset_query(input) {
            AssetQuery::Hip3 { .. } => self
                .resolve_perp_or_fetch(api, input)
                .await
                .map(Market::Perp),
            _ => self.resolve(input),
        }
    }

    /// [`Self::resolve_or_fetch`] for a market to trade, whose spot miss
    /// suggests perp names: an order's coin is a perp unless it is spelled as a
    /// pair.
    pub async fn resolve_tradable_or_fetch(
        &self,
        api: &ApiClient,
        input: &str,
    ) -> Result<Market, CliError> {
        match parse_asset_query(input) {
            AssetQuery::Spot(_) => self
                .resolve_spot(input)
                .map(Market::Spot)
                .map_err(|_| self.not_found(input, self.perp_candidate_names())),
            _ => self.resolve_or_fetch(api, input).await,
        }
    }

    /// The pair a protocol spot key `@N` names, or `None` when `coin` is not
    /// one.
    #[must_use]
    pub fn listed_spot(&self, coin: &str) -> Option<ListedSpot> {
        let internal_id = asset_id::parse_spot_coin_key(coin)?;
        Some(
            match self.spot_by_protocol_asset_id(asset_id::spot_asset_index(internal_id)) {
                Some(spot) => ListedSpot::Known(spot),
                None => ListedSpot::Unknown { internal_id },
            },
        )
    }

    #[must_use]
    pub fn display_coin(&self, coin: &str) -> String {
        asset_id::parse_spot_coin_key(coin)
            .and_then(|internal_id| self.spot_for_internal_id(internal_id))
            .map_or_else(|| coin.to_string(), |spot| spot.symbol.clone())
    }

    #[must_use]
    pub fn perp_by_protocol_asset_id(&self, asset_id: usize) -> Option<PerpAsset> {
        self.metadata
            .perps
            .iter()
            .find(|asset| asset.index == asset_id)
            .cloned()
    }

    #[must_use]
    pub fn spot_by_protocol_asset_id(&self, asset_id: usize) -> Option<SpotAsset> {
        self.metadata
            .spots
            .iter()
            .find(|spot| spot.index == asset_id)
            .cloned()
    }

    #[must_use]
    pub fn perps(&self) -> &[PerpAsset] {
        self.metadata.perps()
    }

    #[must_use]
    pub fn spots(&self) -> &[SpotAsset] {
        self.metadata.spots()
    }

    /// HIP-3 DEXes whose metadata failed to load; see
    /// [`AssetMetadata::unavailable_dexes`].
    #[must_use]
    pub fn unavailable_dexes(&self) -> &[UnavailableDex] {
        self.metadata.unavailable_dexes()
    }

    fn spot_for_internal_id(&self, internal_id: usize) -> Option<&SpotAsset> {
        self.metadata
            .spots
            .iter()
            .find(|spot| asset_id::spot_internal_id(spot.index) == internal_id)
    }

    fn resolve_default_perp(&self, input: &str, symbol: &str) -> Result<PerpAsset, CliError> {
        self.metadata
            .perps
            .iter()
            .find(|asset| asset.dex.is_none() && eq_asset_name(&asset.name, symbol))
            .cloned()
            .ok_or_else(|| self.not_found(input, self.perp_candidate_names()))
    }

    fn resolve_hip3(&self, input: &str, dex: &str, token: &str) -> Result<PerpAsset, CliError> {
        self.metadata
            .perps
            .iter()
            .find(|asset| {
                asset
                    .dex
                    .as_deref()
                    .is_some_and(|asset_dex| eq_asset_name(asset_dex, dex))
                    && eq_asset_name(&asset.name, token)
            })
            .cloned()
            .ok_or_else(|| {
                if self
                    .metadata
                    .unavailable_dexes()
                    .iter()
                    .any(|unavailable| eq_asset_name(&unavailable.name, dex))
                {
                    CliError::Unavailable(format!(
                        "Metadata for the HIP-3 dex {dex} could not be loaded, so {input} cannot be resolved. Try again."
                    ))
                } else {
                    self.not_found(input, self.perp_candidate_names())
                }
            })
    }

    /// After `missed` (a failed snapshot lookup of `input`), look a HIP-3
    /// market up live. A DEX without that market reports the original miss.
    async fn fetch_hip3_perp(
        &self,
        api: &ApiClient,
        input: &str,
        missed: CliError,
    ) -> Result<PerpAsset, CliError> {
        let AssetQuery::Hip3 { dex, token } = parse_asset_query(input) else {
            return Err(missed);
        };
        let dex = self.dexes.list(api).await?.canonical(&dex)?;
        load_hip3_market(api, dex, &token).await?.ok_or(missed)
    }

    fn perp_candidate_names(&self) -> Vec<String> {
        self.metadata
            .perps
            .iter()
            .map(PerpAsset::display_name)
            .collect()
    }

    fn spot_candidate_names(&self) -> Vec<String> {
        self.metadata
            .spots
            .iter()
            .map(|spot| spot.symbol.clone())
            .collect()
    }

    fn not_found(&self, input: &str, candidates: Vec<String>) -> CliError {
        let asset = input.to_string();
        match suggestion(input, candidates) {
            Some(suggestions) => CliError::AssetNotFound { asset, suggestions },
            None => CliError::AssetNotFoundNoSuggestion { asset },
        }
    }
}

/// The one close fuzzy match by Levenshtein distance, if any.
///
/// The match must be tight (see [`suggestion_threshold`]); a loose match is
/// worse than none. Ties break by candidate order, then alphabetically.
#[must_use]
fn suggestion<I, S>(input: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let normalized_input = normalize_asset_name(input);
    let max_distance = suggestion_threshold(&normalized_input);

    candidates
        .into_iter()
        .enumerate()
        .filter_map(|(position, candidate)| {
            let candidate = candidate.as_ref().trim().to_string();
            if candidate.is_empty() {
                return None;
            }

            let normalized_candidate = normalize_asset_name(&candidate);
            let distance = levenshtein(&normalized_input, &normalized_candidate);
            is_rankable_suggestion(
                &normalized_input,
                &normalized_candidate,
                distance,
                max_distance,
            )
            .then_some((distance, position, candidate))
        })
        .min_by(
            |(left_distance, left_position, left), (right_distance, right_position, right)| {
                left_distance
                    .cmp(right_distance)
                    .then_with(|| left_position.cmp(right_position))
                    .then_with(|| left.cmp(right))
            },
        )
        .map(|(_, _, candidate)| candidate)
}

/// Largest edit distance still worth suggesting: one edit per three characters
/// (at least one, at most three).
fn suggestion_threshold(input: &str) -> usize {
    (input.chars().count() / 3).clamp(1, 3)
}

fn is_rankable_suggestion(
    input: &str,
    candidate: &str,
    distance: usize,
    max_distance: usize,
) -> bool {
    distance <= max_distance
        && distance < input.chars().count().max(1)
        && candidate.chars().count() >= 2
}

#[cfg(test)]
mod tests {
    use super::super::market::test_support::{hip3_perp, perp, spot};
    use super::*;

    fn resolver() -> AssetResolver {
        AssetResolver::new(AssetMetadata::from_assets(
            vec![perp("BTC", 0), hip3_perp("dex", "TOKEN", 110_000)],
            vec![spot("PURR/USDC", 10_000, "PURR", "USDC")],
        ))
    }

    #[test]
    fn suggests_at_most_one_tight_match() {
        let candidates = ["BTC", "LTC", "BCH", "FARTCOIN", "TON", "NFTI"];

        assert_eq!(suggestion("BTCC", candidates).as_deref(), Some("BTC"));
        assert_eq!(suggestion("ETHH", ["ETH", "ETC"]).as_deref(), Some("ETH"));
        assert_eq!(
            suggestion("BT", ["BTC", "ETH", "BLUR", "BONK", "PURR/USDC"]).as_deref(),
            Some("BTC")
        );
        assert_eq!(suggestion("NOTACOIN", candidates), None);
        assert_eq!(suggestion("ZZZZZZZZ", ["ZEC", "BTC", "ETH"]), None);
    }

    #[test]
    fn long_placeholder_names_get_no_loose_suggestion() {
        let candidates = ["BTC", "ETH", "BLUR", "BONK"];

        assert_eq!(suggestion("INVALIDCOIN", candidates), None);
        assert_eq!(suggestion(&"A".repeat(40), candidates), None);
    }

    #[test]
    fn typo_tolerance_grows_with_input_length() {
        assert_eq!(suggestion("BTCC", ["BTXX"]), None);
        assert_eq!(
            suggestion("NOTACOIN", ["NOTAXXIN"]).as_deref(),
            Some("NOTAXXIN")
        );
        assert_eq!(suggestion("NOTACOIN", ["NOTXXXIN"]), None);

        let long = "A".repeat(40);
        let three_typos = format!("{}BBB", "A".repeat(37));
        let four_typos = format!("{}BBBB", "A".repeat(36));
        assert_eq!(
            suggestion(&long, [three_typos.as_str()]),
            Some(three_typos.clone())
        );
        assert_eq!(suggestion(&long, [four_typos.as_str()]), None);
    }

    #[test]
    fn suggestion_ties_prefer_earlier_candidate() {
        assert_eq!(suggestion("BTX", ["BTC", "BTT"]).as_deref(), Some("BTC"));
    }

    #[test]
    fn a_market_on_a_dex_that_failed_to_load_reports_the_load_failure() {
        let mut metadata = AssetMetadata::from_assets(vec![perp("BTC", 0)], Vec::new());
        metadata.unavailable_dexes = vec![UnavailableDex {
            name: "Broken".to_string(),
            reason: "HTTP 500".to_string(),
        }];
        let resolver = AssetResolver::new(metadata);

        let err = resolver.resolve_perp("broken:GOLD").unwrap_err();
        assert!(matches!(err, CliError::Unavailable(_)), "{err:?}");
        assert_eq!(err.exit_code(), 12);

        assert!(matches!(
            resolver.resolve_perp("other:GOLD"),
            Err(CliError::AssetNotFoundNoSuggestion { .. })
        ));
    }

    #[test]
    fn not_found_carries_one_suggestion() {
        let err = resolver().resolve_perp("BTCC").unwrap_err();

        assert!(matches!(
            err,
            CliError::AssetNotFound { asset, suggestions }
                if asset == "BTCC" && suggestions == "BTC"
        ));

        let err = resolver().resolve_spot("PUR/USDC").unwrap_err();
        assert!(matches!(
            err,
            CliError::AssetNotFound { suggestions, .. } if suggestions == "PURR/USDC"
        ));
    }

    #[test]
    fn resolve_returns_each_market_kind_for_its_query_shape() {
        let resolver = resolver();

        assert!(matches!(resolver.resolve("btc"), Ok(Market::Perp(perp)) if perp.index == 0));
        assert!(
            matches!(resolver.resolve("dex:token"), Ok(Market::Perp(perp)) if perp.index == 110_000)
        );
        assert!(
            matches!(resolver.resolve("purr/usdc"), Ok(Market::Spot(spot)) if spot.index == 10_000)
        );
        assert!(matches!(
            resolver.resolve("#10"),
            Ok(Market::Outcome(outcome)) if outcome.notation() == "#10"
        ));
    }

    #[test]
    fn perp_and_spot_lookups_reject_outcome_notation_with_the_raw_input() {
        let resolver = resolver();

        for err in [
            resolver.resolve_perp(" #10").unwrap_err(),
            resolver.resolve_spot(" #10").unwrap_err(),
        ] {
            assert!(
                matches!(&err, CliError::Unsupported(text) if text.contains("notation ' #10'")),
                "{err:?}"
            );
        }
    }

    #[test]
    fn spot_query_is_not_a_perp_and_a_perp_is_not_a_spot() {
        let resolver = resolver();

        assert!(matches!(
            resolver.resolve_perp("PURR/USDC"),
            Err(CliError::AssetNotFoundNoSuggestion { .. })
        ));
        assert!(matches!(
            resolver.resolve_spot("BTC"),
            Err(CliError::AssetNotFoundNoSuggestion { .. })
        ));
    }

    #[test]
    fn spot_coin_keys_display_as_symbols() {
        let resolver = resolver();

        assert_eq!(resolver.display_coin("@0"), "PURR/USDC");
        assert_eq!(resolver.display_coin("@9"), "@9");
        assert_eq!(resolver.display_coin("BTC"), "BTC");
        assert_eq!(
            resolver.spot_for_internal_id(0).map(|spot| spot.index),
            Some(10_000)
        );
    }

    #[test]
    fn a_miss_is_exit_13_with_at_most_one_suggestion_in_its_text() {
        let not_found = resolver().resolve_perp("BTCC").unwrap_err();
        assert_eq!(not_found.exit_code(), 13);
        assert_eq!(
            not_found.to_string(),
            "Couldn't find asset \"BTCC\". Did you mean BTC?"
        );
        assert_eq!(not_found.suggestion(), Some("BTC"));

        let bare = resolver().resolve_perp("ZZZ").unwrap_err();
        assert!(matches!(bare, CliError::AssetNotFoundNoSuggestion { .. }));

        let outcome = resolver().resolve_perp("#10").unwrap_err();
        assert_eq!(outcome.exit_code(), 13);
        assert!(outcome.to_string().contains("outcome-aware command path"));
        assert!(outcome.to_string().contains("outcomes get #10"));
    }

    #[test]
    fn protocol_spot_keys_name_a_listed_pair_or_stay_unknown() {
        let resolver = resolver();

        assert!(matches!(
            resolver.listed_spot("@0"),
            Some(ListedSpot::Known(spot)) if spot.index == 10_000
        ));
        let unknown = resolver.listed_spot("@9").unwrap();
        assert_eq!(unknown, ListedSpot::Unknown { internal_id: 9 });
        assert_eq!(
            (unknown.asset_index(), unknown.info_coin()),
            (9, "@9".to_string())
        );
        assert_eq!(resolver.listed_spot("BTC"), None);
        assert_eq!(resolver.listed_spot("PURR/USDC"), None);
    }
}

#[cfg(test)]
mod tradable_tests {
    use super::super::market::test_support::{perp, spot};
    use super::*;
    use crate::network::Network;

    #[tokio::test]
    async fn a_spot_miss_to_trade_suggests_perp_names_while_a_lookup_suggests_pairs() {
        let resolver = AssetResolver::new(AssetMetadata::from_assets(
            vec![perp("PURR", 0)],
            vec![spot("PURR/USDC", 10_000, "PURR", "USDC")],
        ));
        let api = ApiClient::new(Network::Testnet, None);

        let looked_up = resolver
            .resolve_or_fetch(&api, "PUR/USDC")
            .await
            .unwrap_err();
        let tradable = resolver
            .resolve_tradable_or_fetch(&api, "PUR/USDC")
            .await
            .unwrap_err();

        assert_eq!(looked_up.suggestion(), Some("PURR/USDC"));
        assert_eq!(tradable.suggestion(), None, "{tradable}");
        assert!(matches!(
            resolver.resolve_tradable_or_fetch(&api, "PURR/USDC").await,
            Ok(Market::Spot(_))
        ));
    }
}
