//! The account-mode domain: an account's abstraction mode, where its balances
//! live under each mode, the collateral token of each perp dex and the unified
//! account risk.
//!
//! Layer rule: below `commands`; it reads the API and never imports a command
//! or a presentation type. `account abstraction` prints it, and order
//! placement, transfers and `watch risk` decide by it.

use std::collections::BTreeMap;

use hypersdk::hypercore::AbstractionMode;
use hypersdk::hypercore::types::{ClearinghouseState, LeverageType};
use hypersdk::{Address, Decimal};
use serde::Serialize;

use crate::assets::{DexMeta, PerpDexList};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::spot_balances::RawSpotBalance;
use crate::wire::InfoRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAbstraction {
    pub raw_mode: String,
    pub normalized_mode: AccountMode,
}

/// The account abstraction a protocol `userAbstraction` value names. It decides
/// where an account's balances live and who moves collateral between dexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountMode {
    /// `disabled` and `default`: separate perp and spot balances.
    Standard {
        /// The raw value was `default` rather than `disabled`.
        is_default: bool,
    },
    Unified,
    PortfolioMargin,
    DexAbstraction,
    Unknown,
}

/// Every known mode with the protocol `userAbstraction` value that names it and
/// the spelling the CLI shows and accepts. The one place the spellings live.
const SPELLINGS: [(AccountMode, &str, &str); 5] = [
    (AccountMode::Unified, "unifiedAccount", "unified-account"),
    (
        AccountMode::PortfolioMargin,
        "portfolioMargin",
        "portfolio-margin",
    ),
    (
        AccountMode::Standard { is_default: false },
        "disabled",
        "disabled",
    ),
    (
        AccountMode::Standard { is_default: true },
        "default",
        "default",
    ),
    (
        AccountMode::DexAbstraction,
        "dexAbstraction",
        "dex-abstraction",
    ),
];

impl AccountMode {
    pub fn parse(raw: &str) -> Self {
        SPELLINGS
            .iter()
            .find(|(_, protocol, _)| *protocol == raw)
            .map_or(Self::Unknown, |(mode, _, _)| *mode)
    }

    /// The spelling `account abstraction` reports and `account abstraction
    /// set --mode` takes.
    pub fn as_str(self) -> &'static str {
        SPELLINGS
            .iter()
            .find(|(mode, _, _)| *mode == self)
            .map_or("unknown", |(_, _, cli)| cli)
    }

    /// Balances live in the spot clearinghouse state; perp dex user states are
    /// not meaningful.
    pub fn pools_collateral(self) -> bool {
        matches!(self, Self::Unified | Self::PortfolioMargin)
    }

    /// The exchange itself moves collateral to a HIP-3 dex an order trades on.
    pub fn moves_dex_collateral_itself(self) -> bool {
        self.pools_collateral() || self == Self::DexAbstraction
    }
}

impl From<AbstractionMode> for AccountMode {
    /// Through the SDK's protocol spelling (its `Standard` is `disabled`), so
    /// a mode is spelled in [`SPELLINGS`] only.
    fn from(mode: AbstractionMode) -> Self {
        Self::parse(mode.api_str())
    }
}

impl Serialize for AccountMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

pub async fn fetch_abstraction(
    api: &ApiClient,
    user: Address,
) -> Result<UserAbstraction, CliError> {
    let raw_mode = api
        .info::<String>(
            &InfoRequest::user("userAbstraction", user),
            "loading user abstraction",
        )
        .await?;
    Ok(UserAbstraction {
        normalized_mode: AccountMode::parse(&raw_mode),
        raw_mode,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DexCollateral {
    /// `None` is the default dex.
    pub dex: Option<String>,
    /// Spot token index of the collateral, 0 being USDC.
    pub token: u32,
}

pub async fn load_dex_collateral(api: &ApiClient) -> Result<Vec<DexCollateral>, CliError> {
    let default = DexMeta::load(api, None).await?;
    let dexes = PerpDexList::load(api).await?;
    let mut collateral = vec![DexCollateral {
        dex: None,
        token: default.collateral_token,
    }];
    for (dex, meta) in dexes.metas(api).await? {
        collateral.push(DexCollateral {
            dex: Some(dex.name.clone()),
            token: meta.collateral_token,
        });
    }
    Ok(collateral)
}

/// How close a unified account is to liquidation, per the account-abstraction
/// docs: the worst collateral token's cross maintenance margin over what the
/// account holds of it that isolated positions do not claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnifiedRatio {
    pub ratio: Decimal,
    pub collateral_token: u32,
}

/// What the unified account ratio finds: the worst token's ratio, or a token
/// whose cross maintenance margin has no collateral left to cover it, which is
/// worse than any ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifiedRisk {
    Ratio(UnifiedRatio),
    Uncovered { collateral_token: u32 },
}

/// The unified account risk of `dexes`, each given with its collateral token
/// and clearinghouse state, against the spot `balances`. A token with no
/// cross maintenance margin is not at risk. A token that has some but whose
/// balance less isolated margin is missing or not positive is `Uncovered`, and
/// outranks every ratio; `None` when no token has maintenance margin.
pub(crate) fn unified_account_ratio(
    dexes: &[(u32, &ClearinghouseState)],
    balances: &[RawSpotBalance],
) -> Option<UnifiedRisk> {
    // Per collateral token: cross maintenance margin, and the margin isolated
    // positions hold out of the balance.
    let mut claims: BTreeMap<u32, (Decimal, Decimal)> = BTreeMap::new();
    for (token, state) in dexes {
        let (maintenance, isolated) = claims.entry(*token).or_default();
        *maintenance += state.cross_maintenance_margin_used;
        *isolated += state
            .asset_positions
            .iter()
            .filter(|position| position.position.leverage.leverage_type == LeverageType::Isolated)
            .map(|position| position.position.margin_used)
            .sum::<Decimal>();
    }
    let mut worst: Option<UnifiedRatio> = None;
    for (token, (maintenance, isolated)) in claims {
        if maintenance <= Decimal::ZERO {
            continue;
        }
        let held = usize::try_from(token).ok().and_then(|index| {
            balances
                .iter()
                .find(|balance| balance.token == Some(index))
                .map(|balance| balance.total)
        });
        let available = held.map_or(Decimal::ZERO, |held| held - isolated);
        if available <= Decimal::ZERO {
            return Some(UnifiedRisk::Uncovered {
                collateral_token: token,
            });
        }
        let ratio = maintenance / available;
        if worst.is_none_or(|current| ratio > current.ratio) {
            worst = Some(UnifiedRatio {
                ratio,
                collateral_token: token,
            });
        }
    }
    worst.map(UnifiedRisk::Ratio)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn modes_parse_once_and_keep_their_reported_names() {
        for (raw, name) in [
            ("unifiedAccount", "unified-account"),
            ("portfolioMargin", "portfolio-margin"),
            ("dexAbstraction", "dex-abstraction"),
            ("disabled", "disabled"),
            ("default", "default"),
            ("someFutureMode", "unknown"),
        ] {
            assert_eq!(AccountMode::parse(raw).as_str(), name, "{raw}");
        }
    }

    #[test]
    fn the_sdks_modes_are_the_modes_their_protocol_spelling_names() {
        for (sdk, name) in [
            (AbstractionMode::Standard, "disabled"),
            (AbstractionMode::UnifiedAccount, "unified-account"),
            (AbstractionMode::PortfolioMargin, "portfolio-margin"),
        ] {
            assert_eq!(AccountMode::from(sdk).as_str(), name, "{sdk:?}");
        }
        // `default` is a protocol value no SDK mode sets.
        assert_ne!(
            AccountMode::from(AbstractionMode::Standard),
            AccountMode::parse("default")
        );
    }

    #[test]
    fn only_unified_and_portfolio_margin_pool_collateral() {
        let pools = |raw| AccountMode::parse(raw).pools_collateral();
        assert!(pools("unifiedAccount"));
        assert!(pools("portfolioMargin"));
        for raw in ["disabled", "default", "dexAbstraction", "someFutureMode"] {
            assert!(!pools(raw), "{raw}");
        }
    }

    #[test]
    fn the_exchange_moves_dex_collateral_itself_in_pooled_and_dex_abstraction_modes() {
        let moves = |raw| AccountMode::parse(raw).moves_dex_collateral_itself();
        for raw in ["unifiedAccount", "portfolioMargin", "dexAbstraction"] {
            assert!(moves(raw), "{raw}");
        }
        for raw in ["disabled", "default", "someFutureMode"] {
            assert!(!moves(raw), "{raw}");
        }
    }

    fn state(maintenance: &str, isolated_margin: &str, cross_margin: &str) -> ClearinghouseState {
        let position = |kind: serde_json::Value, margin: &str| {
            json!({"type": "oneWay", "position": {
                "coin": "BTC", "szi": "1", "leverage": kind, "entryPx": "1",
                "positionValue": "1", "unrealizedPnl": "0", "returnOnEquity": "0",
                "liquidationPx": null, "marginUsed": margin, "maxLeverage": 50,
                "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
            }})
        };
        let summary = json!({
            "accountValue": "0", "totalNtlPos": "0", "totalRawUsd": "0", "totalMarginUsed": "0"
        });
        serde_json::from_value(json!({
            "marginSummary": summary,
            "crossMarginSummary": summary,
            "crossMaintenanceMarginUsed": maintenance,
            "withdrawable": "0",
            "assetPositions": [
                position(json!({"type": "cross", "value": 5}), cross_margin),
                position(json!({"type": "isolated", "value": 5, "rawUsd": "1"}), isolated_margin)
            ],
            "time": 0
        }))
        .unwrap()
    }

    fn balance(token: usize, total: &str) -> RawSpotBalance {
        RawSpotBalance {
            coin: format!("T{token}"),
            token: Some(token),
            hold: Decimal::ZERO,
            total: total.parse().unwrap(),
            entry_ntl: Decimal::ZERO,
        }
    }

    fn ratio_of(risk: Option<UnifiedRisk>) -> UnifiedRatio {
        match risk {
            Some(UnifiedRisk::Ratio(ratio)) => ratio,
            other => panic!("expected a ratio, got {other:?}"),
        }
    }

    #[test]
    fn the_ratio_sums_dexes_of_one_collateral_and_removes_isolated_margin_only() {
        // (500 + 400) / (1100 - 100 isolated). The cross position's 7000 of
        // margin is not removed from the balance.
        let default_dex = state("500", "100", "7000");
        let other_dex = state("400", "0", "0");

        let ratio = ratio_of(unified_account_ratio(
            &[(0, &default_dex), (0, &other_dex)],
            &[balance(0, "1100")],
        ));

        assert_eq!(ratio.ratio, Decimal::new(9, 1));
        assert_eq!(ratio.collateral_token, 0);
    }

    #[test]
    fn the_account_ratio_is_the_worst_collateral_token() {
        let usdc_dex = state("90", "0", "0");
        let usdh_dex = state("95", "0", "0");
        let idle_dex = state("0", "0", "0");

        let ratio = ratio_of(unified_account_ratio(
            &[(0, &usdc_dex), (360, &usdh_dex), (7, &idle_dex)],
            &[balance(0, "100"), balance(360, "100")],
        ));

        assert_eq!(ratio.ratio, Decimal::new(95, 2));
        assert_eq!(ratio.collateral_token, 360);
    }

    #[test]
    fn maintenance_margin_with_nothing_to_cover_it_is_uncovered_not_silent() {
        let dex = state("10", "0", "0");
        let swallowed = state("1", "60", "0");
        let covered = state("99", "0", "0");

        for (dexes, balances, token) in [
            (vec![(0, &dex)], vec![], 0),
            (vec![(0, &dex)], vec![balance(0, "0")], 0),
            (vec![(8, &swallowed)], vec![balance(8, "60")], 8),
            // Uncovered outranks a token at 99%.
            (
                vec![(0, &covered), (8, &swallowed)],
                vec![balance(0, "100"), balance(8, "60")],
                8,
            ),
        ] {
            assert_eq!(
                unified_account_ratio(&dexes, &balances),
                Some(UnifiedRisk::Uncovered {
                    collateral_token: token
                })
            );
        }
    }

    #[test]
    fn no_maintenance_margin_means_nothing_at_risk() {
        let idle = state("0", "0", "0");
        assert_eq!(unified_account_ratio(&[(0, &idle)], &[]), None);
    }
}
