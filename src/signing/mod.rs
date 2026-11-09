//! The selected signer: a local private key, an OWS vault wallet or a hosted
//! Bankr wallet behind one signing interface.
//!
//! Every signature the CLI makes is one of three [`Payload`]s, and each
//! provider answers once, in [`SelectedSigner::sign`], which payloads it can
//! sign: local keys sign any digest, an OWS wallet signs any digest once a wallet
//! resolved, and Bankr signs only the typed-data documents it can rebuild.
//! Provider failures are checked by [`signature::Provider`] before a signature
//! is returned.

pub(crate) mod signature;

use alloy::dyn_abi::TypedData;
use alloy::primitives::B256;
use alloy::signers::SignerSync;
use hypersdk::Address;
use hypersdk::hypercore::signing::agent_signing_hash;
use hypersdk::hypercore::types::{Action, ActionRequest, Signature};
use hypersdk::hypercore::{Chain, PrivateKeySigner};
use ows_lib::types::WalletInfo;

use crate::bankr::{BankrClient, BankrSigner, BankrTypedData};
use crate::errors::CliError;
use crate::ows::signing::{self as ows_signing, OwsSigningConfig};
use crate::ows::vault::Vault;

/// What one signature covers.
pub(crate) enum Payload<'a> {
    /// An L1 action: its msgpack hash wrapped in the `Agent` struct.
    Agent { chain: Chain, connection_id: B256 },
    /// A user-signed action's EIP-712 document.
    UserSigned(&'a TypedData),
    /// The EIP-712 digest of a user-signed [`Action`] whose document hypersdk
    /// does not expose.
    Digest(B256),
}

impl Payload<'_> {
    /// The EIP-712 digest a signature over this payload must recover against.
    pub(crate) fn digest(&self) -> Result<B256, CliError> {
        match self {
            Self::Agent {
                chain,
                connection_id,
            } => Ok(agent_signing_hash(*chain, *connection_id)),
            Self::UserSigned(typed_data) => typed_data.eip712_signing_hash().map_err(|err| {
                CliError::Internal(anyhow::anyhow!("failed to hash typed data: {err}"))
            }),
            Self::Digest(digest) => Ok(*digest),
        }
    }
}

/// How a local key was obtained, which is all `wallet show` needs to say.
#[derive(Debug, Clone)]
enum LocalOrigin {
    PrivateKey,
    Keystore,
    /// A pre-OWS stored account, which may be an agent wallet whose data is
    /// queried under its master address.
    StoredAccount {
        alias: String,
        query_address: Address,
    },
}

#[derive(Debug, Clone)]
enum SignerBackend {
    Local {
        signer: PrivateKeySigner,
        origin: LocalOrigin,
    },
    Ows(OwsSigningConfig),
    Bankr(BankrSigner),
}

/// How a signer describes itself to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerDescription {
    pub alias: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct SelectedSigner {
    backend: SignerBackend,
}

impl SelectedSigner {
    /// A key from a flag, the environment or the legacy config file.
    #[must_use]
    pub fn private_key(signer: PrivateKeySigner) -> Self {
        Self::local(signer, LocalOrigin::PrivateKey)
    }

    #[must_use]
    pub fn keystore(signer: PrivateKeySigner) -> Self {
        Self::local(signer, LocalOrigin::Keystore)
    }

    /// A pre-OWS stored account. `query_address` is the account whose data the
    /// key acts on: the master address of an agent wallet.
    #[must_use]
    pub fn stored_account(signer: PrivateKeySigner, alias: String, query_address: Address) -> Self {
        Self::local(
            signer,
            LocalOrigin::StoredAccount {
                alias,
                query_address,
            },
        )
    }

    fn local(signer: PrivateKeySigner, origin: LocalOrigin) -> Self {
        Self {
            backend: SignerBackend::Local { signer, origin },
        }
    }

    /// A wallet stored in the OWS vault, signing as `address`.
    #[must_use]
    pub fn vault_wallet(wallet: &WalletInfo, address: Address, vault: Vault) -> Self {
        Self::ows(OwsSigningConfig::for_wallet(wallet, address, vault))
    }

    /// An address named by `--ows-signer` that no vault wallet backs: it can
    /// identify an account but not sign.
    #[must_use]
    pub fn ows_address_only(selector: String, address: Address) -> Self {
        Self::ows(OwsSigningConfig::address_only(selector, address))
    }

    fn ows(config: OwsSigningConfig) -> Self {
        Self {
            backend: SignerBackend::Ows(config),
        }
    }

    #[must_use]
    pub fn bankr(client: BankrClient, address: Address) -> Self {
        Self {
            backend: SignerBackend::Bankr(BankrSigner::new(client, address)),
        }
    }

    /// The address whose key signs.
    #[must_use]
    pub fn address(&self) -> Address {
        match &self.backend {
            SignerBackend::Local { signer, .. } => signer.address(),
            SignerBackend::Ows(config) => config.address(),
            SignerBackend::Bankr(signer) => signer.address(),
        }
    }

    /// The account whose data this signer's commands read and act on.
    #[must_use]
    pub fn query_address(&self) -> Address {
        match &self.backend {
            SignerBackend::Local {
                origin: LocalOrigin::StoredAccount { query_address, .. },
                ..
            } => *query_address,
            _ => self.address(),
        }
    }

    /// Whether this signs as an agent wallet for another account, which can
    /// trade for its master but not approve agents or withdraw.
    #[must_use]
    pub fn is_stored_agent(&self) -> bool {
        self.query_address() != self.address()
    }

    #[must_use]
    pub fn describe(&self) -> SignerDescription {
        let (alias, source) = match &self.backend {
            SignerBackend::Local { origin, .. } => match origin {
                LocalOrigin::PrivateKey => (None, "private key/config/env".to_string()),
                LocalOrigin::Keystore => (None, "keystore".to_string()),
                LocalOrigin::StoredAccount { alias, .. } => {
                    (Some(alias.clone()), "stored signing account".to_string())
                }
            },
            SignerBackend::Ows(config) => config.describe(),
            SignerBackend::Bankr(signer) => {
                (None, format!("Bankr hosted wallet ({})", signer.address()))
            }
        };
        SignerDescription { alias, source }
    }

    /// Guard network lookups that should fail before side effects when an OWS selector
    /// is only an address placeholder. This is deliberately not a command allowlist:
    /// command-level OWS status is exposed in schema/docs, while signing still
    /// verifies the recovered address for every payload before submission.
    pub fn ensure_can_attempt_live_signing(&self) -> Result<(), CliError> {
        match &self.backend {
            SignerBackend::Ows(config) => config.ensure_signable(),
            SignerBackend::Local { .. } | SignerBackend::Bankr(_) => Ok(()),
        }
    }

    /// Fail now, before any side effect, when this provider cannot sign a
    /// user-signed action such as `approveAgent`, a withdrawal or a send.
    pub fn ensure_can_sign_user_actions(&self) -> Result<(), CliError> {
        match &self.backend {
            SignerBackend::Ows(config) => config.ensure_signable(),
            SignerBackend::Bankr(_) => {
                BankrTypedData::for_payload(&Payload::Digest(B256::ZERO)).map(|_| ())
            }
            SignerBackend::Local { .. } => Ok(()),
        }
    }

    /// Sign `payload`, or say why this provider cannot.
    fn sign(&self, payload: &Payload<'_>) -> Result<Signature, CliError> {
        match &self.backend {
            SignerBackend::Local { signer, .. } => signer
                .sign_hash_sync(&payload.digest()?)
                .map(Into::into)
                .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to sign action: {err}"))),
            SignerBackend::Ows(config) => ows_signing::sign(config, payload),
            SignerBackend::Bankr(signer) => signer.sign(payload),
        }
    }

    /// Sign an [`Action`] and encode the exchange request carrying it.
    pub fn sign_l1_action_sync(
        &self,
        action: Action,
        nonce: u64,
        vault_address: Option<Address>,
        chain: Chain,
    ) -> Result<serde_json::Value, CliError> {
        let prehash = action
            .prehash(nonce, vault_address, None, chain)
            .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to hash action: {err}")))?;
        // An action signed as an L1 action hashes to the `Agent` wrapper of its
        // msgpack hash; any other action is signed over its own typed data.
        let payload = match action.hash(nonce, vault_address, None) {
            Ok(connection_id) if agent_signing_hash(chain, connection_id) == prehash => {
                Payload::Agent {
                    chain,
                    connection_id,
                }
            }
            _ => Payload::Digest(prehash),
        };
        let signature = self.sign(&payload)?;
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

    /// Sign a user-signed action's EIP-712 document.
    pub fn sign_typed_data(&self, typed_data: &TypedData) -> Result<Signature, CliError> {
        self.sign(&Payload::UserSigned(typed_data))
    }

    /// Sign the msgpack hash of an L1 action hypersdk does not model.
    pub async fn sign_l1_connection_id(
        &self,
        chain: Chain,
        connection_id: B256,
    ) -> Result<Signature, CliError> {
        let payload = Payload::Agent {
            chain,
            connection_id,
        };
        match &self.backend {
            SignerBackend::Bankr(signer) => signer.sign_async(&payload).await,
            _ => self.sign(&payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::dyn_abi::{Eip712Types, Resolver};
    use alloy::sol_types::SolStruct;
    use hypersdk::hypercore::types::api::ApproveAgent;
    use hypersdk::hypercore::types::{Action, ScheduleCancel};

    alloy::sol! {
        struct UsdClassTransfer {
            string hyperliquidChain;
            string amount;
            bool toPerp;
            uint64 nonce;
        }

        struct Withdraw {
            string hyperliquidChain;
            string destination;
            string amount;
            uint64 time;
        }
    }

    const KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000009";

    fn key() -> PrivateKeySigner {
        KEY.parse().unwrap()
    }

    /// The typed data a user-signed command signs, built the way the commands build it.
    fn user_signed<T: SolStruct>(message: &impl serde::Serialize, chain: Chain) -> TypedData {
        let prefixed = format!("HyperliquidTransaction:{}", T::NAME);
        let mut resolver = Resolver::from_struct::<T>();
        resolver.ingest_string(T::eip712_encode_type()).unwrap();
        let mut types = Eip712Types::from(&resolver);
        let primary = types.remove(T::NAME).unwrap();
        types.insert(prefixed.clone(), primary);
        TypedData {
            domain: chain.domain(),
            resolver: Resolver::from(types),
            primary_type: prefixed,
            message: serde_json::to_value(message).unwrap(),
        }
    }

    fn usd_class_transfer(chain: Chain) -> TypedData {
        user_signed::<UsdClassTransfer>(
            &serde_json::json!({
                "hyperliquidChain": chain.to_string(),
                "amount": "12.5",
                "toPerp": true,
                "nonce": 1_700_000_000_123_u64,
            }),
            chain,
        )
    }

    fn withdraw(chain: Chain) -> TypedData {
        user_signed::<Withdraw>(
            &serde_json::json!({
                "hyperliquidChain": chain.to_string(),
                "destination": "0x0000000000000000000000000000000000000001",
                "amount": "5",
                "time": 1_700_000_000_123_u64,
            }),
            chain,
        )
    }

    fn approve_agent(chain: Chain) -> Action {
        Action::ApproveAgent(ApproveAgent {
            signature_chain_id: chain.arbitrum_id().to_owned(),
            hyperliquid_chain: chain,
            agent_address: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            agent_name: Some("bot".to_string()),
            nonce: 1_700_000_000_123,
        })
    }

    /// A Bankr signer whose API cannot be reached: any request it makes fails
    /// with a network error, so exit `13` proves the refusal came first.
    fn unreachable_bankr() -> SelectedSigner {
        SelectedSigner::bankr(
            BankrClient::new("http://127.0.0.1:1", "bk_test"),
            key().address(),
        )
    }

    #[test]
    fn local_signing_matches_hypersdks_own_action_signing() {
        let signer = SelectedSigner::private_key(key());
        let vault: Address = "0x0000000000000000000000000000000000000003"
            .parse()
            .unwrap();
        for chain in [Chain::Mainnet, Chain::Testnet] {
            for (action, vault_address) in [
                (Action::ScheduleCancel(ScheduleCancel { time: None }), None),
                (
                    Action::ScheduleCancel(ScheduleCancel { time: Some(9) }),
                    Some(vault),
                ),
                (approve_agent(chain), None),
            ] {
                let nonce = 1_700_000_000_123_u64;
                let expected = action
                    .clone()
                    .sign_sync(&key(), nonce, vault_address, None, chain)
                    .unwrap();
                let signed = signer
                    .sign_l1_action_sync(action, nonce, vault_address, chain)
                    .unwrap();
                assert_eq!(signed, serde_json::to_value(expected).unwrap(), "{chain:?}");
            }
        }
    }

    #[test]
    fn local_typed_data_and_connection_id_signatures_recover_to_the_key() {
        let signer = SelectedSigner::private_key(key());
        let typed = withdraw(Chain::Testnet);
        let signature = signer.sign_typed_data(&typed).unwrap();
        assert_eq!(
            alloy::primitives::Signature::from(signature)
                .recover_address_from_prehash(&typed.eip712_signing_hash().unwrap())
                .unwrap(),
            key().address()
        );

        let connection_id = B256::repeat_byte(0x22);
        let signature =
            block_on(signer.sign_l1_connection_id(Chain::Mainnet, connection_id)).unwrap();
        assert_eq!(
            alloy::primitives::Signature::from(signature)
                .recover_address_from_prehash(&agent_signing_hash(Chain::Mainnet, connection_id))
                .unwrap(),
            key().address()
        );
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn bankr_refuses_every_user_signed_payload_it_does_not_cover_before_a_request() {
        let signer = unreachable_bankr();
        for chain in [Chain::Mainnet, Chain::Testnet] {
            let err = signer.sign_typed_data(&withdraw(chain)).unwrap_err();
            assert_eq!(err.exit_code(), 13, "withdraw on {chain:?}: {err}");

            let err = signer
                .sign_l1_action_sync(approve_agent(chain), 1_700_000_000_123, None, chain)
                .unwrap_err();
            assert_eq!(err.exit_code(), 13, "approveAgent on {chain:?}: {err}");
        }
    }

    #[test]
    fn bankr_covers_trading_actions_and_the_usd_class_transfer() {
        // These reach the (unreachable) API, so they fail as network errors
        // rather than as refusals.
        let signer = unreachable_bankr();
        let err = signer
            .sign_l1_action_sync(
                Action::ScheduleCancel(ScheduleCancel { time: None }),
                1_700_000_000_123,
                None,
                Chain::Mainnet,
            )
            .unwrap_err();
        assert_ne!(err.exit_code(), 13, "{err}");
        let err = signer
            .sign_typed_data(&usd_class_transfer(Chain::Mainnet))
            .unwrap_err();
        assert_ne!(err.exit_code(), 13, "{err}");
    }

    #[test]
    fn bankr_documents_hash_exactly_like_the_typed_data_they_stand_for() {
        for chain in [Chain::Mainnet, Chain::Testnet] {
            let typed = usd_class_transfer(chain);
            let document =
                crate::bankr::BankrTypedData::for_payload(&Payload::UserSigned(&typed)).unwrap();
            assert_eq!(
                document.signing_hash().unwrap(),
                typed.eip712_signing_hash().unwrap(),
                "{chain:?}"
            );
            assert_eq!(
                document.document()["domain"]["chainId"],
                if chain.is_mainnet() { 42161 } else { 421_614 },
                "the chain id goes to Bankr as a decimal number"
            );
        }
    }

    #[test]
    fn user_signed_capability_is_asked_of_the_provider_before_signing() {
        assert!(
            SelectedSigner::private_key(key())
                .ensure_can_sign_user_actions()
                .is_ok()
        );
        let err = unreachable_bankr()
            .ensure_can_sign_user_actions()
            .unwrap_err();
        assert_eq!(err.exit_code(), 13);
        let address_only = SelectedSigner::ows_address_only(
            "0x0000000000000000000000000000000000000001".to_string(),
            "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            address_only
                .ensure_can_sign_user_actions()
                .unwrap_err()
                .exit_code(),
            13
        );
    }

    #[test]
    fn describe_names_each_signer_kind() {
        let alias = |signer: &SelectedSigner| signer.describe();
        assert_eq!(
            alias(&SelectedSigner::private_key(key())).source,
            "private key/config/env"
        );
        assert_eq!(alias(&SelectedSigner::keystore(key())).source, "keystore");
        let stored = SelectedSigner::stored_account(
            key(),
            "main".to_string(),
            "0x0000000000000000000000000000000000000004"
                .parse()
                .unwrap(),
        );
        assert_eq!(stored.describe().alias.as_deref(), Some("main"));
        assert!(stored.is_stored_agent());
        assert!(!SelectedSigner::private_key(key()).is_stored_agent());
        assert!(
            unreachable_bankr()
                .describe()
                .source
                .starts_with("Bankr hosted wallet (0x")
        );
    }
}
