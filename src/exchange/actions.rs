use alloy::dyn_abi::{Eip712Types, Resolver, TypedData};
use alloy::primitives::keccak256;
use alloy::sol_types::SolStruct;
use hypersdk::Address;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::types::{Action, Response, Signature};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::clock;
use crate::errors::{self, CliError};
use crate::exchange_reply::{self, Reply};
use crate::hints;
use crate::http_api::ApiClient;
use crate::response_sanitization::Untrusted;
use crate::signing::{SelectedSigner, Signs, USER_SIGNED_PREFIX, ensure_supports_expires_after};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignedJsonActionRequest {
    action: serde_json::Value,
    nonce: u64,
    signature: Signature,
    vault_address: Option<hypersdk::Address>,
    /// A user-signed action has no expiry: the exchange refuses the field
    /// there, so the key is always `null`.
    expires_after: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RawL1ActionRequest<'a, A> {
    action: &'a A,
    nonce: u64,
    signature: Signature,
    vault_address: Option<Address>,
    /// `--expires-after` as a millisecond timestamp; `null` without it.
    expires_after: Option<u64>,
}

/// What a request carried, kept to name it if the exchange never answered.
#[derive(Debug, Clone, Copy)]
struct Sent {
    nonce: u64,
    expires_after: Option<u64>,
}

/// The `context` of an action whose rejection needs no more specific name.
pub(crate) const EXCHANGE_ACTION: &str = "exchange action";

/// The `expiresAfter` of an action signed now, when `--expires-after` was given.
fn expires_after_now(api: &ApiClient) -> Option<u64> {
    api.request_lifetime()
        .map(|lifetime| clock::expires_at_ms(clock::now_ms(), lifetime))
}

/// Sign and send an L1 action supported by hypersdk, returning the ok response
/// payload as the exchange sent it. The sender takes the nonce (the current
/// time in milliseconds, raised above every nonce this machine already signed
/// with, see [`crate::signing::nonce`]) and the expiry at the moment it signs, so a caller that
/// returns early, as a dry run does, spends neither.
pub(crate) async fn send_l1_action(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: Action,
    vault_address: Option<Address>,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    send_l1_action_with_nonce(
        api,
        signer,
        action,
        api.nonces().next(),
        vault_address,
        context,
    )
    .await
}

/// [`send_l1_action`] with a nonce the caller owns: one the person supplied
/// (`account invalidate-nonce`) or one the signed document embeds
/// (`approveAgent`).
pub(crate) async fn send_l1_action_with_nonce(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: Action,
    nonce: u64,
    vault_address: Option<Address>,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let parsed =
        sign_and_post::<serde_json::Value>(api, signer, action, nonce, vault_address).await?;
    ensure_ok_raw_response(parsed, context)
}

/// Sign and send an L1 action that the exchange answers with the `default`
/// payload, which any other reply is an error for: the unit-returning
/// counterpart of [`send_l1_action`], for an action with nothing to report.
pub(crate) async fn send_default_l1_action(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: Action,
) -> Result<(), CliError> {
    send_default_l1_action_with_nonce(api, signer, action, api.nonces().next()).await
}

/// [`send_default_l1_action`] with a nonce the signed document embeds
/// (`usdSend`, `spotSend`, `sendAsset`) or the person supplied
/// (`account invalidate-nonce`).
pub(crate) async fn send_default_l1_action_with_nonce(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: Action,
    nonce: u64,
) -> Result<(), CliError> {
    let parsed = sign_and_post::<Response>(api, signer, action, nonce, None).await?;
    ensure_default_response(parsed, EXCHANGE_ACTION)
}

async fn sign_and_post<T: DeserializeOwned>(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: Action,
    nonce: u64,
    vault_address: Option<Address>,
) -> Result<T, CliError> {
    let expires_after = expires_after_now(api);
    let request =
        signer.sign_l1_action_sync(action, nonce, vault_address, expires_after, api.chain())?;
    let sent = Sent {
        nonce,
        expires_after,
    };
    post(api, &request, sent).await
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
    signs: Signs<'_>,
) -> Result<(), CliError> {
    if api.request_lifetime().is_some() {
        ensure_supports_expires_after(Some(signs))?;
    }
    let request = SignedJsonActionRequest {
        action,
        nonce,
        signature,
        vault_address: None,
        expires_after: None,
    };
    let sent = Sent {
        nonce,
        expires_after: None,
    };
    let parsed = post(api, &request, sent).await?;
    ensure_default_response(parsed, EXCHANGE_ACTION)
}

/// Sign and send a raw L1 action not currently modeled by hypersdk's [`Action`] enum.
pub(crate) async fn send_raw_l1_json_action<A: Serialize>(
    api: &ApiClient,
    signer: &SelectedSigner,
    action: &A,
    vault_address: Option<Address>,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let nonce = api.nonces().next();
    let expires_after = expires_after_now(api);
    let signature = sign_raw_l1_action(
        signer,
        api.chain(),
        action,
        nonce,
        vault_address,
        expires_after,
    )
    .await?;
    let request = RawL1ActionRequest {
        action,
        nonce,
        signature,
        vault_address,
        expires_after,
    };
    let sent = Sent {
        nonce,
        expires_after,
    };
    post_json_raw(api, &request, sent, context).await
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
    expires_after: Option<u64>,
) -> Result<Signature, CliError> {
    let connection_id = raw_rmp_hash(action, nonce, vault_address, expires_after)
        .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to encode action: {err}")))?;
    signer.sign_l1_connection_id(chain, connection_id).await
}

/// The L1 action hash the exchange verifies: the msgpack of the action, the
/// nonce, the vault (`0x00`, or `0x01` and the address) and, when the request
/// expires, `0x00` and the expiry, all big-endian. This is the Python SDK's
/// `action_hash`.
fn raw_rmp_hash<T: Serialize>(
    value: &T,
    nonce: u64,
    maybe_vault_address: Option<Address>,
    maybe_expires_after: Option<u64>,
) -> Result<alloy::primitives::B256, rmp_serde::encode::Error> {
    let mut bytes = rmp_serde::to_vec_named(value)?;
    bytes.extend(nonce.to_be_bytes());

    if let Some(vault_address) = maybe_vault_address {
        bytes.push(1);
        bytes.extend(vault_address.as_slice());
    } else {
        bytes.push(0);
    }

    if let Some(expires_after) = maybe_expires_after {
        bytes.push(0);
        bytes.extend(expires_after.to_be_bytes());
    }

    Ok(keccak256(bytes))
}

/// POST a signed request. A reply that never arrives after the send leaves the
/// action's fate unknown; the error then names the nonce so the person can burn
/// it with `account invalidate-nonce`.
async fn post<T: DeserializeOwned>(
    api: &ApiClient,
    request: &impl Serialize,
    sent: Sent,
) -> Result<T, CliError> {
    api.exchange::<T>(request)
        .await
        .map_err(|err| name_unknown_outcome(err, sent, api.chain()))
}

fn name_unknown_outcome(err: CliError, sent: Sent, chain: Chain) -> CliError {
    let CliError::UnknownOutcome(cause) = err else {
        return err;
    };
    let expiry = sent
        .expires_after
        .map(|ms| format!(", expiring at {ms}"))
        .unwrap_or_default();
    let nonce = sent.nonce;
    CliError::UnknownOutcome(format!(
        "{cause} It was signed with nonce {nonce}{expiry}. If it may still be in flight, burn the nonce with the same signer: {}",
        hints::invalidate_nonce_command(!chain.is_mainnet(), nonce),
    ))
}

async fn post_json_raw(
    api: &ApiClient,
    request: &impl Serialize,
    sent: Sent,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let parsed = post::<serde_json::Value>(api, request, sent).await?;
    ensure_ok_raw_response(parsed, context)
}

fn ensure_ok_raw_response(
    response: serde_json::Value,
    context: &'static str,
) -> Result<serde_json::Value, CliError> {
    let message = match Reply::decode(&response) {
        Reply::Ok(payload) => return Ok(payload.clone()),
        Reply::Err(text) => text,
        Reply::Unrecognized(body) => exchange_reply::failure_text(body),
    };
    Err(errors::from_exchange_rejection(&message, context))
}

fn ensure_default_response(response: Response, context: &'static str) -> Result<(), CliError> {
    match response {
        Response::Ok(hypersdk::hypercore::types::OkResponse::Default) => Ok(()),
        Response::Err(err) => Err(errors::from_exchange_rejection(&err, context)),
        other => Err(CliError::internal_with_remote(
            format!("{context}: unexpected response type: "),
            &Untrusted::from_remote(&format!("{other:?}")),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersdk::hypercore::types::Action;

    const NONCE: u64 = 1_700_000_000_123;
    const EXPIRES: u64 = 1_700_000_030_123;
    const VAULT: &str = "0x1111111111111111111111111111111111111111";

    fn hash(value: &impl Serialize, vault: Option<&str>, expires: Option<u64>) -> String {
        let vault = vault.map(|vault| vault.parse::<Address>().unwrap());
        raw_rmp_hash(value, NONCE, vault, expires)
            .unwrap()
            .to_string()
    }

    // Known answers computed outside this code base: the Python SDK's
    // `action_hash` is `keccak(msgpack(action) + nonce_be8 + vault_marker
    // [+ 0x00 + expires_be8])`, evaluated here by hand over the msgpack bytes of
    // `{"type":"noop"}` (81 a4 "type" a4 "noop") and of
    // `{"type":"reserveRequestWeight","weight":10}` with `cast keccak`.
    #[test]
    fn raw_hashes_match_the_python_sdks_action_hash() {
        let noop = serde_json::json!({"type": "noop"});
        assert_eq!(
            hash(&noop, None, None),
            "0x5b767265cd17a5f022bb4df5409d37a94579f433f4b342696bf4469f496c13a0"
        );
        assert_eq!(
            hash(&noop, None, Some(EXPIRES)),
            "0x0cb6fdcccbf459d38a690d4c8980e2823936b6f0817b71d242db4ceac265817e"
        );
        assert_eq!(
            hash(&noop, Some(VAULT), None),
            "0xf2cc1f28cd1376a4871b4bd6e8f28b5742affe34500a405a5509b304fcba08af"
        );
        assert_eq!(
            hash(&noop, Some(VAULT), Some(EXPIRES)),
            "0x3ce03acbc3389b960ac89bafc48b0aee641a9cd22aeb0d31cb0d2bf8f96eea62"
        );

        // A typed action, as the commands send: a `serde_json::Value` number
        // would not encode as a msgpack integer in this dependency graph.
        #[derive(Serialize)]
        struct Reserve {
            #[serde(rename = "type")]
            kind: &'static str,
            weight: u32,
        }
        let reserve = Reserve {
            kind: "reserveRequestWeight",
            weight: 10,
        };
        assert_eq!(
            hash(&reserve, None, None),
            "0xa6486bc2a85ee7ae8cfae06c6d28931a51f40fe6593669aeff6e9c1dc8cdeb04"
        );
        assert_eq!(
            hash(&reserve, None, Some(EXPIRES)),
            "0xf065b90e83ca8795363d7690dda87756ce3efbbeb71580f452fe7e2c1fbaa60c"
        );
    }

    #[test]
    fn raw_hashes_match_hypersdks_hash_of_the_same_action() {
        let destination: Address = "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01"
            .parse()
            .unwrap();
        let actions = [
            Action::Noop,
            Action::ReserveRequestWeight {
                weight: 10,
                destination: None,
            },
            Action::ReserveRequestWeight {
                weight: 7,
                destination: Some(destination),
            },
            Action::CreateSubAccount {
                name: "desk".to_string(),
            },
        ];
        for action in &actions {
            for vault in [None, Some(VAULT)] {
                for expires in [None, Some(EXPIRES)] {
                    let sdk = action
                        .hash(NONCE, vault.map(|vault| vault.parse().unwrap()), expires)
                        .unwrap();
                    assert_eq!(
                        hash(action, vault, expires),
                        sdk.to_string(),
                        "{action:?} vault={vault:?} expires={expires:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_expiry_is_part_of_the_raw_hash() {
        let noop = serde_json::json!({"type": "noop"});
        let none = hash(&noop, None, None);
        let some = hash(&noop, None, Some(EXPIRES));
        assert_ne!(none, some);
        assert_ne!(some, hash(&noop, None, Some(EXPIRES + 1)));
    }

    #[test]
    fn an_unknown_outcome_names_the_nonce_and_a_set_expiry() {
        let unknown = || CliError::unknown_outcome("The request timed out after 10s");
        let sent = Sent {
            nonce: NONCE,
            expires_after: None,
        };
        assert_eq!(
            name_unknown_outcome(unknown(), sent, Chain::Mainnet).to_string(),
            format!(
                "Can't confirm whether the exchange applied this action. The request timed out after 10s. It was signed with nonce {NONCE}. If it may still be in flight, burn the nonce with the same signer: hyperliquid account invalidate-nonce --nonce {NONCE}"
            )
        );
        let sent = Sent {
            nonce: NONCE,
            expires_after: Some(EXPIRES),
        };
        assert!(
            name_unknown_outcome(unknown(), sent, Chain::Testnet)
                .to_string()
                .ends_with(&format!(
                    "signed with nonce {NONCE}, expiring at {EXPIRES}. If it may still be in flight, burn the nonce with the same signer: hyperliquid --testnet account invalidate-nonce --nonce {NONCE}"
                ))
        );
    }

    #[test]
    fn other_errors_pass_through_untouched() {
        let sent = Sent {
            nonce: NONCE,
            expires_after: None,
        };
        let err = name_unknown_outcome(CliError::AuthRequired, sent, Chain::Mainnet);
        assert!(matches!(err, CliError::AuthRequired));
    }

    #[test]
    fn a_raw_reply_is_its_ok_payload_or_a_rejection_quoting_the_exchange() {
        let payload = serde_json::json!({"type": "default"});
        for status in ["ok", "OK"] {
            let reply = serde_json::json!({"status": status, "response": payload});
            assert_eq!(ensure_ok_raw_response(reply, "ctx").unwrap(), payload);
        }
        assert_eq!(
            ensure_ok_raw_response(serde_json::json!({"status": "ok"}), "ctx").unwrap(),
            serde_json::Value::Null
        );

        // Any other status is a rejection: the exchange's text when it sent a
        // string, the whole body otherwise, whatever the status is called.
        for status in ["err", "failure", "mystery"] {
            let text = ensure_ok_raw_response(
                serde_json::json!({"status": status, "response": "Insufficient margin"}),
                "order failed",
            )
            .unwrap_err()
            .to_string();
            assert!(text.contains("Insufficient margin"), "{status}: {text}");
        }
        let body = serde_json::json!({"status": "err", "response": {"detail": 1}});
        let text = ensure_ok_raw_response(body.clone(), "order failed")
            .unwrap_err()
            .to_string();
        assert!(text.contains(&body.to_string()), "{text}");
    }
}
