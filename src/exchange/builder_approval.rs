//! The `approveBuilderFee` action, shared by every command that approves a
//! builder.

use alloy::sol;
use hypersdk::Address;
use serde::Serialize;

use crate::builder_fee::MaxFeeRate;
use crate::errors::CliError;
use crate::exchange::user_signed;
use crate::http_api::ApiClient;
use crate::signable::UserSigned;
use crate::signing::SelectedSigner;

sol! {
    #[derive(Serialize)]
    struct ApproveBuilderFee {
        string hyperliquidChain;
        string maxFeeRate;
        address builder;
        uint64 nonce;
    }
}

impl UserSigned for ApproveBuilderFee {
    const ACTION_TYPE: &'static str = "approveBuilderFee";
}

pub(crate) async fn submit(
    api: &ApiClient,
    signer: &SelectedSigner,
    builder: Address,
    max_fee_rate: &MaxFeeRate,
) -> Result<(), CliError> {
    user_signed::submit(api, signer, |hyperliquid_chain, nonce| ApproveBuilderFee {
        hyperliquidChain: hyperliquid_chain,
        maxFeeRate: max_fee_rate.percent().to_string(),
        builder,
        nonce,
    })
    .await
}
