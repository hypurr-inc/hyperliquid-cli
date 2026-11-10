//! The margin mode an order or a leverage change names: its one spelling, the
//! rule that a market trading isolated only refuses cross, and the account's
//! own mode, which `activeAssetData` reports.
//!
//! Orders and `positions update-leverage` both depend on these rules, so they
//! live here and not in a command module.

use std::fmt;

use clap::ValueEnum;
use hypersdk::Address;
use hypersdk::hypercore::types::LeverageType;
use serde::{Deserialize, Serialize};

use crate::assets::{MarginRule, PerpAsset};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

/// Perpetual margin-mode intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MarginModeArg {
    Cross,
    Isolated,
}

impl MarginModeArg {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cross => "cross",
            Self::Isolated => "isolated",
        }
    }

    /// The `isCross` of an `updateLeverage` action.
    #[must_use]
    pub fn is_cross(self) -> bool {
        self == Self::Cross
    }

    /// Whether `perp` trades in this mode: every market takes isolated, only
    /// one whose rule allows it takes cross.
    #[must_use]
    pub fn is_available_on(self, perp: &PerpAsset) -> bool {
        self == Self::Isolated || perp.margin_rule.allows_cross()
    }

    /// The mode a market's rule implies when nothing else names one.
    fn default_for(rule: MarginRule) -> Self {
        if rule.allows_cross() {
            Self::Cross
        } else {
            Self::Isolated
        }
    }
}

impl From<LeverageType> for MarginModeArg {
    fn from(leverage_type: LeverageType) -> Self {
        match leverage_type {
            LeverageType::Cross => Self::Cross,
            LeverageType::Isolated => Self::Isolated,
        }
    }
}

impl fmt::Display for MarginModeArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a market states why cross is refused on it: the start of the error each
/// command words its own way.
#[must_use]
pub fn isolated_only_market(perp: &PerpAsset) -> String {
    format!(
        "{} is an isolated-only market ({})",
        perp.display_name(),
        perp.margin_rule.as_str()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginSource {
    /// The user asked for it (`--margin-mode`).
    Requested,
    /// Nobody named one: what the market's rule implies.
    MarketRule,
    /// Nobody named one: what the account is set to, as read from the exchange.
    Account,
}

/// The margin mode of an order on one perpetual market, resolved once and read
/// by validation, the live check, and every preview, so none re-derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginPlan {
    requested: Option<MarginModeArg>,
    rule: MarginRule,
    /// The account's own mode for the market, once read.
    current: Option<MarginModeArg>,
}

impl MarginPlan {
    /// The plan for `perp` when the user asked for `requested`. The market's
    /// rule is public metadata, so asking for cross on an isolated-only market
    /// is refused here, with no account lookup.
    pub fn for_market(
        perp: &PerpAsset,
        requested: Option<MarginModeArg>,
    ) -> Result<Self, CliError> {
        if let Some(mode) = requested
            && !mode.is_available_on(perp)
        {
            return Err(CliError::Unsupported(format!(
                "{}, so --margin-mode cross is not available; use --margin-mode isolated or omit the flag",
                isolated_only_market(perp)
            )));
        }
        Ok(Self {
            requested,
            rule: perp.margin_rule,
            current: None,
        })
    }

    /// The mode this plan stands for: the requested one, else the account's
    /// once read, else the market rule's.
    #[must_use]
    pub fn mode(&self) -> MarginModeArg {
        self.requested
            .or(self.current)
            .unwrap_or_else(|| MarginModeArg::default_for(self.rule))
    }

    #[must_use]
    pub fn source(&self) -> MarginSource {
        match (self.requested, self.current) {
            (Some(_), _) => MarginSource::Requested,
            (None, Some(_)) => MarginSource::Account,
            (None, None) => MarginSource::MarketRule,
        }
    }

    /// The mode the account must already be set to before an order is placed:
    /// only when the user asked for one and the market lets the account choose.
    /// An isolated-only market has no other mode to be in.
    #[must_use]
    pub fn required_account_mode(&self) -> Option<MarginModeArg> {
        self.requested.filter(|_| self.rule.allows_cross())
    }

    /// Whether reading the account would change what a preview shows: the mode
    /// would otherwise be the market rule's guess, and the account can choose.
    #[must_use]
    pub fn account_decides_preview(&self) -> bool {
        self.source() == MarginSource::MarketRule && self.rule.allows_cross()
    }

    /// This plan once the account's own mode is known.
    #[must_use]
    pub fn with_account_mode(mut self, current: MarginModeArg) -> Self {
        self.current = Some(current);
        self
    }
}

/// The margin mode `user` is set to for `perp`, from `activeAssetData`. Unlike
/// a position's leverage type, it exists before any position is open.
pub async fn read_account_mode(
    api: &ApiClient,
    user: Address,
    perp: &PerpAsset,
) -> Result<MarginModeArg, CliError> {
    #[derive(Deserialize)]
    struct ActiveAssetData {
        leverage: ActiveLeverage,
    }

    #[derive(Deserialize)]
    struct ActiveLeverage {
        #[serde(rename = "type")]
        leverage_type: LeverageType,
    }

    // HIP-3 coins are dex-qualified on the wire, as in position rows.
    let request = InfoRequest::user("activeAssetData", user).field("coin", perp.display_name());
    let data: ActiveAssetData = api.info(&request, "loading margin mode").await?;
    Ok(data.leverage.leverage_type.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::test_support::perp;

    fn market(rule: MarginRule) -> PerpAsset {
        PerpAsset {
            margin_rule: rule,
            ..perp("BTC", 0)
        }
    }

    #[test]
    fn a_requested_mode_wins_and_nothing_asked_falls_to_the_market_rule() {
        let open = market(MarginRule::CrossOrIsolated);
        let isolated_only = market(MarginRule::NoCross);

        let requested = MarginPlan::for_market(&open, Some(MarginModeArg::Isolated)).unwrap();
        assert_eq!(requested.mode(), MarginModeArg::Isolated);
        assert_eq!(requested.source(), MarginSource::Requested);

        let open_default = MarginPlan::for_market(&open, None).unwrap();
        assert_eq!(open_default.mode(), MarginModeArg::Cross);
        assert_eq!(open_default.source(), MarginSource::MarketRule);

        let isolated_default = MarginPlan::for_market(&isolated_only, None).unwrap();
        assert_eq!(isolated_default.mode(), MarginModeArg::Isolated);
        assert_eq!(isolated_default.source(), MarginSource::MarketRule);
    }

    #[test]
    fn cross_is_refused_on_every_isolated_only_rule_and_isolated_on_none() {
        for rule in [
            MarginRule::IsolatedOnly,
            MarginRule::NoCross,
            MarginRule::StrictIsolated,
        ] {
            let perp = market(rule);
            let err = MarginPlan::for_market(&perp, Some(MarginModeArg::Cross)).unwrap_err();
            assert_eq!(err.exit_code(), 13, "{rule:?}");
            assert!(
                err.to_string().contains("BTC is an isolated-only market"),
                "{err}"
            );
            assert!(MarginPlan::for_market(&perp, Some(MarginModeArg::Isolated)).is_ok());
        }
        assert!(
            MarginPlan::for_market(
                &market(MarginRule::CrossOrIsolated),
                Some(MarginModeArg::Cross)
            )
            .is_ok()
        );
    }

    #[test]
    fn the_account_is_read_only_where_it_can_change_what_is_shown_or_checked() {
        let open = market(MarginRule::CrossOrIsolated);
        let isolated_only = market(MarginRule::StrictIsolated);

        let none = MarginPlan::for_market(&open, None).unwrap();
        assert!(none.account_decides_preview());
        assert_eq!(none.required_account_mode(), None);
        let known = none.with_account_mode(MarginModeArg::Isolated);
        assert_eq!(known.mode(), MarginModeArg::Isolated);
        assert_eq!(known.source(), MarginSource::Account);
        assert!(!known.account_decides_preview());

        let asked = MarginPlan::for_market(&open, Some(MarginModeArg::Cross)).unwrap();
        assert!(!asked.account_decides_preview());
        assert_eq!(asked.required_account_mode(), Some(MarginModeArg::Cross));
        assert_eq!(
            asked.with_account_mode(MarginModeArg::Isolated).mode(),
            MarginModeArg::Cross
        );

        let forced = MarginPlan::for_market(&isolated_only, Some(MarginModeArg::Isolated)).unwrap();
        assert_eq!(forced.required_account_mode(), None);
        assert!(
            !MarginPlan::for_market(&isolated_only, None)
                .unwrap()
                .account_decides_preview()
        );
    }
}
