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
    pub token: Option<usize>,
    pub hold: Decimal,
    pub total: Decimal,
    #[serde(rename = "entryNtl")]
    pub entry_ntl: Decimal,
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

/// The entry for spot token `token`, when the account holds one.
pub(crate) fn balance_of(balances: &[RawSpotBalance], token: usize) -> Option<&RawSpotBalance> {
    balances.iter().find(|balance| balance.token == Some(token))
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
