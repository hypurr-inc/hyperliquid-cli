//! What planning produces and later stages read: the orders to send, with the
//! facts a confirmation prompt, a dry-run preview and a result row need.

use std::fmt;

use hypersdk::Decimal;
use hypersdk::hypercore::types::{BatchOrder, OrderRequest, TpSl};

use super::args::{CreateOrderType, MarginModeArg, OrderSide, TifArg, TpslGroupingArg};
use super::exchange::OrderBuilderFee;
use crate::assets::{Market, PerpAsset};

/// A quote-denominated order size, as the user gave it with `--amount`.
#[derive(Debug, Clone)]
pub(super) struct QuoteAmount {
    pub(super) value: Decimal,
    /// The quote asset the amount is in, such as `USDC`.
    pub(super) unit: String,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedOrder {
    pub(super) request: OrderRequest,
    pub(super) market: Market,
    pub(super) coin: String,
    pub(super) side: OrderSide,
    pub(super) order_type: CreateOrderType,
    pub(super) tif: Option<TifArg>,
    pub(super) trigger_price: Option<Decimal>,
    pub(super) amount: Option<QuoteAmount>,
    pub(super) warning: Option<String>,
    pub(super) builder: Option<OrderBuilderFee>,
    /// The margin mode the account's position must already use.
    pub(super) margin_mode: Option<MarginModeArg>,
}

impl PreparedOrder {
    pub(super) fn perp(&self) -> Option<&PerpAsset> {
        match &self.market {
            Market::Perp(perp) => Some(perp),
            Market::Spot(_) | Market::Outcome(_) => None,
        }
    }

    pub(super) fn price(&self) -> Decimal {
        self.request.limit_px
    }

    pub(super) fn size(&self) -> Decimal {
        self.request.sz
    }

    pub(super) fn notional(&self) -> Decimal {
        self.size() * self.price()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TpslLegKind {
    Parent,
    TakeProfit,
    StopLoss,
}

impl TpslLegKind {
    pub(super) fn of(tpsl: TpSl) -> Self {
        match tpsl {
            TpSl::Tp => Self::TakeProfit,
            TpSl::Sl => Self::StopLoss,
        }
    }
}

impl fmt::Display for TpslLegKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parent => write!(f, "parent"),
            Self::TakeProfit => write!(f, "take_profit"),
            Self::StopLoss => write!(f, "stop_loss"),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct PreparedTpslLeg {
    pub(super) request: OrderRequest,
    pub(super) leg: TpslLegKind,
    pub(super) coin: String,
    pub(super) side: OrderSide,
    pub(super) order_type: CreateOrderType,
    pub(super) price: Decimal,
    pub(super) size: Decimal,
    pub(super) tif: Option<TifArg>,
    pub(super) reduce_only: bool,
    pub(super) warning: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedTpslBatch {
    pub(super) batch: BatchOrder,
    pub(super) grouping: TpslGroupingArg,
    pub(super) legs: Vec<PreparedTpslLeg>,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedOrderBatch {
    pub(super) batch: BatchOrder,
    pub(super) legs: Vec<PreparedOrder>,
}

pub(super) enum CreateOrderSubmission {
    Single(PreparedOrder),
    NormalTpsl(NormalTpslOrder),
}

pub(super) struct NormalTpslOrder {
    pub(super) parent: PreparedOrder,
    pub(super) batch: PreparedTpslBatch,
}

impl CreateOrderSubmission {
    /// The entry order, which sizes the HIP-3 margin helper and names the market.
    pub(super) fn parent(&self) -> &PreparedOrder {
        match self {
            Self::Single(prepared) => prepared,
            Self::NormalTpsl(order) => &order.parent,
        }
    }
}
