//! The selected signer: a local private key, an OWS vault wallet or a hosted
//! Bankr wallet behind one signing interface.
//!
//! Every signature the CLI makes is one of three [`Payload`]s, and each
//! provider answers once, in [`SelectedSigner::sign`], which payloads it can
//! sign: local keys sign any digest, an OWS wallet signs any digest once a wallet
//! resolved, and Bankr signs only the typed-data documents it can rebuild.
//! [`Signs`] names a payload's kind before it exists, so a command's spec can
//! state what it signs and [`SelectedSigner::ensure_covers`] can refuse early;
//! [`Payload::signs`] derives the same kind from a payload, so the early and the
//! late answer are one rule. Both live in the [`payload`] leaf, which the
//! providers import instead of this module. Every signature a remote provider
//! returns is checked, once, against the payload's own digest and the selected
//! address.

pub(crate) mod nonce;
pub(crate) mod payload;
pub(crate) mod signature;

use alloy::dyn_abi::TypedData;
use alloy::primitives::B256;
use alloy::signers::SignerSync;
use chrono::{DateTime, Utc};
use hypersdk::Address;
use hypersdk::hypercore::signing::agent_signing_hash;
use hypersdk::hypercore::types::{Action, ActionRequest, Signature};
use hypersdk::hypercore::{Chain, PrivateKeySigner};
use ows_lib::types::WalletInfo;

pub(crate) use self::payload::{Payload, USER_SIGNED_PREFIX};
pub use self::payload::{Signs, ensure_supports_expires_after};
use self::signature::Provider;
use crate::bankr::{self, BankrClient, BankrSigner};
use crate::errors::CliError;
use crate::ows::signing::{self as ows_signing, OwsSigningConfig};
use crate::ows::vault::Vault;

/// What a signing key is to the exchange: whether it acts for its own account
/// or for a master's. An API wallet can trade for its master but not approve
/// agents or withdraw, and holds no account data of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerRole {
    /// Acts for its own account: the exchange answered and it is no API wallet.
    Master,
    /// An API wallet signing for `master`, as a stored account records or as
    /// the exchange reports.
    Agent { master: Address },
    /// Nothing proves which. A key that no stored account describes, whose
    /// role was not asked of the exchange or could not be read.
    Unverified,
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

    /// A wallet in `vault`, signing as its Hyperliquid account.
    pub fn vault_wallet(vault: &Vault, wallet: &WalletInfo) -> Result<Self, CliError> {
        let account = Vault::hyperliquid_account(wallet)?;
        Ok(Self::ows(OwsSigningConfig::for_wallet(
            wallet,
            account.address,
            vault.clone(),
        )))
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

    /// What the account store alone says this signer is: a stored account
    /// whose data lives under another address is an agent of that master.
    /// Every other signer is [`SignerRole::Unverified`] here; only the
    /// exchange can say ([`Invocation::signer_role`](crate::invocation::Invocation::signer_role)).
    /// The one offline definition of an agent wallet, which `master_only` and
    /// the agent form of an action use through the invocation.
    #[must_use]
    pub fn local_role(&self) -> SignerRole {
        match self.query_address() {
            master if master != self.address() => SignerRole::Agent { master },
            _ => SignerRole::Unverified,
        }
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

    /// Whether this provider signs `signs` at all: a local key and an OWS wallet
    /// sign anything, Bankr only what [`bankr::covers`] says. Holds whether or
    /// not a key is at hand, so a dry run can ask it too.
    pub fn ensure_covers(&self, signs: Signs<'_>) -> Result<(), CliError> {
        match &self.backend {
            SignerBackend::Local { .. } | SignerBackend::Ows(_) => Ok(()),
            SignerBackend::Bankr(_) => bankr::ensure_covers(signs),
        }
    }

    /// Whether a key stands behind this identity: an address-only `--ows-signer`
    /// names an account to preview, not one to sign for.
    pub fn ensure_holds_key(&self) -> Result<(), CliError> {
        match &self.backend {
            SignerBackend::Ows(config) => config.ensure_signable(),
            SignerBackend::Local { .. } | SignerBackend::Bankr(_) => Ok(()),
        }
    }

    fn sign(&self, payload: &Payload<'_>) -> Result<Signature, CliError> {
        let digest = payload.digest()?;
        let signature = match &self.backend {
            SignerBackend::Local { signer, .. } => signer
                .sign_hash_sync(&digest)
                .map(Into::into)
                .map_err(|err| {
                CliError::Internal(anyhow::anyhow!("failed to sign action: {err}"))
            })?,
            SignerBackend::Ows(config) => ows_signing::sign(config, payload, digest)?,
            SignerBackend::Bankr(signer) => signer.sign(payload)?,
        };
        self.verified(signature, digest)
    }

    /// [`Self::sign`] for an async caller: Bankr's request is awaited instead of
    /// run on a scoped runtime thread.
    async fn sign_async(&self, payload: &Payload<'_>) -> Result<Signature, CliError> {
        let SignerBackend::Bankr(signer) = &self.backend else {
            return self.sign(payload);
        };
        let digest = payload.digest()?;
        let signature = signer.sign_async(payload).await?;
        self.verified(signature, digest)
    }

    /// A remote provider's signature, proven to be over `digest` by the address
    /// that was selected. A local key needs no proof.
    fn verified(&self, signature: Signature, digest: B256) -> Result<Signature, CliError> {
        match &self.backend {
            SignerBackend::Local { .. } => {}
            SignerBackend::Ows(config) => {
                Provider::OWS.verify(&signature, digest, config.address())?;
            }
            SignerBackend::Bankr(signer) => {
                Provider::BANKR.verify(&signature, digest, signer.address())?;
            }
        }
        Ok(signature)
    }

    /// Sign `action` and build its request body. `expires_after` is a
    /// millisecond timestamp. An L1 action hashes it, so the signature covers
    /// it; a user-signed action signs a document with no such field, and the
    /// exchange refuses the field there, so that is an error here too.
    pub fn sign_l1_action_sync(
        &self,
        action: Action,
        nonce: u64,
        vault_address: Option<Address>,
        expires_after: Option<u64>,
        chain: Chain,
    ) -> Result<serde_json::Value, CliError> {
        let expiry = expires_after
            .map(|ms| {
                i64::try_from(ms)
                    .ok()
                    .and_then(DateTime::<Utc>::from_timestamp_millis)
                    .ok_or_else(|| {
                        CliError::Internal(anyhow::anyhow!("expiry {ms} is not a valid timestamp"))
                    })
            })
            .transpose()?;
        let prehash = action
            .prehash(nonce, vault_address, expiry, chain)
            .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to hash action: {err}")))?;
        // An action signed as an L1 action hashes to the `Agent` wrapper of its
        // msgpack hash; any other action is signed over its own typed data.
        let payload = match action.hash(nonce, vault_address, expires_after) {
            Ok(connection_id) if agent_signing_hash(chain, connection_id) == prehash => {
                Payload::Agent {
                    chain,
                    connection_id,
                }
            }
            _ => Payload::Digest(prehash),
        };
        if expires_after.is_some() {
            ensure_supports_expires_after(Some(payload.signs()))?;
        }
        let signature = self.sign(&payload)?;
        let request = ActionRequest {
            action,
            nonce,
            signature,
            vault_address,
            expires_after,
        };
        serde_json::to_value(request).map_err(|err| {
            CliError::Internal(anyhow::anyhow!("failed to encode signed action: {err}"))
        })
    }

    pub fn sign_typed_data(&self, typed_data: &TypedData) -> Result<Signature, CliError> {
        self.sign(&Payload::UserSigned(typed_data))
    }

    /// Sign the msgpack hash of an L1 action hypersdk does not model.
    pub async fn sign_l1_connection_id(
        &self,
        chain: Chain,
        connection_id: B256,
    ) -> Result<Signature, CliError> {
        self.sign_async(&Payload::Agent {
            chain,
            connection_id,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ows::vault::test_support::import_test_wallet;
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
        let prefixed = format!("{USER_SIGNED_PREFIX}{}", T::NAME);
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
            BankrClient::new("http://127.0.0.1:1", "bk_test").unwrap(),
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
            for (action, vault_address, expires_after) in [
                (
                    Action::ScheduleCancel(ScheduleCancel { time: None }),
                    None,
                    None,
                ),
                (
                    Action::ScheduleCancel(ScheduleCancel { time: Some(9) }),
                    Some(vault),
                    None,
                ),
                (approve_agent(chain), None, None),
                (
                    Action::ScheduleCancel(ScheduleCancel { time: None }),
                    None,
                    Some(1_700_000_030_123_u64),
                ),
                (
                    Action::ScheduleCancel(ScheduleCancel { time: Some(9) }),
                    Some(vault),
                    Some(1_700_000_030_123),
                ),
            ] {
                let nonce = 1_700_000_000_123_u64;
                let expected = action
                    .clone()
                    .sign_sync(
                        &key(),
                        nonce,
                        vault_address,
                        expires_after
                            .and_then(|ms| DateTime::<Utc>::from_timestamp_millis(ms as i64)),
                        chain,
                    )
                    .unwrap();
                let signed = signer
                    .sign_l1_action_sync(action, nonce, vault_address, expires_after, chain)
                    .unwrap();
                assert_eq!(signed, serde_json::to_value(expected).unwrap(), "{chain:?}");
                assert_eq!(signed["expiresAfter"], serde_json::json!(expires_after));
            }
        }
    }

    #[test]
    fn an_expiry_changes_what_an_l1_action_signs() {
        let signer = SelectedSigner::private_key(key());
        let action = || Action::ScheduleCancel(ScheduleCancel { time: None });
        let sign = |expires_after| {
            signer
                .sign_l1_action_sync(
                    action(),
                    1_700_000_000_123,
                    None,
                    expires_after,
                    Chain::Testnet,
                )
                .unwrap()["signature"]
                .clone()
        };
        assert_ne!(sign(None), sign(Some(1_700_000_030_123)));
        assert_ne!(sign(Some(1_700_000_030_123)), sign(Some(1_700_000_030_124)));
    }

    #[test]
    fn a_user_signed_action_cannot_carry_an_expiry() {
        let signer = SelectedSigner::private_key(key());
        for chain in [Chain::Mainnet, Chain::Testnet] {
            let err = signer
                .sign_l1_action_sync(
                    approve_agent(chain),
                    1_700_000_000_123,
                    None,
                    Some(1_700_000_030_123),
                    chain,
                )
                .unwrap_err();
            assert_eq!(err.exit_code(), 13, "{err}");
            assert_eq!(
                err.to_string(),
                ensure_supports_expires_after(Some(Signs::Digest))
                    .unwrap_err()
                    .to_string(),
                "the refusal at signing time is the one the spec check gives"
            );
        }
    }

    #[test]
    fn only_commands_that_sign_an_l1_action_accept_an_expiry() {
        assert!(ensure_supports_expires_after(Some(Signs::L1Action)).is_ok());
        for signs in [
            Some(Signs::Digest),
            Some(Signs::UserSigned("Withdraw")),
            None,
        ] {
            let err = ensure_supports_expires_after(signs).unwrap_err();
            assert_eq!(err.exit_code(), 13, "{signs:?}");
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
                .sign_l1_action_sync(approve_agent(chain), 1_700_000_000_123, None, None, chain)
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
            let document = crate::bankr::document(&Payload::UserSigned(&typed)).unwrap();
            let sent: TypedData = serde_json::from_value(document.clone()).unwrap();
            assert_eq!(
                sent.eip712_signing_hash().unwrap(),
                typed.eip712_signing_hash().unwrap(),
                "{chain:?}"
            );
            assert_eq!(
                document["domain"]["chainId"],
                if chain.is_mainnet() { 42161 } else { 421_614 },
                "the chain id goes to Bankr as a decimal number"
            );
        }
    }

    #[test]
    fn covering_is_asked_of_the_provider_and_holding_a_key_of_the_identity() {
        let usd_class = Signs::user_signed::<UsdClassTransfer>();
        let withdraw = Signs::user_signed::<Withdraw>();
        let address_only = SelectedSigner::ows_address_only(
            "0x0000000000000000000000000000000000000001".to_string(),
            "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
        );

        for signs in [Signs::L1Action, usd_class, withdraw, Signs::Digest] {
            let local = SelectedSigner::private_key(key());
            assert!(local.ensure_covers(signs).is_ok(), "{signs:?}");
            assert!(local.ensure_holds_key().is_ok(), "{signs:?}");
            assert!(address_only.ensure_covers(signs).is_ok(), "{signs:?}");
        }
        assert_eq!(address_only.ensure_holds_key().unwrap_err().exit_code(), 13);

        let bankr = unreachable_bankr();
        assert!(bankr.ensure_holds_key().is_ok());
        assert!(bankr.ensure_covers(Signs::L1Action).is_ok());
        assert!(bankr.ensure_covers(usd_class).is_ok());
        for refused in [withdraw, Signs::Digest] {
            assert_eq!(
                bankr.ensure_covers(refused).unwrap_err().exit_code(),
                13,
                "{refused:?}"
            );
        }
    }

    #[test]
    fn a_payload_is_the_kind_its_spec_declares() {
        let usd_class = usd_class_transfer(Chain::Mainnet);
        let withdraw = withdraw(Chain::Mainnet);
        assert_eq!(
            Payload::UserSigned(&usd_class).signs(),
            Signs::user_signed::<UsdClassTransfer>()
        );
        assert_eq!(
            Payload::UserSigned(&withdraw).signs(),
            Signs::user_signed::<Withdraw>()
        );
        assert_eq!(
            Payload::Agent {
                chain: Chain::Mainnet,
                connection_id: B256::ZERO
            }
            .signs(),
            Signs::L1Action
        );
        assert_eq!(Payload::Digest(B256::ZERO).signs(), Signs::Digest);
    }

    #[test]
    fn a_declared_kind_is_covered_exactly_when_its_payload_gets_a_document() {
        for (typed, signs) in [
            (
                usd_class_transfer(Chain::Mainnet),
                Signs::user_signed::<UsdClassTransfer>(),
            ),
            (withdraw(Chain::Mainnet), Signs::user_signed::<Withdraw>()),
        ] {
            assert_eq!(
                crate::bankr::covers(signs),
                crate::bankr::document(&Payload::UserSigned(&typed)).is_ok(),
                "{signs:?}"
            );
        }
    }

    #[test]
    fn a_signature_from_another_address_is_refused_for_every_payload_kind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wallet = import_test_wallet(tmp.path(), "test-wallet");
        let other: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let signer = SelectedSigner::ows(OwsSigningConfig::for_wallet(
            &wallet,
            other,
            Vault::at(tmp.path()),
        ));

        let chain = Chain::Testnet;
        let results = [
            signer.sign_typed_data(&withdraw(chain)).map(|_| ()),
            signer
                .sign_l1_action_sync(
                    Action::ScheduleCancel(ScheduleCancel { time: None }),
                    1,
                    None,
                    None,
                    chain,
                )
                .map(|_| ()),
            signer
                .sign_l1_action_sync(approve_agent(chain), 1, None, None, chain)
                .map(|_| ()),
            block_on(signer.sign_l1_connection_id(chain, B256::repeat_byte(3))).map(|_| ()),
        ];
        for result in results {
            let err = result.unwrap_err();
            assert_eq!(err.exit_code(), 10, "{err}");
            assert!(err.to_string().contains("ows_signer_mismatch"), "{err}");
        }
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
        assert!(
            unreachable_bankr()
                .describe()
                .source
                .starts_with("Bankr hosted wallet (0x")
        );
    }

    #[test]
    fn only_a_stored_account_that_records_a_master_is_known_offline_to_be_an_agent() {
        let master: Address = "0x0000000000000000000000000000000000000004"
            .parse()
            .unwrap();
        assert_eq!(
            SelectedSigner::stored_account(key(), "agent".to_string(), master).local_role(),
            SignerRole::Agent { master }
        );
        let own = SelectedSigner::private_key(key());
        let stored_master = SelectedSigner::stored_account(key(), "m".to_string(), own.address());
        for signer in [own, stored_master, SelectedSigner::keystore(key())] {
            assert_eq!(signer.local_role(), SignerRole::Unverified);
        }
    }
}
