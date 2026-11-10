//! The wallet commands that messages tell the user to run, spelled once so a
//! rename of one command cannot strand advice in the vault, the store, the
//! Bankr client and the wallet commands.

pub const SETUP: &str = "hyperliquid setup";
pub const SET_DEFAULT: &str = "hyperliquid account set-default <WALLET>";
pub const WALLET_CREATE: &str = "hyperliquid wallet create";
pub const WALLET_IMPORT: &str = "hyperliquid wallet import";
pub const WALLET_IMPORT_MNEMONIC: &str = "hyperliquid wallet import-mnemonic";
pub const WALLET_LOGIN: &str = "hyperliquid wallet login";

/// Back a wallet up before deleting it.
pub fn wallet_export(wallet: &str) -> String {
    format!("hyperliquid wallet export {wallet}")
}

/// Remove a wallet the CLI could not roll back.
pub fn wallet_delete(wallet: &str) -> String {
    format!("hyperliquid wallet delete {wallet}")
}
