//! A position as more than one command shows it, and the one lookup of an
//! account's position in a market.
//!
//! Layer rule: below the commands that print positions (`positions list`,
//! `account portfolio`, `vault positions`, `watch risk`) and the orders that
//! read one (`orders tpsl`, `positions update-margin`); none of them reaches
//! into another command for it.

use std::fmt;

use hypersdk::hypercore::types::{AssetPosition, LeverageType, PositionData};
use hypersdk::{Address, Decimal};
use serde::{Serialize, Serializer};

use crate::assets::PerpAsset;
use crate::commands::common::rows::{ListRow, MISSING, Row, or_missing, style_column};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::output;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Leverage {
    pub kind: LeverageType,
    pub value: u32,
}

impl fmt::Display for Leverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}x", self.kind, self.value)
    }
}

impl Serialize for Leverage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The mark price a position implies: its value per unit of size.
pub(crate) fn mark_price(position: &PositionData) -> Decimal {
    let size = position.abs_size();
    if size.is_zero() {
        Decimal::ZERO
    } else {
        position.position_value / size
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PositionRow {
    pub coin: String,
    pub side: &'static str,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub entry: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub mark: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub unrealized_pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub position_value: Decimal,
    pub leverage: Leverage,
    #[serde(with = "rust_decimal::serde::str")]
    pub margin_used: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub liquidation_price: Option<Decimal>,
}

impl From<AssetPosition> for PositionRow {
    fn from(value: AssetPosition) -> Self {
        let position = value.position;
        Self {
            mark: mark_price(&position),
            side: position.side(),
            leverage: Leverage {
                kind: position.leverage.leverage_type,
                value: position.leverage.value,
            },
            coin: position.coin,
            size: position.szi,
            entry: position.entry_px,
            unrealized_pnl: position.unrealized_pnl,
            position_value: position.position_value,
            margin_used: position.margin_used,
            liquidation_price: position.liquidation_px,
        }
    }
}

impl PositionRow {
    pub(crate) fn open(positions: Vec<AssetPosition>) -> Vec<Self> {
        positions
            .into_iter()
            .map(Self::from)
            .filter(|row| !row.size.is_zero())
            .collect()
    }
}

impl Row for PositionRow {
    const HEADERS: &'static [&'static str] = &[
        "Coin",
        "Side",
        "Size",
        "Entry",
        "Mark",
        "Unrealized PnL",
        "Position Value",
        "Leverage",
        "Margin Used",
        "Liquidation",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.side.to_string(),
            self.size.to_string(),
            or_missing(self.entry),
            self.mark.to_string(),
            self.unrealized_pnl.to_string(),
            self.position_value.to_string(),
            self.leverage.to_string(),
            self.margin_used.to_string(),
            or_missing(self.liquidation_price),
        ]
    }

    fn pretty_cells(&self) -> Vec<String> {
        let mut cells = self.cells();
        style_column::<Self>(&mut cells, "Unrealized PnL", |_| {
            pretty_pnl(self.unrealized_pnl)
        });
        for cell in &mut cells {
            if cell == MISSING {
                *cell = output::colors::gray(cell);
            }
        }
        cells
    }
}

impl ListRow for PositionRow {
    const EMPTY: &'static str = "No open positions.";
}

pub(crate) fn pretty_pnl(value: Decimal) -> String {
    let formatted = value.to_string();
    if value > Decimal::ZERO {
        output::colors::green(&formatted)
    } else if value < Decimal::ZERO {
        output::colors::red(&formatted)
    } else {
        formatted
    }
}

/// A position the account closed is still listed by the exchange with a zero size:
/// it is returned only when `include_flat`.
pub(crate) async fn find_position(
    client: &ApiClient,
    account: Address,
    perp: &PerpAsset,
    include_flat: bool,
) -> Result<Option<AssetPosition>, CliError> {
    let coin = perp.display_name();
    Ok(client
        .sdk()
        .clearinghouse_state(account, perp.dex.clone())
        .await?
        .asset_positions
        .into_iter()
        .find(|position| {
            position.position.coin.eq_ignore_ascii_case(&coin)
                && (include_flat || !position.position.szi.is_zero())
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(szi: &str, pnl: &str) -> AssetPosition {
        serde_json::from_value::<AssetPosition>(serde_json::json!({
            "type": "oneWay",
            "position": {
                "coin": "BTC",
                "szi": szi,
                "leverage": {"type": "cross", "value": 5},
                "entryPx": "50000",
                "positionValue": "5100",
                "unrealizedPnl": pnl,
                "returnOnEquity": "0.1",
                "liquidationPx": null,
                "marginUsed": "1000",
                "maxLeverage": 50,
                "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn position_rows_compute_mark_from_value_and_abs_size() {
        let row = PositionRow::from(position("0.1", "100"));

        assert_eq!(row.mark, Decimal::from(51_000));
        assert_eq!(row.side, "long");
        assert_eq!(row.leverage.to_string(), "cross 5x");

        let short = PositionRow::from(position("-0.1", "-20"));
        assert_eq!(short.mark, Decimal::from(51_000));
        assert_eq!(short.side, "short");
    }

    #[test]
    fn closed_positions_are_not_listed() {
        let rows = PositionRow::open(vec![position("0", "0"), position("0.1", "100")]);

        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn position_json_keeps_decimal_strings_and_null_liquidation() {
        let json = serde_json::to_value(PositionRow::from(position("0.1", "100"))).unwrap();

        assert_eq!(json["entry"], "50000");
        assert_eq!(json["mark"], "51000");
        assert_eq!(json["leverage"], "cross 5x");
        assert!(json["liquidation_price"].is_null());
    }
}
