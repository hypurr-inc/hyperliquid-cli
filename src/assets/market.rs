use hypersdk::hypercore::{MarginMode, PerpMarket, PriceTick};

use crate::errors::CliError;

use super::asset_id::{self, OutcomeNotation};

/// A perp market's price grid: five significant figures and at most
/// `6 - szDecimals` decimals.
///
/// The SDK treats a market whose `szDecimals` exceeds the decimal budget (6 for
/// perps, 8 for spot) as invalid and has no tick for it, but such a market
/// still takes whole-number prices. The budget is clamped here, once, so the
/// tick a listing shows is the tick orders are truncated to.
#[must_use]
pub fn perp_price_tick(sz_decimals: i64) -> PriceTick {
    PriceTick::for_perp(sz_decimals.min(6))
}

/// A spot market's price grid; see [`perp_price_tick`] (the budget is 8).
#[must_use]
pub fn spot_price_tick(sz_decimals: i64) -> PriceTick {
    PriceTick::for_spot(sz_decimals.min(8))
}

/// Which margin modes a perpetual market accepts, from `meta`'s `marginMode`
/// and its deprecated duplicate `onlyIsolated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginRule {
    /// Cross and isolated are both allowed (no `marginMode`).
    CrossOrIsolated,
    /// `onlyIsolated: true` without a `marginMode`: isolated only, and whether
    /// margin can be removed is not stated.
    IsolatedOnly,
    /// `noCross`: isolated only; margin can be removed.
    NoCross,
    /// `strictIsolated`: isolated only; margin cannot be removed (it leaves
    /// proportionally as the position closes).
    StrictIsolated,
}

impl MarginRule {
    /// The rule `meta` states, preferring `marginMode` over the legacy flag.
    #[must_use]
    pub fn from_sdk(only_isolated: bool, margin_mode: Option<MarginMode>) -> Self {
        match margin_mode {
            Some(MarginMode::StrictIsolated) => Self::StrictIsolated,
            Some(MarginMode::NoCross) => Self::NoCross,
            None => Self::from_legacy_flag(only_isolated),
        }
    }

    /// [`Self::from_sdk`] for raw `meta`, where `marginMode` is the wire string.
    /// A value this CLI does not know falls back to the legacy flag.
    #[must_use]
    pub(super) fn from_wire(only_isolated: bool, margin_mode: Option<&str>) -> Self {
        match margin_mode {
            Some("strictIsolated") => Self::StrictIsolated,
            Some("noCross") => Self::NoCross,
            _ => Self::from_legacy_flag(only_isolated),
        }
    }

    #[must_use]
    pub fn of_market(market: &PerpMarket) -> Self {
        Self::from_sdk(market.isolated_margin, market.margin_mode)
    }

    fn from_legacy_flag(only_isolated: bool) -> Self {
        if only_isolated {
            Self::IsolatedOnly
        } else {
            Self::CrossOrIsolated
        }
    }

    #[must_use]
    pub fn allows_cross(self) -> bool {
        self == Self::CrossOrIsolated
    }

    /// Only `strictIsolated` is known to refuse margin removal; a legacy
    /// `onlyIsolated` market leaves the answer to the exchange.
    #[must_use]
    pub fn refuses_margin_removal(self) -> bool {
        self == Self::StrictIsolated
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CrossOrIsolated => "cross_or_isolated",
            Self::IsolatedOnly => "isolated_only",
            Self::NoCross => "no_cross",
            Self::StrictIsolated => "strict_isolated",
        }
    }
}

/// Perpetual market, including HIP-3 markets when `dex` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpAsset {
    pub name: String,
    pub index: usize,
    pub dex: Option<String>,
    pub sz_decimals: u32,
    pub collateral: String,
    pub margin_rule: MarginRule,
}

impl PerpAsset {
    /// `name` for default-dex markets, `dex:name` for HIP-3 markets.
    #[must_use]
    pub fn display_name(&self) -> String {
        match &self.dex {
            Some(dex) => format!("{dex}:{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotAsset {
    pub symbol: String,
    pub index: usize,
    pub base: String,
    pub quote: String,
    pub base_sz_decimals: u32,
}

impl SpotAsset {
    /// Coin key info and websocket endpoints use for this pair.
    ///
    /// Only PURR/USDC is addressed by name; every other pair by `@<spot index>`.
    #[must_use]
    pub fn info_coin(&self) -> String {
        if self.base.eq_ignore_ascii_case("PURR")
            && self.quote.eq_ignore_ascii_case("USDC")
            && self.index == asset_id::spot_asset_index(0)
        {
            self.symbol.clone()
        } else {
            asset_id::spot_coin_key(self.index)
        }
    }
}

/// Outcome market side, held as the notation the user typed (`#10` / `+10`).
///
/// The notation is decoded on demand so a malformed one is reported by the
/// command that needs the encoding, not by every resolver caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeAsset {
    notation: String,
}

impl OutcomeAsset {
    #[must_use]
    pub fn new(notation: impl Into<String>) -> Self {
        Self {
            notation: notation.into(),
        }
    }

    #[must_use]
    pub fn notation(&self) -> &str {
        &self.notation
    }

    pub fn parse(&self) -> Result<OutcomeNotation, CliError> {
        asset_id::parse_outcome_notation(&self.notation)
    }

    pub fn asset_index(&self) -> Result<usize, CliError> {
        let asset_id = asset_id::outcome_asset_id(self.parse()?.encoding)?;
        usize::try_from(asset_id).map_err(|_| {
            CliError::Unsupported("outcome asset id is outside supported bounds".to_string())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Market {
    Perp(PerpAsset),
    Spot(SpotAsset),
    Outcome(OutcomeAsset),
}

impl Market {
    /// Protocol asset id used in exchange actions.
    pub fn asset_index(&self) -> Result<usize, CliError> {
        match self {
            Self::Perp(perp) => Ok(perp.index),
            Self::Spot(spot) => Ok(spot.index),
            Self::Outcome(outcome) => outcome.asset_index(),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{MarginRule, PerpAsset, SpotAsset};

    pub(crate) fn perp(name: &str, index: usize) -> PerpAsset {
        PerpAsset {
            name: name.to_string(),
            index,
            dex: None,
            sz_decimals: 8,
            collateral: "USDC".to_string(),
            margin_rule: MarginRule::CrossOrIsolated,
        }
    }

    pub(crate) fn hip3_perp(dex: &str, name: &str, index: usize) -> PerpAsset {
        PerpAsset {
            dex: Some(dex.to_string()),
            ..perp(name, index)
        }
    }

    pub(crate) fn spot(symbol: &str, index: usize, base: &str, quote: &str) -> SpotAsset {
        SpotAsset {
            symbol: symbol.to_string(),
            index,
            base: base.to_string(),
            quote: quote.to_string(),
            base_sz_decimals: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_mode_names_the_rule_and_beats_the_legacy_flag() {
        for (only_isolated, mode, expected) in [
            (false, None, MarginRule::CrossOrIsolated),
            (true, None, MarginRule::IsolatedOnly),
            (true, Some("strictIsolated"), MarginRule::StrictIsolated),
            (true, Some("noCross"), MarginRule::NoCross),
            (false, Some("noCross"), MarginRule::NoCross),
            (true, Some("somethingNew"), MarginRule::IsolatedOnly),
            (false, Some("somethingNew"), MarginRule::CrossOrIsolated),
        ] {
            assert_eq!(MarginRule::from_wire(only_isolated, mode), expected);
        }
        assert_eq!(
            MarginRule::from_sdk(true, Some(MarginMode::StrictIsolated)),
            MarginRule::StrictIsolated
        );
        assert_eq!(
            MarginRule::from_sdk(true, Some(MarginMode::NoCross)),
            MarginRule::NoCross
        );
        assert_eq!(MarginRule::from_sdk(true, None), MarginRule::IsolatedOnly);
    }

    #[test]
    fn only_strict_isolated_refuses_removal_and_only_the_open_rule_allows_cross() {
        assert!(MarginRule::CrossOrIsolated.allows_cross());
        for rule in [
            MarginRule::IsolatedOnly,
            MarginRule::NoCross,
            MarginRule::StrictIsolated,
        ] {
            assert!(!rule.allows_cross());
        }
        assert!(MarginRule::StrictIsolated.refuses_margin_removal());
        assert!(!MarginRule::NoCross.refuses_margin_removal());
        assert!(!MarginRule::IsolatedOnly.refuses_margin_removal());
        assert!(!MarginRule::CrossOrIsolated.refuses_margin_removal());
    }
}
