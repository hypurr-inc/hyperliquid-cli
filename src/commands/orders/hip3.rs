//! The helper transfer that funds a HIP-3 dex before an order draws on it.

use std::collections::BTreeMap;

use hypersdk::{Address, Decimal};
use serde_json::json;

use super::args::BPS_DENOMINATOR;
use super::prepared::PreparedOrder;
use crate::account_mode::fetch_abstraction;
use crate::errors::from_sdk;
use crate::exchange::send_asset::{self, AssetMove, AssetVenue};
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::signing::SelectedSigner;

/// The leverage an order is assumed to run at when sizing its margin.
const DEFAULT_HIP3_LEVERAGE: i64 = 10;
/// The margin kept, in basis points of the estimate: 12,000 is 120% of it.
const MARGIN_BUFFER_BPS: i64 = 12_000;

#[derive(Debug, Clone)]
pub(super) struct Hip3MarginPlan {
    pub(super) dest_dex: String,
    pub(super) token: String,
    pub(super) amount: Decimal,
}

impl Hip3MarginPlan {
    fn dest(&self) -> String {
        format!("dex:{}", self.dest_dex)
    }

    pub(super) fn to_json(&self) -> serde_json::Value {
        json!({
            "action": "send_asset",
            "source": "perp",
            "dest": self.dest(),
            "token": self.token,
            "amount": self.amount.normalize().to_string(),
        })
    }
}

/// The transfers that top up each HIP-3 dex `orders` trade on, one per dex
/// that needs one. Orders on the default dex, or on an account whose exchange moves
/// dex collateral itself, need none.
pub(super) async fn plan_hip3_margin_transfers(
    api: &ApiClient,
    user: Address,
    orders: &[&PreparedOrder],
) -> Result<Vec<Hip3MarginPlan>, anyhow::Error> {
    let by_dex = exposure_by_dex(orders);
    if by_dex.is_empty() {
        return Ok(Vec::new());
    }

    let abstraction = fetch_abstraction(api, user).await?;
    if abstraction.normalized_mode.moves_dex_collateral_itself() {
        return Ok(Vec::new());
    }

    let mut plans = Vec::new();
    for (dest_dex, (token, notional)) in by_dex {
        if let Some(plan) = plan_dex_transfer(api, user, dest_dex, token, notional).await? {
            plans.push(plan);
        }
    }
    Ok(plans)
}

/// What the orders on each HIP-3 dex draw on: the dex's collateral token and
/// the total notional of the orders placed there. Orders on the default dex are
/// not listed.
fn exposure_by_dex<'a>(orders: &[&'a PreparedOrder]) -> BTreeMap<&'a str, (&'a str, Decimal)> {
    let mut by_dex: BTreeMap<&str, (&str, Decimal)> = BTreeMap::new();
    for order in orders {
        if let Some(perp) = order.perp()
            && let Some(dex) = perp.dex.as_deref().filter(|dex| !dex.is_empty())
        {
            by_dex
                .entry(dex)
                .or_insert((perp.collateral.as_str(), Decimal::ZERO))
                .1 += order.notional();
        }
    }
    by_dex
}

async fn plan_dex_transfer(
    api: &ApiClient,
    user: Address,
    dest_dex: &str,
    token: &str,
    notional: Decimal,
) -> Result<Option<Hip3MarginPlan>, anyhow::Error> {
    let required = required_margin(notional);
    if required <= Decimal::ZERO {
        return Ok(None);
    }

    let dest_available = api
        .sdk()
        .clearinghouse_state(user, Some(dest_dex.to_string()))
        .await
        .map_err(from_sdk)?
        .withdrawable;
    if dest_available >= required {
        return Ok(None);
    }

    Ok(Some(Hip3MarginPlan {
        dest_dex: dest_dex.to_string(),
        token: token.to_string(),
        amount: required - dest_available,
    }))
}

/// The margin to hold on a dex for orders worth `notional`, to the cent.
fn required_margin(notional: Decimal) -> Decimal {
    (notional / Decimal::from(DEFAULT_HIP3_LEVERAGE) * Decimal::from(MARGIN_BUFFER_BPS)
        / Decimal::from(BPS_DENOMINATOR))
    .round_dp(2)
}

pub(super) async fn execute_hip3_margin_transfers(
    inv: &Invocation,
    signer: &SelectedSigner,
    user: Address,
    plans: &[Hip3MarginPlan],
) -> Result<(), anyhow::Error> {
    for plan in plans {
        send_asset::send_asset(
            inv.remote()?.api(),
            signer,
            AssetMove {
                to: user,
                source: AssetVenue::perp(),
                dest: AssetVenue::dex(&plan.dest_dex),
                token: plan.token.clone(),
                amount: plan.amount,
                from_subaccount: None,
            },
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use hypersdk::hypercore::types::{OrderRequest, OrderTypePlacement, TimeInForce};

    use super::*;
    use crate::assets::test_support::{hip3_perp, perp};
    use crate::assets::{Market, PerpAsset};
    use crate::commands::orders::args::{CreateOrderType, OrderSide};

    fn order(perp: PerpAsset, price: i64, size: i64) -> PreparedOrder {
        PreparedOrder {
            request: OrderRequest {
                asset: perp.index,
                is_buy: true,
                limit_px: Decimal::from(price),
                sz: Decimal::from(size),
                reduce_only: false,
                order_type: OrderTypePlacement::Limit {
                    tif: TimeInForce::Gtc,
                },
                cloid: Default::default(),
            },
            coin: perp.display_name(),
            market: Market::Perp(perp),
            side: OrderSide::Buy,
            order_type: CreateOrderType::Limit,
            tif: None,
            trigger_price: None,
            amount: None,
            warning: None,
            builder: None,
            margin: None,
            priority_rate: None,
        }
    }

    #[test]
    fn required_margin_is_a_tenth_of_the_notional_plus_a_fifth() {
        assert_eq!(required_margin(Decimal::from(1_000)), Decimal::from(120));
        assert_eq!(
            required_margin(Decimal::new(12_345, 2)),
            Decimal::new(1_481, 2)
        );
        assert_eq!(required_margin(Decimal::ZERO), Decimal::ZERO);
    }

    #[test]
    fn exposure_sums_the_notional_per_dex_and_skips_the_default_dex() {
        let mut usdh = hip3_perp("abc", "C", 120_002);
        usdh.collateral = "USDH".to_string();
        let orders = [
            order(hip3_perp("xyz", "A", 110_000), 2, 10),
            order(hip3_perp("xyz", "B", 110_001), 3, 10),
            order(usdh, 5, 4),
            order(perp("BTC", 0), 50_000, 1),
        ];
        let refs: Vec<&PreparedOrder> = orders.iter().collect();
        let by_dex = exposure_by_dex(&refs);
        assert_eq!(by_dex.len(), 2);
        assert_eq!(by_dex["xyz"], ("USDC", Decimal::from(50)));
        assert_eq!(by_dex["abc"], ("USDH", Decimal::from(20)));
    }
}
