//! An account's `borrowLendUserState`, which `borrowlend user` prints and
//! `watch risk` reads for the protocol's own health verdict.
//!
//! Layer rule: below the commands; neither reaches into the other.

use hypersdk::{Address, Decimal};
use serde::Deserialize;

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;
use crate::wire_decimal;

/// `borrowLendUserState`: the account's borrow/lend positions and the
/// protocol's own verdict on their health. Unlike spot balances it is the
/// account's actual supply and borrow, whatever the account abstraction.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawBorrowLendUserState {
    pub(crate) token_to_state: Vec<(u32, RawTokenState)>,
    /// The protocol's verdict: `healthy` or a state it is not.
    pub(crate) health: String,
    #[serde(default, deserialize_with = "wire_decimal::deserialize_option")]
    health_factor: Option<Decimal>,
}

impl RawBorrowLendUserState {
    pub(crate) fn health_factor(&self) -> Option<Decimal> {
        self.health_factor
    }
}

pub(crate) async fn fetch_user_state(
    api: &ApiClient,
    user: Address,
) -> Result<RawBorrowLendUserState, CliError> {
    api.info(
        &InfoRequest::user("borrowLendUserState", user),
        "loading user borrow/lend state",
    )
    .await
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawTokenState {
    pub(crate) borrow: RawLegState,
    pub(crate) supply: RawLegState,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawLegState {
    #[serde(with = "wire_decimal")]
    pub(crate) basis: Decimal,
    #[serde(with = "wire_decimal")]
    pub(crate) value: Decimal,
}
