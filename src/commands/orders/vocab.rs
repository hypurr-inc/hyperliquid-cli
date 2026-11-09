//! The vocabulary the order commands share: the sides, order types,
//! time-in-force and TP/SL groupings they speak in, and the basis-point
//! bounds on a market order. The clap argument structs in `args` use it; the
//! planning, validation and rendering code depend on it rather than on them.

use std::fmt;

use clap::ValueEnum;
use hypersdk::hypercore::types::{OrderGrouping, TimeInForce, TpSl};
use serde::{Deserialize, Serialize};

pub(super) const DEFAULT_MARKET_ORDER_SLIPPAGE_BPS: u16 = 500;
pub(super) const MIN_MARKET_ORDER_SLIPPAGE_BPS: u16 = 1;
pub(super) const MAX_MARKET_ORDER_SLIPPAGE_BPS: u16 = 1_000;

pub(super) const BPS_DENOMINATOR: i64 = 10_000;

/// Buy/sell order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub(super) fn is_buy(self) -> bool {
        matches!(self, Self::Buy)
    }

    pub(super) fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

impl fmt::Display for OrderSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buy => write!(f, "buy"),
            Self::Sell => write!(f, "sell"),
        }
    }
}

/// Supported `orders create --type` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CreateOrderType {
    Limit,
    Market,
    StopLoss,
    TakeProfit,
    StopLimit,
    TakeLimit,
}

impl fmt::Display for CreateOrderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit => write!(f, "limit"),
            Self::Market => write!(f, "market"),
            Self::StopLoss => write!(f, "stop-loss"),
            Self::TakeProfit => write!(f, "take-profit"),
            Self::StopLimit => write!(f, "stop-limit"),
            Self::TakeLimit => write!(f, "take-limit"),
        }
    }
}

impl CreateOrderType {
    pub(super) fn is_trigger(self) -> bool {
        matches!(
            self,
            Self::StopLoss | Self::TakeProfit | Self::StopLimit | Self::TakeLimit
        )
    }

    pub(super) fn market_trigger(tpsl: TpSl) -> Self {
        match tpsl {
            TpSl::Tp => Self::TakeProfit,
            TpSl::Sl => Self::StopLoss,
        }
    }

    pub(super) fn limit_trigger(tpsl: TpSl) -> Self {
        match tpsl {
            TpSl::Tp => Self::TakeLimit,
            TpSl::Sl => Self::StopLimit,
        }
    }
}

/// CLI-facing TIF values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TifArg {
    Gtc,
    Alo,
    Ioc,
}

impl From<TifArg> for TimeInForce {
    fn from(value: TifArg) -> Self {
        match value {
            TifArg::Gtc => Self::Gtc,
            TifArg::Alo => Self::Alo,
            TifArg::Ioc => Self::Ioc,
        }
    }
}

impl fmt::Display for TifArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gtc => write!(f, "gtc"),
            Self::Alo => write!(f, "alo"),
            Self::Ioc => write!(f, "ioc"),
        }
    }
}

/// How an order batch groups its TP/SL legs. The CLI enums in `args` are the
/// values each command accepts; this is the one place that names a grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpslGroupingArg {
    NormalTpsl,
    PositionTpsl,
}

impl From<TpslGroupingArg> for OrderGrouping {
    fn from(value: TpslGroupingArg) -> Self {
        match value {
            TpslGroupingArg::NormalTpsl => Self::NormalTpsl,
            TpslGroupingArg::PositionTpsl => Self::PositionTpsl,
        }
    }
}

impl TpslGroupingArg {
    /// The grouping as it goes on the wire, spelled by the SDK's own
    /// serialization so the previews cannot drift from what is sent.
    pub(super) fn wire_value(self) -> serde_json::Value {
        serde_json::json!(OrderGrouping::from(self))
    }
}

impl fmt::Display for TpslGroupingArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NormalTpsl => write!(f, "normal-tpsl"),
            Self::PositionTpsl => write!(f, "position-tpsl"),
        }
    }
}
