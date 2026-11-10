//! What planning produces and later stages read: the orders to send, with the
//! facts a confirmation prompt, a dry-run preview and a result row need.

use std::fmt;

use hypersdk::Decimal;
use hypersdk::hypercore::types::{BatchOrder, OrderGrouping, OrderRequest, TpSl};

use super::vocab::{CreateOrderType, OrderSide, TifArg, TpslGroupingArg};
use crate::assets::{Market, PerpAsset};
use crate::builder_fee::BuilderFee;
use crate::margin::MarginPlan;

/// The grouping an order action with no TP/SL carries: none, or the order
/// write priority `rate` when one is paid.
pub(super) fn priority_grouping(rate: Option<u32>) -> OrderGrouping {
    rate.map_or(OrderGrouping::Na, OrderGrouping::PriorityRate)
}

/// The one place an order action is assembled. The SDK's `builder` field is
/// always empty: it carries only `{ b, f }`, which cannot express
/// sub-builders, so a builder fee goes in the raw action the submit path
/// builds from the order's [`BuilderFee`].
pub(super) fn order_batch(orders: Vec<OrderRequest>, grouping: OrderGrouping) -> BatchOrder {
    BatchOrder {
        orders,
        grouping,
        builder: None,
    }
}

#[derive(Debug, Clone)]
pub(super) struct QuoteAmount {
    pub(super) value: Decimal,
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
    pub(super) builder: Option<BuilderFee>,
    /// The margin mode of a perpetual order: what the user asked for, what
    /// the market's rule implies, and what the account is set to. `None` on
    /// a market with no margin.
    pub(super) margin: Option<MarginPlan>,
    pub(super) priority_rate: Option<u32>,
}

impl PreparedOrder {
    pub(super) fn perp(&self) -> Option<&PerpAsset> {
        match &self.market {
            Market::Perp(perp) => Some(perp),
            Market::Spot(_) | Market::Outcome(_) => None,
        }
    }

    pub(super) fn batch(&self) -> BatchOrder {
        order_batch(
            vec![self.request.clone()],
            priority_grouping(self.priority_rate),
        )
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

impl PreparedOrderBatch {
    /// The market and margin plan the batch is checked and previewed against.
    ///
    /// The first leg speaks for all, and that is sound only because of how a
    /// batch is made: `orders scale` plans every leg on one market with the
    /// one `--margin-mode`, and `orders batch-create` legs carry no
    /// `--margin-mode` (the orders file has no such field), so on any mix of
    /// markets its plans request nothing and nothing is checked.
    pub(super) fn margin_check(&self) -> Option<(&PerpAsset, &MarginPlan)> {
        let leg = self.legs.first()?;
        leg.perp().zip(leg.margin.as_ref())
    }
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

#[cfg(test)]
mod tests {
    use hypersdk::hypercore::types::{OrderTypePlacement, TimeInForce};

    use super::*;
    use crate::assets::MarginRule;
    use crate::assets::test_support::{hip3_perp, perp};
    use crate::margin::MarginModeArg;

    fn order(perp: PerpAsset, requested: Option<MarginModeArg>) -> PreparedOrder {
        PreparedOrder {
            request: OrderRequest {
                asset: perp.index,
                is_buy: true,
                limit_px: Decimal::from(10),
                sz: Decimal::ONE,
                reduce_only: false,
                order_type: OrderTypePlacement::Limit {
                    tif: TimeInForce::Gtc,
                },
                cloid: Default::default(),
            },
            coin: perp.display_name(),
            margin: Some(MarginPlan::for_market(&perp, requested).unwrap()),
            market: Market::Perp(perp),
            side: OrderSide::Buy,
            order_type: CreateOrderType::Limit,
            tif: None,
            trigger_price: None,
            amount: None,
            warning: None,
            builder: None,
            priority_rate: None,
        }
    }

    fn batch(legs: Vec<PreparedOrder>) -> PreparedOrderBatch {
        PreparedOrderBatch {
            batch: order_batch(Vec::new(), OrderGrouping::Na),
            legs,
        }
    }

    #[test]
    fn a_batch_is_checked_against_its_first_legs_market_and_mode() {
        let scale = batch(vec![
            order(perp("BTC", 0), Some(MarginModeArg::Isolated)),
            order(perp("BTC", 0), Some(MarginModeArg::Isolated)),
        ]);
        let (market, margin) = scale.margin_check().unwrap();
        assert_eq!(market.display_name(), "BTC");
        assert_eq!(
            margin.required_account_mode(),
            Some(MarginModeArg::Isolated)
        );
        assert!(batch(Vec::new()).margin_check().is_none());
    }

    #[test]
    fn a_batch_of_legs_that_request_no_mode_requires_none_on_any_mix_of_markets() {
        let mut isolated_only = hip3_perp("xyz", "HOOD", 110_000);
        isolated_only.margin_rule = MarginRule::StrictIsolated;
        let mixed = batch(vec![
            order(perp("BTC", 0), None),
            order(isolated_only, None),
            order(hip3_perp("xyz", "OPEN", 110_001), None),
        ]);
        let (_, margin) = mixed.margin_check().unwrap();
        assert_eq!(margin.required_account_mode(), None);
    }

    #[test]
    fn an_order_is_its_own_action_with_no_sdk_builder_field() {
        let mut prepared = order(perp("BTC", 0), None);
        assert_eq!(prepared.batch().orders.len(), 1);
        assert!(matches!(prepared.batch().grouping, OrderGrouping::Na));
        assert!(prepared.batch().builder.is_none());
        prepared.priority_rate = Some(80_000);
        assert!(matches!(
            prepared.batch().grouping,
            OrderGrouping::PriorityRate(80_000)
        ));
    }
}
