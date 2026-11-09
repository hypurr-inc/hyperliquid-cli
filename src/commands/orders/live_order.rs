//! A resting order that chase or a bracket entry still owns.
//!
//! Matching is by OID or cloid only, so a poll never treats some other order
//! on the same market as the one this command placed.

use std::time::{Duration, Instant};

use hypersdk::hypercore::types::{
    Action, BasicOrder, BatchCancel, BatchCancelCloid, Cancel, CancelByCloid,
};
use hypersdk::hypercore::{Cloid, HttpClient};
use hypersdk::{Address, Decimal};

use super::{OrderSubmissionContext, parse_cloid};
use crate::commands::actions;
use crate::errors::{CliError, from_sdk};

pub(crate) struct LiveOrder {
    pub oid: Option<u64>,
    pub cloid: Cloid,
    pub asset: u32,
    pub open_size: Decimal,
}

/// Default maximum time to track a resting order or ALO chase quote (5 minutes).
pub(crate) const DEFAULT_TRACKING_TIMEOUT: Duration = Duration::from_secs(300);

/// Unique client order ID for an order a command tracks (a chase quote, a
/// bracket entry): nonce-derived high bits plus a per-run sequence.
pub(crate) fn generated_cloid(seq: u64) -> Result<Cloid, CliError> {
    parse_cloid(&format!("0x{:016x}{:016x}", actions::nonce_now(), seq))
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(crate) async fn send_tracked_action(
    context: OrderSubmissionContext<'_>,
    action: Action,
    vault_address: Option<Address>,
    error_label: &'static str,
) -> Result<serde_json::Value, CliError> {
    actions::send_l1_action_raw(
        context.api,
        context.signer,
        action,
        actions::nonce_now(),
        vault_address,
        error_label,
    )
    .await
}

impl LiveOrder {
    /// Size still open for this order. Zero means it filled or was cancelled.
    pub(crate) fn open_size_in(&self, orders: &[BasicOrder]) -> Decimal {
        orders
            .iter()
            .filter(|order| {
                self.oid.is_some_and(|oid| order.oid == oid)
                    || order.cloid.is_some_and(|cloid| cloid == self.cloid)
            })
            .map(|order| order.sz)
            .fold(Decimal::ZERO, |total, size| total + size)
    }

    /// [`LiveOrder::open_size_in`] over `user`'s open orders on `dex`, the dex
    /// that lists the order's market. Another dex's list never holds the
    /// order, so reading it would report the order as gone.
    pub(crate) async fn open_size_now(
        &self,
        client: &HttpClient,
        user: Address,
        dex: Option<String>,
    ) -> Result<Decimal, CliError> {
        let open = client.open_orders(user, dex).await.map_err(from_sdk)?;
        Ok(self.open_size_in(&open))
    }

    /// Cancel by cloid, even when an oid is known. Chase uses this so a
    /// requote never cancels a different order that reused an oid.
    pub(crate) fn cancel_by_cloid(&self) -> Action {
        Action::CancelByCloid(BatchCancelCloid {
            cancels: vec![CancelByCloid {
                asset: self.asset,
                cloid: self.cloid,
            }],
            fast: false,
        })
    }

    pub(crate) fn cancel_prefer_oid(&self) -> Action {
        match self.oid {
            Some(oid) => Action::Cancel(BatchCancel {
                cancels: vec![Cancel {
                    asset: self.asset as usize,
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

    #[test]
    fn open_size_matches_only_the_tracked_order() {
        let cloid = "0x00000000000000000000000000000001"
            .parse::<Cloid>()
            .unwrap();
        let tracked = LiveOrder {
            oid: Some(42),
            cloid,
            asset: 0,
            open_size: Decimal::ONE,
        };
        let orders = vec![
            order(42, Some(cloid), Decimal::new(3, 1)),
            order(99, None, Decimal::from(5)),
        ];
        assert_eq!(tracked.open_size_in(&orders), Decimal::new(3, 1));
    }
}
