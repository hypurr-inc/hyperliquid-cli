//! What gets signed, before and after it exists: [`Signs`] names a payload's
//! kind so a command's spec can state it, [`Payload`] is the document itself.
//!
//! A leaf: the providers (`bankr`, `ows::signing`) and the specs import this,
//! never the selected-signer module that consumes them. [`Payload::signs`]
//! derives the same kind from a payload that a spec declares, so the early and
//! the late answer are one rule, and [`ensure_supports_expires_after`] states
//! once which kinds can carry `--expires-after`.

use alloy::dyn_abi::TypedData;
use alloy::primitives::B256;
use alloy::sol_types::SolStruct;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::signing::agent_signing_hash;

use crate::errors::CliError;

/// The prefix of a user-signed action's EIP-712 primary type.
pub(crate) const USER_SIGNED_PREFIX: &str = "HyperliquidTransaction:";

/// The kind of signature a command asks for, known before anything is signed:
/// what a [`CommandSpec`](crate::command_spec::CommandSpec) declares and what
/// each provider answers [`SelectedSigner::ensure_covers`](super::SelectedSigner::ensure_covers) for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signs<'a> {
    /// An L1 action: trading, margin, vault and subaccount actions.
    L1Action,
    /// A user-signed action's EIP-712 document, by struct name.
    UserSigned(&'a str),
    /// A user-signed [`Action`](hypersdk::hypercore::types::Action) whose
    /// document hypersdk does not expose.
    Digest,
}

impl Signs<'static> {
    #[must_use]
    pub const fn user_signed<T: SolStruct>() -> Self {
        Self::UserSigned(T::NAME)
    }
}

impl Signs<'_> {
    /// Whether an action of this kind can carry `expiresAfter`: only an L1
    /// action hashes it. A user-signed action signs a document with no such
    /// field, and the exchange rejects the request field beside it
    /// (<https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint#expires-after>).
    #[must_use]
    pub const fn carries_expires_after(self) -> bool {
        matches!(self, Self::L1Action)
    }
}

pub(crate) fn expires_after_refusal() -> CliError {
    CliError::Unsupported(
        "--expires-after applies to L1 actions only (orders, cancels, modifies, leverage, margin, TWAP, vault, subaccount and similar); user-signed actions such as transfers, withdrawals, staking deposits and delegations, API wallet and builder approvals and abstraction changes do not support expiresAfter"
            .to_string(),
    )
}

/// Whether a command that signs `signs` can carry `--expires-after`. The one
/// rule: the run pipeline asks it for a command's spec before anything runs,
/// and [`SelectedSigner::sign_l1_action_sync`](super::SelectedSigner::sign_l1_action_sync)
/// asks it again for the payload it just derived.
pub fn ensure_supports_expires_after(signs: Option<Signs<'_>>) -> Result<(), CliError> {
    match signs {
        Some(signs) if signs.carries_expires_after() => Ok(()),
        Some(_) => Err(expires_after_refusal()),
        None => Err(CliError::Unsupported(
            "--expires-after applies to commands that sign an L1 action; this command signs nothing"
                .to_string(),
        )),
    }
}

pub(crate) enum Payload<'a> {
    /// An L1 action: its msgpack hash wrapped in the `Agent` struct.
    Agent { chain: Chain, connection_id: B256 },
    /// A user-signed action's EIP-712 document.
    UserSigned(&'a TypedData),
    /// The EIP-712 digest of a user-signed [`Action`](hypersdk::hypercore::types::Action)
    /// whose document hypersdk does not expose.
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

    /// The kind of signature this payload is, as a command's spec would declare it.
    pub(crate) fn signs(&self) -> Signs<'_> {
        match self {
            Self::Agent { .. } => Signs::L1Action,
            Self::UserSigned(typed_data) => Signs::UserSigned(
                typed_data
                    .primary_type
                    .strip_prefix(USER_SIGNED_PREFIX)
                    .unwrap_or(&typed_data.primary_type),
            ),
            Self::Digest(_) => Signs::Digest,
        }
    }
}
