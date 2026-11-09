//! Open Wallet Standard integration.
//!
//! OWS is the only managed wallet backend. [`vault`] owns the wallet lifecycle
//! (create, import, list, rename, export, delete, default selection) and
//! [`signing`] owns signing with a vault wallet.

pub mod signing;
pub mod vault;

use crate::errors::CliError;

/// The Hyperliquid chain CAIP-2 identifier used for account derivation and lookup.
pub const HYPERLIQUID_CAIP2: &str = "eip155:999";

/// Hyperliquid uses Ethereum-compatible secp256k1 keys, so wallets without an
/// explicit Hyperliquid account fall back to their standard EVM account.
const EVM_CAIP2: &str = "eip155:1";

const HYPERLIQUID_CHAIN_ALIAS: &str = "hyperliquid";

/// How a vault wallet is named where its origin matters: in `wallet show`, the
/// receipts of the wallet commands and `account ls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletSource {
    /// A wallet in the vault.
    Vault,
    /// A wallet this run just created or imported.
    Stored,
}

impl WalletSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Vault => "OWS wallet",
            Self::Stored => "stored OWS wallet",
        }
    }

    /// `OWS wallet (<name>)`: how a signer backed by vault wallet `name` is described.
    #[must_use]
    pub fn describe(self, name: &str) -> String {
        format!("{} ({name})", self.label())
    }
}

/// A selector that names several wallets: the user must pick by id.
pub(crate) fn ambiguous_wallet(selector: &str, count: usize) -> CliError {
    CliError::InvalidAuth(format!(
        "ows_ambiguous_wallet: OWS wallet selector '{selector}' matched {count} wallets; use the wallet id"
    ))
}

fn map_ows_error(err: ows_lib::OwsLibError) -> CliError {
    use ows_lib::OwsLibError;

    match err {
        OwsLibError::WalletNotFound(wallet) => CliError::OwsWalletNotFound { wallet },
        OwsLibError::AmbiguousWallet { name, count } => ambiguous_wallet(&name, count),
        OwsLibError::WalletNameExists(name) => CliError::Unsupported(format!(
            "ows_wallet_name_exists: a wallet named '{name}' already exists"
        )),
        OwsLibError::InvalidInput(message) => {
            CliError::Unsupported(format!("ows_unsupported_method: {message}"))
        }
        OwsLibError::Io(err) => CliError::Unavailable(format!("ows_unavailable: {err}")),
        OwsLibError::Crypto(err) => CliError::InvalidAuth(format!(
            "ows_wallet_locked: failed to decrypt OWS wallet: {err}"
        )),
        OwsLibError::Signer(err) => CliError::InvalidAuth(format!("ows_signing_failed: {err}")),
        OwsLibError::Core(err) => CliError::Unsupported(format!("ows_chain_mismatch: {err}")),
        other => CliError::InvalidAuth(format!("ows_malformed_response: {other}")),
    }
}
