//! Where an account's USDC lives, and whether it covers a spend.

use hypersdk::{Address, Decimal};

use crate::account_mode::fetch_abstraction;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::spot_balances::user_spot_balances_raw;

/// USDC is spot token 0 on every network.
const USDC_TOKEN_INDEX: usize = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pool {
    /// The spot balance, whatever the account's abstraction.
    Spot,
    /// What the account can spend: a standard account spends from its perp
    /// balance; a pooled one (unified or portfolio margin) keeps every balance
    /// in the spot state, where the perp `withdrawable` is not the real balance.
    Spendable,
}

/// The USDC `user` can draw from `pool`, with the name that balance goes by in
/// a refusal.
async fn available_usdc(
    api: &ApiClient,
    user: Address,
    pool: Pool,
) -> Result<(Decimal, &'static str), CliError> {
    if pool == Pool::Spot
        || fetch_abstraction(api, user)
            .await?
            .normalized_mode
            .pools_collateral()
    {
        let spot = user_spot_balances_raw(api, user)
            .await?
            .into_iter()
            .find(|balance| balance.token == Some(USDC_TOKEN_INDEX))
            .map(|balance| balance.total - balance.hold)
            .unwrap_or_default();
        return Ok((spot, "spot USDC"));
    }
    let withdrawable = api
        .sdk()
        .clearinghouse_state(user, None)
        .await?
        .withdrawable;
    Ok((withdrawable, "perp withdrawable"))
}

pub(crate) async fn ensure_usdc(
    api: &ApiClient,
    user: Address,
    pool: Pool,
    amount: Decimal,
) -> Result<(), CliError> {
    let (available, label) = available_usdc(api, user, pool).await?;
    if available < amount {
        return Err(CliError::Unsupported(format!(
            "insufficient balance: requested {amount} USDC, available {label} {available}"
        )));
    }
    Ok(())
}
