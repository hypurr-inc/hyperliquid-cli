use alloy::dyn_abi::{Eip712Types, Resolver, TypedData};
use alloy::primitives::keccak256;
use alloy::sol_types::SolStruct;
use hypersdk::Address;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::types::{Action, Response, Signature};
use serde::Serialize;

use crate::clock;
use crate::errors::{self, CliError};
use crate::http_api::ApiClient;
use crate::response_sanitization::Untrusted;
use crate::signing::{SelectedSigner, USER_SIGNED_PREFIX};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignedJsonActionRequest {
    action: serde_json::Value,
    nonce: u64,
    signature: Signature,
    vault_address: Option<hypersdk::Address>,
    /// The exchange takes an optional expiry; no command sets one, and the
    /// request has always carried the key as `null`.
    expires_after: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RawL1ActionRequest<'a, A> {
    action: &'a A,
    nonce: u64,
    signature: Signature,
    vault_address: Option<Address>,
    /// See [`SignedJsonActionRequest::expires_after`].
    expires_after: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RawL1ActionMetadata {
    nonce: u64,
    vault_address: Option<Address>,
}

impl RawL1ActionMetadata {
    pub(crate) fn new(nonce: u64) -> Self {
        Self {
            nonce,
            vault_address: None,
        }
    }

    pub(crate) fn with_vault_address(mut self, vault_address: Option<Address>) -> Self {
        self.vault_address = vault_address;
        self
    }
}

/// The nonce of an exchange action: the current time in milliseconds, which
/// the exchange accepts inside a window around its own clock.
#[must_use]
pub(crate) fn nonce_now() -> u64 {
    clock::now_ms()
}

pub(crate) async fn send_l1_action(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: Action,
    nonce: u64,
) -> Result<(), CliError> {
    let request = signer.sign_l1_action_sync(action, nonce, None, api.chain())?;
    post_json(api, &request, "exchange action").await
}

/// Sign and send an L1 action supported by hypersdk, returning the raw ok response payload.
pub(crate) async fn send_l1_action_raw(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: Action,
    nonce: u64,
    vault_address: Option<Address>,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let request = signer.sign_l1_action_sync(action, nonce, vault_address, api.chain())?;
    post_json_raw(api, &request, context).await
}

pub(crate) fn sign_user_action<T: SolStruct>(
    signer: &SelectedSigner,
    chain: Chain,
    message: &impl Serialize,
) -> Result<Signature, CliError> {
    let typed_data = typed_data::<T>(message, chain);
    signer.sign_typed_data(&typed_data)
}

/// Send a user-signed JSON action that is not currently modeled in hypersdk's Action enum.
pub(crate) async fn send_user_signed_json_action(
    api: &ApiClient,
    action: serde_json::Value,
    nonce: u64,
    signature: Signature,
) -> Result<(), CliError> {
    let request = SignedJsonActionRequest {
        action,
        nonce,
        signature,
        vault_address: None,
        expires_after: None,
    };
    post_json(api, &request, "exchange action").await
}

/// Sign and send a raw L1 action not currently modeled by hypersdk's [`Action`] enum.
pub(crate) async fn send_raw_l1_json_action<A: Serialize>(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: &A,
    metadata: RawL1ActionMetadata,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let signature = sign_raw_l1_action(
        signer,
        api.chain(),
        action,
        metadata.nonce,
        metadata.vault_address,
    )
    .await?;
    let request = RawL1ActionRequest {
        action,
        nonce: metadata.nonce,
        signature,
        vault_address: metadata.vault_address,
        expires_after: None,
    };
    post_json_raw(api, &request, context).await
}

fn typed_data<T: SolStruct>(message: &impl Serialize, chain: Chain) -> TypedData {
    let mut resolver = Resolver::from_struct::<T>();
    resolver
        .ingest_string(T::eip712_encode_type())
        .expect("failed to ingest EIP-712 type");

    let mut types = Eip712Types::from(&resolver);
    let primary_type = types.remove(T::NAME).expect("missing primary EIP-712 type");
    types.insert(format!("{USER_SIGNED_PREFIX}{}", T::NAME), primary_type);

    TypedData {
        domain: chain.domain(),
        resolver: Resolver::from(types),
        primary_type: format!("{USER_SIGNED_PREFIX}{}", T::NAME),
        message: serde_json::to_value(message).expect("serialize typed-data message"),
    }
}

async fn sign_raw_l1_action<A: Serialize>(
    signer: &SelectedSigner,
    chain: Chain,
    action: &A,
    nonce: u64,
    vault_address: Option<Address>,
) -> Result<Signature, CliError> {
    let connection_id = raw_rmp_hash(action, nonce, vault_address)
        .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to encode action: {err}")))?;
    signer.sign_l1_connection_id(chain, connection_id).await
}

fn raw_rmp_hash<T: Serialize>(
    value: &T,
    nonce: u64,
    maybe_vault_address: Option<Address>,
) -> Result<alloy::primitives::B256, rmp_serde::encode::Error> {
    let mut bytes = rmp_serde::to_vec_named(value)?;
    bytes.extend(nonce.to_be_bytes());

    if let Some(vault_address) = maybe_vault_address {
        bytes.push(1);
        bytes.extend(vault_address.as_slice());
    } else {
        bytes.push(0);
    }

    Ok(keccak256(bytes))
}

async fn post_json(
    api: &ApiClient,
    request: &impl Serialize,
    context: &'static str,
) -> Result<(), CliError> {
    let parsed = api.exchange::<Response>(request).await?;
    ensure_default_response(parsed, context)
}

async fn post_json_raw(
    api: &ApiClient,
    request: &impl Serialize,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let parsed = api.exchange::<serde_json::Value>(request).await?;
    ensure_ok_raw_response(parsed, context)
}

fn ensure_ok_raw_response(
    response: serde_json::Value,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let status = response
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    if status.eq_ignore_ascii_case("ok") {
        return Ok(response
            .get("response")
            .cloned()
            .unwrap_or(serde_json::Value::Null));
    }

    let message = response
        .get("response")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| response.to_string());
    Err(errors::from_exchange_rejection(&message, context))
}

pub(crate) fn ensure_default_response(
    response: Response,
    context: &'static str,
) -> Result<(), CliError> {
    match response {
        Response::Ok(hypersdk::hypercore::types::OkResponse::Default) => Ok(()),
        Response::Err(err) => Err(errors::from_exchange_rejection(&err, context)),
        other => Err(CliError::internal_with_remote(
            format!("{context}: unexpected response type: "),
            &Untrusted::from_remote(&format!("{other:?}")),
        )),
    }
}
