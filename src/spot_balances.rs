//! A user's spot balances as `spotClearinghouseState` reports them.
//!
//! Layer rule: below `commands`. Where an account's USDC lives
//! ([`crate::exchange::funds`]), the unified account ratio
//! ([`crate::account_mode`]) and the account readers all start from this read.

use hypersdk::{Address, Decimal};
use serde::Deserialize;

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSpotClearinghouseState {
    balances: Vec<RawSpotBalance>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawSpotBalance {
    pub coin: String,
    pub token: Option<u32>,
    pub hold: Decimal,
    pub total: Decimal,
    #[serde(rename = "entryNtl")]
    pub entry_ntl: Decimal,
}

/// The balance of spot token `token`. A row that names no token (an outcome
/// balance) is no token's.
pub(crate) fn balance_of(balances: &[RawSpotBalance], token: u32) -> Option<&RawSpotBalance> {
    balances.iter().find(|balance| balance.token == Some(token))
}

pub(crate) async fn user_spot_balances_raw(
    api: &ApiClient,
    user: Address,
) -> Result<Vec<RawSpotBalance>, CliError> {
    let state = api
        .info::<RawSpotClearinghouseState>(
            &InfoRequest::user("spotClearinghouseState", user),
            "loading user spot balances",
        )
        .await?;
    Ok(state.balances)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_tokenless_outcome_balance_alongside_regular_balance() {
        let raw = r#"{"balances":[{"coin":"USDC","token":0,"total":"95.99764142","hold":"0.0","entryNtl":"0.0"},{"coin":"+100","total":"0.0","hold":"0.0","entryNtl":"0.0"}]}"#;

        let state: RawSpotClearinghouseState = serde_json::from_str(raw).unwrap();

        assert_eq!(state.balances[0].coin, "USDC");
        assert_eq!(state.balances[0].token, Some(0));
        assert_eq!(state.balances[1].coin, "+100");
        assert_eq!(state.balances[1].token, None);
    }
}
