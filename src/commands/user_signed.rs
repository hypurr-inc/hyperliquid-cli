//! User-signed EIP-712 actions (`HyperliquidTransaction:*`).
//!
//! The same struct is the typed data that gets signed and the body of the
//! exchange action: the action is that body plus its `type` tag and the
//! `signatureChainId`. A command declares the struct once and names its tag.

use alloy::sol_types::SolStruct;
use serde::Serialize;

use crate::commands::actions;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::signing::SelectedSigner;

/// A user-signed action: an EIP-712 struct whose JSON is the action body.
pub(crate) trait UserSigned: SolStruct + Serialize {
    /// The exchange action's `type` tag.
    const ACTION_TYPE: &'static str;
}

/// Sign and submit the action `build` describes.
///
/// `build` receives the network name for `hyperliquidChain` and the nonce the
/// action is signed and sent with.
pub(crate) async fn submit<T: UserSigned>(
    api: &ApiClient,
    signer: &SelectedSigner,
    build: impl FnOnce(String, u64) -> T,
) -> Result<(), CliError> {
    let chain = api.chain();
    let nonce = actions::nonce_now();
    let message = build(chain.to_string(), nonce);
    let signature = actions::sign_user_action::<T>(signer, chain, &message)?;
    let mut action = serde_json::to_value(&message)
        .map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
    if let serde_json::Value::Object(fields) = &mut action {
        fields.insert("type".to_string(), T::ACTION_TYPE.into());
        fields.insert(
            "signatureChainId".to_string(),
            chain.arbitrum_id().to_string().into(),
        );
    }
    actions::send_user_signed_json_action(api, action, nonce, signature).await
}
