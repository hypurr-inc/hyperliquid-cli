//! User-signed EIP-712 actions (`HyperliquidTransaction:*`).
//!
//! The same struct is the typed data that gets signed and the body of the
//! exchange action: the action is that body plus its `type` tag and the
//! `signatureChainId`. A command declares the struct once and names its tag;
//! [`document`] is the action, sent by [`submit`] and shown by [`preview`].

use alloy::dyn_abi::{Eip712Types, Resolver, TypedData};
use alloy::sol_types::SolStruct;
use hypersdk::hypercore::Chain;
use serde::Serialize;
use serde_json::Value;

use crate::errors::CliError;
use crate::exchange::actions;
use crate::http_api::ApiClient;
use crate::network::ChainExt;
use crate::signable::{Signs, UserSigned, ensure_supports_expires_after, user_signed_primary_type};
use crate::signing::SelectedSigner;

/// `build` receives the network name for `hyperliquidChain` and the nonce the
/// action is signed and sent with.
pub(crate) async fn submit<T: UserSigned>(
    api: &ApiClient,
    signer: &SelectedSigner,
    build: impl FnOnce(String, u64) -> T,
) -> Result<(), CliError> {
    if api.request_lifetime().is_some() {
        ensure_supports_expires_after(Some(Signs::UserSigned(T::NAME)))?;
    }
    let chain = api.chain();
    let nonce = api.nonces().next();
    let message = build(chain.label().to_string(), nonce);
    let signature = signer.sign_typed_data(&typed_data::<T>(&message, chain))?;
    actions::send_user_signed(api, &document(&message, chain), nonce, signature).await
}

/// The exchange action of `message`: its fields, the action's `type` tag and
/// the `signatureChainId` of `chain`.
pub(crate) fn document<T: UserSigned>(message: &T, chain: Chain) -> Value {
    let mut action = serde_json::to_value(message).expect("serialize user-signed action");
    if let Value::Object(fields) = &mut action {
        fields.insert("type".to_string(), T::ACTION_TYPE.into());
        fields.insert(
            "signatureChainId".to_string(),
            chain.signature_chain_id().into(),
        );
    }
    action
}

/// The action a dry run shows: [`document`] without its `nonce`, which the
/// live send takes when it signs. Addresses are spelled as the typed message
/// serializes them, lowercase; a preview that prints them checksummed sets them
/// itself.
pub(crate) fn preview<T: UserSigned>(message: &T, chain: Chain) -> Value {
    let mut action = document(message, chain);
    if let Value::Object(fields) = &mut action {
        fields.remove("nonce");
    }
    action
}

/// The EIP-712 typed data a user-signed `message` is signed over.
pub(crate) fn typed_data<T: SolStruct>(message: &impl Serialize, chain: Chain) -> TypedData {
    let mut resolver = Resolver::from_struct::<T>();
    resolver
        .ingest_string(T::eip712_encode_type())
        .expect("failed to ingest EIP-712 type");

    let mut types = Eip712Types::from(&resolver);
    let primary_type = types.remove(T::NAME).expect("missing primary EIP-712 type");
    types.insert(user_signed_primary_type(T::NAME), primary_type);

    TypedData {
        domain: chain.domain(),
        resolver: Resolver::from(types),
        primary_type: user_signed_primary_type(T::NAME),
        message: serde_json::to_value(message).expect("serialize typed-data message"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::signable::UsdClassTransfer;

    fn transfer(chain: Chain) -> UsdClassTransfer {
        UsdClassTransfer {
            hyperliquidChain: chain.label().to_string(),
            amount: "12.5".to_string(),
            toPerp: true,
            nonce: 1_700_000_000_123,
        }
    }

    #[test]
    fn the_action_is_the_signed_struct_with_its_tag_and_chain_id() {
        assert_eq!(
            document(&transfer(Chain::Testnet), Chain::Testnet),
            json!({
                "type": "usdClassTransfer",
                "signatureChainId": "0x66eee",
                "hyperliquidChain": "Testnet",
                "amount": "12.5",
                "toPerp": true,
                "nonce": 1_700_000_000_123_u64,
            })
        );
    }

    #[test]
    fn a_preview_is_the_action_without_its_nonce() {
        let message = transfer(Chain::Mainnet);
        let mut action = document(&message, Chain::Mainnet);
        action.as_object_mut().unwrap().remove("nonce");
        assert_eq!(preview(&message, Chain::Mainnet), action);
    }

    #[test]
    fn the_signed_document_names_the_prefixed_primary_type() {
        let typed = typed_data::<UsdClassTransfer>(&transfer(Chain::Mainnet), Chain::Mainnet);
        assert_eq!(
            typed.primary_type,
            "HyperliquidTransaction:UsdClassTransfer"
        );
    }
}
