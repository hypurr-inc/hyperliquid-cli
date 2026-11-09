//! A resting order that chase or a bracket entry still owns.
//!
//! Matching is by OID or cloid only, so a poll never treats some other order
//! on the same market as the one this command placed.

use std::time::Instant;

use hypersdk::hypercore::types::{
    Action, BasicOrder, BatchCancel, BatchCancelCloid, Cancel, CancelByCloid,
};
use hypersdk::hypercore::{Cloid, HttpClient};
use hypersdk::{Address, Decimal};

use super::context::OrderSubmissionContext;
use crate::commands::actions;
use crate::errors::{CliError, from_sdk};

/// A protocol asset index as a cancel-by-cloid request carries it: 32 bits,
/// where an order request carries it as a `usize`. Built before an order is
/// placed, so an order that could not be cancelled by cloid is never placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CloidAsset(u32);

impl CloidAsset {
    pub(super) fn new(index: usize) -> Result<Self, CliError> {
        u32::try_from(index).map(Self).map_err(|_| {
            CliError::Internal(anyhow::anyhow!(
                "asset index {index} does not fit a cancel-by-cloid request"
            ))
        })
    }

    pub(super) fn index(self) -> u32 {
        self.0
    }
}

pub(super) struct LiveOrder {
    pub oid: Option<u64>,
    pub cloid: Cloid,
    pub asset: CloidAsset,
    /// The size still open, as of the last read.
    pub open_size: Decimal,
}

pub(super) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(super) async fn send_tracked_action(
    context: OrderSubmissionContext<'_>,
    action: Action,
    error_label: &'static str,
) -> Result<serde_json::Value, CliError> {
    actions::send_l1_action(
        context.api,
        context.signer,
        action,
        context.vault_address,
        error_label,
    )
    .await
}

impl LiveOrder {
    /// Size still open for this order. Zero means it filled or was cancelled.
    pub(super) fn open_size_in(&self, orders: &[BasicOrder]) -> Decimal {
        orders
            .iter()
            .filter(|order| {
                self.oid.is_some_and(|oid| order.oid == oid)
                    || order.cloid.is_some_and(|cloid| cloid == self.cloid)
            })
            .map(|order| order.sz)
            .fold(Decimal::ZERO, |total, size| total + size)
    }

    /// Record that `open_now` of the order is still open and return the size
    /// that left the book since the last read. An order that is gone left with
    /// everything it had open: a fill is the only way a command's own resting
    /// order disappears.
    pub(super) fn settle(&mut self, open_now: Decimal) -> Decimal {
        let open_now = open_now.max(Decimal::ZERO).min(self.open_size);
        let left = self.open_size - open_now;
        self.open_size = open_now;
        left
    }

    pub(super) fn is_gone(&self) -> bool {
        self.open_size <= Decimal::ZERO
    }

    /// Re-read the order from `user`'s open orders on `dex`, the dex that lists
    /// the order's market (another dex's list never holds the order, so reading
    /// it would report the order as gone), and [`settle`](Self::settle) it.
    /// Returns the size that left the book since the last read.
    pub(super) async fn refresh(
        &mut self,
        client: &HttpClient,
        user: Address,
        dex: Option<String>,
    ) -> Result<Decimal, CliError> {
        let open = client.open_orders(user, dex).await.map_err(from_sdk)?;
        Ok(self.settle(self.open_size_in(&open)))
    }

    /// Cancel by cloid, even when an oid is known. Chase uses this so a
    /// requote never cancels a different order that reused an oid.
    pub(super) fn cancel_by_cloid(&self) -> Action {
        Action::CancelByCloid(BatchCancelCloid {
            cancels: vec![CancelByCloid {
                asset: self.asset.index(),
                cloid: self.cloid,
            }],
            fast: false,
        })
    }

    pub(super) fn cancel_prefer_oid(&self) -> Action {
        match self.oid {
            Some(oid) => Action::Cancel(BatchCancel {
                cancels: vec![Cancel {
                    asset: self.asset.index() as usize,
                    oid,
                }],
                fast: false,
            }),
            None => self.cancel_by_cloid(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersdk::hypercore::types::{OrderType, Side};

    fn order(oid: u64, cloid: Option<Cloid>, sz: Decimal) -> BasicOrder {
        BasicOrder {
            timestamp: 0,
            coin: "ETH".to_string(),
            side: Side::Bid,
            limit_px: Decimal::from(100),
            sz,
            oid,
            orig_sz: sz,
            cloid,
            order_type: OrderType::Limit,
            tif: None,
            reduce_only: false,
            is_trigger: None,
            trigger_px: None,
            trigger_condition: None,
            is_position_tpsl: None,
        }
    }

    fn tracked(open_size: Decimal) -> LiveOrder {
        LiveOrder {
            oid: Some(42),
            cloid: "0x00000000000000000000000000000001"
                .parse::<Cloid>()
                .unwrap(),
            asset: CloidAsset::new(0).unwrap(),
            open_size,
        }
    }

    #[test]
    fn open_size_matches_only_the_tracked_order() {
        let tracked = tracked(Decimal::ONE);
        let orders = vec![
            order(42, Some(tracked.cloid), Decimal::new(3, 1)),
            order(99, None, Decimal::from(5)),
        ];
        assert_eq!(tracked.open_size_in(&orders), Decimal::new(3, 1));
    }

    #[test]
    fn settling_reports_what_left_the_book_and_counts_a_vanished_order_as_filled() {
        let mut order = tracked(Decimal::ONE);
        assert_eq!(order.settle(Decimal::new(6, 1)), Decimal::new(4, 1));
        assert_eq!(order.open_size, Decimal::new(6, 1));
        assert!(!order.is_gone());

        // Nothing changed: nothing left.
        assert_eq!(order.settle(Decimal::new(6, 1)), Decimal::ZERO);

        // Gone: the remaining 0.6 left the book too.
        assert_eq!(order.settle(Decimal::ZERO), Decimal::new(6, 1));
        assert!(order.is_gone());
    }

    #[test]
    fn an_order_that_vanishes_before_any_partial_fill_left_with_its_whole_size() {
        let mut order = tracked(Decimal::from(20));
        assert_eq!(order.settle(Decimal::ZERO), Decimal::from(20));
    }

    #[test]
    fn a_read_never_grows_the_open_size() {
        let mut order = tracked(Decimal::ONE);
        assert_eq!(order.settle(Decimal::from(2)), Decimal::ZERO);
        assert_eq!(order.open_size, Decimal::ONE);
    }

    #[test]
    fn an_asset_that_does_not_fit_a_cloid_cancel_is_refused_up_front() {
        assert_eq!(CloidAsset::new(110_035).unwrap().index(), 110_035);
        let err = CloidAsset::new(usize::try_from(u32::MAX).unwrap() + 1).unwrap_err();
        assert!(err.to_string().contains("cancel-by-cloid"), "{err}");
    }
}
