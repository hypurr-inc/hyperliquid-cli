//! User-signed EIP-712 actions (`HyperliquidTransaction:*`).
//!
//! The same struct is the typed data that gets signed and the body of the
//! exchange action: the action is that body plus its `type` tag and the
//! `signatureChainId`. A command declares the struct once and names its tag.

use alloy::sol_types::SolStruct;
use hypersdk::Address;
use serde::Serialize;

use crate::errors::CliError;
use crate::exchange::actions;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::signing::{SelectedSigner, Signs};

/// A user-signed action: an EIP-712 struct whose JSON is the action body.
pub(crate) trait UserSigned: SolStruct + Serialize {
    /// The exchange action's `type` tag.
    const ACTION_TYPE: &'static str;
}

/// The agent form of an action: an API wallet cannot sign the user-signed
/// (EIP-712) form, so the actions that have one are signed by it as L1 actions
/// that apply to the master it acts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentForm {
    pub master: Address,
}

impl AgentForm {
    /// The form the run's signer signs in: `Some` for a stored API wallet,
    /// `None` for any other signer, which signs the user-signed form. The one
    /// answer for the live action, the dry-run marker and the rule on what
    /// the agent form may target, which each command states for itself.
    pub(crate) fn of(inv: &Invocation) -> Option<Self> {
        inv.agent_master().map(|master| Self { master })
    }
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
    let nonce = api.nonces().next();
    let message = build(chain.to_string(), nonce);
    let signature = actions::sign_user_action::<T>(signer, chain, &message)?;
    let mut action =
        serde_json::to_value(&message).map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
    if let serde_json::Value::Object(fields) = &mut action {
        fields.insert("type".to_string(), T::ACTION_TYPE.into());
        fields.insert(
            "signatureChainId".to_string(),
            chain.arbitrum_id().to_string().into(),
        );
    }
    actions::send_user_signed_json_action(api, action, nonce, signature, Signs::UserSigned(T::NAME))
        .await
}
