//! The subaccount listing `account subaccounts` and `subaccount list` print.
//!
//! Layer rule: below the commands; neither reaches into the other.

use hypersdk::hypercore::SubAccount;
use hypersdk::{Address, Decimal};
use serde::Serialize;

use crate::commands::common::rows::{ListRow, Row, Rows};
use crate::invocation::Invocation;
use crate::wire::InfoRequest;

pub(crate) async fn subaccounts(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    // The exchange answers `null` for an account without subaccounts.
    let subaccounts: Vec<SubaccountRow> = inv
        .remote()?
        .api()
        .info::<Option<Vec<SubAccount>>>(
            &InfoRequest::user("subAccounts", user),
            "loading subaccounts",
        )
        .await?
        .unwrap_or_default()
        .into_iter()
        .map(SubaccountRow::from)
        .collect();
    inv.print(&Rows::new(subaccounts));
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubaccountRow {
    pub name: String,
    pub address: String,
    pub master: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub account_value: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub withdrawable: Decimal,
    pub positions_count: usize,
    pub spot_balances_count: usize,
}

impl From<SubAccount> for SubaccountRow {
    fn from(subaccount: SubAccount) -> Self {
        Self {
            name: subaccount.name,
            address: subaccount.sub_account_user.to_string(),
            master: subaccount.master.to_string(),
            account_value: subaccount.clearinghouse_state.margin_summary.account_value,
            withdrawable: subaccount.clearinghouse_state.withdrawable,
            positions_count: subaccount.clearinghouse_state.asset_positions.len(),
            spot_balances_count: subaccount.spot_state.balances.len(),
        }
    }
}

impl Row for SubaccountRow {
    const HEADERS: &'static [&'static str] = &[
        "Name",
        "Address",
        "Master",
        "Account Value",
        "Withdrawable",
        "Positions",
        "Spot Balances",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.address.clone(),
            self.master.clone(),
            self.account_value.to_string(),
            self.withdrawable.to_string(),
            self.positions_count.to_string(),
            self.spot_balances_count.to_string(),
        ]
    }
}

impl ListRow for SubaccountRow {
    const EMPTY: &'static str = "No subaccounts.";
}
