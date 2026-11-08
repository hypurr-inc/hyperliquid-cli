//! The selected signer: a local private key or an OWS vault wallet behind one
//! signing interface.

use alloy::dyn_abi::TypedData;
use alloy::signers::SignerSync;
use hypersdk::Address;
use hypersdk::hypercore::signing::{agent_signing_hash, sign_l1_action};
use hypersdk::hypercore::types::{Action, ActionRequest, Signature};
use hypersdk::hypercore::{Chain, PrivateKeySigner};

use crate::bankr::{self, BankrClient};
use crate::errors::CliError;
use crate::ows::signing::{self as ows_signing, OwsSigningConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignerSource {
    PrivateKey,
    Keystore,
    StoredAccount { alias: String },
    Ows { selector: String },
    Bankr { address: Address },
}

#[derive(Debug, Clone)]
enum SignerBackend {
    LocalPrivateKey(PrivateKeySigner),
    Ows(OwsSigningConfig),
    Bankr(BankrSigner),
}

/// A Bankr hosted wallet: the key stays with Bankr and every signature is an
/// API call, which is why signing goes through a scoped runtime thread.
#[derive(Debug, Clone)]
pub struct BankrSigner {
    client: BankrClient,
    address: Address,
}

impl BankrSigner {
    #[must_use]
    pub fn new(client: BankrClient, address: Address) -> Self {
        Self { client, address }
    }

    #[must_use]
    pub fn address(&self) -> Address {
        self.address
    }

    #[must_use]
    pub fn client(&self) -> &BankrClient {
        &self.client
    }
}

#[derive(Debug, Clone)]
pub struct SelectedSigner {
    backend: SignerBackend,
    source: SignerSource,
    query_address: Address,
}

impl SelectedSigner {
    #[must_use]
    pub fn local_private_key(
        signer: PrivateKeySigner,
        source: SignerSource,
        query_address: Address,
    ) -> Self {
        Self {
            backend: SignerBackend::LocalPrivateKey(signer),
            source,
            query_address,
        }
    }

    #[must_use]
    pub fn ows(config: OwsSigningConfig) -> Self {
        let selector = config.selector().to_string();
        let address = config.address();
        Self {
            backend: SignerBackend::Ows(config),
            source: SignerSource::Ows { selector },
            query_address: address,
        }
    }

    #[must_use]
    pub fn bankr(client: BankrClient, address: Address) -> Self {
        Self {
            backend: SignerBackend::Bankr(BankrSigner::new(client, address)),
            source: SignerSource::Bankr { address },
            query_address: address,
        }
    }

    #[must_use]
    pub fn address(&self) -> Address {
        match &self.backend {
            SignerBackend::LocalPrivateKey(signer) => signer.address(),
            SignerBackend::Ows(config) => config.address(),
            SignerBackend::Bankr(signer) => signer.address(),
        }
    }

    #[must_use]
    pub fn query_address(&self) -> Address {
        self.query_address
    }

    #[must_use]
    pub fn source(&self) -> &SignerSource {
        &self.source
    }

    /// The vault wallet's name, when the signer is a resolved OWS wallet.
    #[must_use]
    pub fn ows_wallet_name(&self) -> Option<&str> {
        match &self.backend {
            SignerBackend::LocalPrivateKey(_) | SignerBackend::Bankr(_) => None,
            SignerBackend::Ows(config) => config.wallet_name(),
        }
    }

    pub fn private_key_signer(&self) -> Result<&PrivateKeySigner, CliError> {
        match &self.backend {
            SignerBackend::LocalPrivateKey(signer) => Ok(signer),
            SignerBackend::Ows(config) => {
                Err(ows_signing::unsupported_live_signing(config.selector()))
            }
            SignerBackend::Bankr(signer) => Err(CliError::Unsupported(format!(
                "the Bankr wallet {} signs through the Bankr API; a local private key is required for this command",
                signer.address()
            ))),
        }
    }

    /// Guard network lookups that should fail before side effects when an OWS selector
    /// is only an address placeholder. This is deliberately not a command allowlist:
    /// command-level OWS status is exposed in schema/docs, while the signing helpers
    /// still verify the recovered address for every payload before submission.
    pub fn ensure_can_attempt_live_signing(&self) -> Result<(), CliError> {
        match &self.backend {
            SignerBackend::LocalPrivateKey(_) | SignerBackend::Bankr(_) => Ok(()),
            SignerBackend::Ows(config) if config.has_resolved_wallet() => Ok(()),
            SignerBackend::Ows(config) => {
                Err(ows_signing::unsupported_live_signing(config.selector()))
            }
        }
    }

    pub fn sign_l1_action_sync(
        &self,
        action: Action,
        nonce: u64,
        vault_address: Option<Address>,
        chain: Chain,
    ) -> Result<serde_json::Value, CliError> {
        match &self.backend {
            SignerBackend::LocalPrivateKey(signer) => {
                let request = action
                    .sign_sync(signer, nonce, vault_address, None, chain)
                    .map_err(|err| {
                        CliError::Internal(anyhow::anyhow!("failed to sign action: {err}"))
                    })?;
                serde_json::to_value(request).map_err(|err| {
                    CliError::Internal(anyhow::anyhow!("failed to encode signed action: {err}"))
                })
            }
            SignerBackend::Ows(config) => {
                let signing_hash =
                    action
                        .prehash(nonce, vault_address, None, chain)
                        .map_err(|err| {
                            CliError::Internal(anyhow::anyhow!("failed to hash OWS action: {err}"))
                        })?;
                let signature = ows_signing::sign_hash(config, signing_hash)?;
                let request = ActionRequest {
                    action,
                    nonce,
                    signature,
                    vault_address,
                    expires_after: None,
                };
                serde_json::to_value(request).map_err(|err| {
                    CliError::Internal(anyhow::anyhow!("failed to encode signed action: {err}"))
                })
            }
            SignerBackend::Bankr(signer) => {
                let typed_data = bankr_typed_data(&action, nonce, vault_address, chain)?;
                let signature = signer
                    .client()
                    .blocking_sign(&typed_data, signer.address())?;
                let request = ActionRequest {
                    action,
                    nonce,
                    signature,
                    vault_address,
                    expires_after: None,
                };
                serde_json::to_value(request).map_err(|err| {
                    CliError::Internal(anyhow::anyhow!("failed to encode signed action: {err}"))
                })
            }
        }
    }

    pub fn sign_typed_data(&self, typed_data: &TypedData) -> Result<Signature, CliError> {
        match &self.backend {
            SignerBackend::LocalPrivateKey(signer) => signer
                .sign_dynamic_typed_data_sync(typed_data)
                .map(Into::into)
                .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to sign action: {err}"))),
            SignerBackend::Ows(config) => ows_signing::sign_typed_data(config, typed_data),
            SignerBackend::Bankr(signer) => signer.client().blocking_sign(
                &bankr::BankrTypedData::from_typed(typed_data)?,
                signer.address(),
            ),
        }
    }

    pub async fn sign_l1_connection_id(
        &self,
        chain: Chain,
        connection_id: alloy::primitives::B256,
    ) -> Result<Signature, CliError> {
        match &self.backend {
            SignerBackend::LocalPrivateKey(signer) => sign_l1_action(signer, chain, connection_id)
                .await
                .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to sign action: {err}"))),
            SignerBackend::Ows(config) => {
                ows_signing::sign_hash(config, agent_signing_hash(chain, connection_id))
            }
            SignerBackend::Bankr(signer) => {
                let typed_data = bankr::agent_typed_data(chain, connection_id);
                signer.client().blocking_sign(&typed_data, signer.address())
            }
        }
    }

    pub fn sign_message(&self, message: &[u8]) -> Result<alloy::primitives::Signature, CliError> {
        match &self.backend {
            SignerBackend::LocalPrivateKey(signer) => {
                signer.sign_message_sync(message).map_err(|err| {
                    CliError::Internal(anyhow::anyhow!("failed to sign message: {err}"))
                })
            }
            SignerBackend::Ows(config) => ows_signing::sign_message(config, message),
            SignerBackend::Bankr(signer) => Err(CliError::Unsupported(format!(
                "the Bankr wallet {} signs Hyperliquid actions through Bankr, not arbitrary messages",
                signer.address()
            ))),
        }
    }
}

/// The EIP-712 document Bankr signs for an L1 action, refusing any action whose
/// signature hypersdk derives from a schema this module does not build (sends,
/// withdrawals, staking, approvals, multisig) before a request is sent.
fn bankr_typed_data(
    action: &Action,
    nonce: u64,
    vault_address: Option<Address>,
    chain: Chain,
) -> Result<bankr::BankrTypedData, CliError> {
    let typed_data = match action {
        Action::UsdClassTransfer(inner) => {
            bankr::usd_class_transfer_typed_data(chain, &inner.amount, inner.to_perp, inner.nonce)
        }
        _ => {
            let connection_id = action.hash(nonce, vault_address, None).map_err(|err| {
                CliError::Internal(anyhow::anyhow!("failed to hash bankr action: {err}"))
            })?;
            bankr::agent_typed_data(chain, connection_id)
        }
    };
    let expected = action
        .prehash(nonce, vault_address, None, chain)
        .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to hash bankr action: {err}")))?;
    if typed_data.signing_hash()? != expected {
        return Err(CliError::Unsupported(
            "Bankr signs trading actions (orders, cancels, modifies, leverage, margin, trailing stops, TWAP, vault and subaccount actions) and spot-to-perp USDC moves; this action needs a user-signed payload Bankr does not cover, so use a local key or an OWS wallet for it"
                .to_string(),
        ));
    }
    Ok(typed_data)
}
