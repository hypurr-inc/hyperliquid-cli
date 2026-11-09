use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::invocation::Invocation;
use crate::output::{TableData, serialize_row};
use crate::ows::WalletSource;
use crate::ows::vault::{HyperliquidAccount, Vault};
use crate::terminal::OutputFormat;
use ows_lib::types::WalletInfo as VaultWalletInfo;

/// The `type` every `account ls` row reports: the vault is the only backend.
const OWS_WALLET_TYPE: &str = "ows-wallet";

/// `id` is a 1-based display index, and `type` (always [`OWS_WALLET_TYPE`]) is only on
/// the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: i64,
    pub alias: String,
    pub address: String,
    pub is_default: bool,
    pub created_at: String,
}

impl Serialize for AccountRow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut row = serializer.serialize_struct("AccountRow", 6)?;
        row.serialize_field("id", &self.id)?;
        row.serialize_field("alias", &self.alias)?;
        row.serialize_field("address", &self.address)?;
        row.serialize_field("type", OWS_WALLET_TYPE)?;
        row.serialize_field("is_default", &self.is_default)?;
        row.serialize_field("created_at", &self.created_at)?;
        row.end()
    }
}

impl AccountRow {
    pub(super) fn for_wallet(
        display_index: usize,
        wallet: VaultWalletInfo,
        account: &HyperliquidAccount,
        is_default: bool,
    ) -> Self {
        Self {
            id: display_index as i64,
            alias: wallet.name,
            address: account.address_text.clone(),
            is_default,
            created_at: wallet.created_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountsOutput {
    accounts: Vec<AccountRow>,
}

impl AccountsOutput {
    #[must_use]
    pub fn new(accounts: Vec<AccountRow>) -> Self {
        Self { accounts }
    }
}

impl TableData for AccountsOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["ID", "Alias", "Address", "Type", "Default", "Created At"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.accounts
            .iter()
            .map(|account| {
                vec![
                    account.id.to_string(),
                    account.alias.clone(),
                    account.address.clone(),
                    WalletSource::Vault.label().to_string(),
                    if account.is_default { "yes" } else { "no" }.to_string(),
                    account.created_at.clone(),
                ]
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No accounts.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serialize_row(&self.accounts)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct WalletReceipt {
    pub message: String,
    pub address: String,
    pub alias: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_default: Option<bool>,
    config_path: String,
    vault_path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config_keys_written: Vec<String>,
}

impl WalletReceipt {
    pub fn new(
        inv: &Invocation,
        message: &str,
        address: &str,
        alias: Option<String>,
        source: &str,
    ) -> Self {
        Self {
            message: message.to_string(),
            address: address.to_string(),
            alias,
            source: source.to_string(),
            is_default: None,
            config_path: inv.settings().config_path_display(),
            vault_path: Vault::from_settings(inv.settings()).display_path(),
            config_keys_written: Vec::new(),
        }
    }
}

impl TableData for WalletReceipt {
    fn headers(&self) -> Vec<&str> {
        vec!["Field", "Value"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let mut rows = vec![
            vec!["Message".to_string(), self.message.clone()],
            vec!["Address".to_string(), self.address.clone()],
            vec![
                "Alias".to_string(),
                self.alias.clone().unwrap_or_else(|| "n/a".to_string()),
            ],
            vec!["Source".to_string(), self.source.clone()],
            vec![
                "Default".to_string(),
                self.is_default
                    .map(|is_default| if is_default { "yes" } else { "no" }.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
            ],
            vec!["Config".to_string(), self.config_path.clone()],
            vec!["Vault".to_string(), self.vault_path.clone()],
        ];
        if !self.config_keys_written.is_empty() {
            rows.push(vec![
                "Config keys written".to_string(),
                self.config_keys_written.join(", "),
            ]);
        }
        rows
    }

    fn to_json_value(&self) -> serde_json::Value {
        serialize_row(self)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct WalletAddressOutput {
    address: String,
}

impl TableData for WalletAddressOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Address"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.address.clone()]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serialize_row(self)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct StatusOutput {
    status: String,
    message: String,
}

impl TableData for StatusOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Status", "Message"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.status.clone(), self.message.clone()]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serialize_row(self)
    }
}

pub(super) fn print_wallet_address(inv: &Invocation, address: &str) {
    if inv.format() == OutputFormat::Json {
        inv.print_untimed(&WalletAddressOutput {
            address: address.to_string(),
        });
    } else {
        println!("{address}");
    }
}

pub(super) fn print_status(inv: &Invocation, status: &str, message: &str) {
    if inv.format() == OutputFormat::Json {
        inv.print_untimed(&StatusOutput {
            status: status.to_string(),
            message: message.to_string(),
        });
    } else {
        println!("{message}");
    }
}

/// Human runs get a stderr note; JSON keeps its status document.
pub(super) fn print_declined(inv: &Invocation, message: &str) {
    if inv.format() == OutputFormat::Json {
        print_status(inv, "cancelled", message);
    } else {
        eprintln!("{message}");
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BankrStatusOutput {
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    config_path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config_keys_written: Vec<String>,
}

impl BankrStatusOutput {
    pub fn new(inv: &Invocation, status: &str, message: String) -> Self {
        Self {
            status: status.to_string(),
            message,
            address: None,
            api_url: None,
            config_path: inv.settings().config_path_display(),
            config_keys_written: Vec::new(),
        }
    }
}

impl TableData for BankrStatusOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Field", "Value"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let mut rows = vec![
            vec!["Status".to_string(), self.status.clone()],
            vec!["Message".to_string(), self.message.clone()],
        ];
        if let Some(address) = &self.address {
            rows.push(vec!["Address".to_string(), address.clone()]);
        }
        if let Some(api_url) = &self.api_url {
            rows.push(vec!["Bankr API".to_string(), api_url.clone()]);
        }
        rows.push(vec!["Config".to_string(), self.config_path.clone()]);
        if !self.config_keys_written.is_empty() {
            rows.push(vec![
                "Config keys written".to_string(),
                self.config_keys_written.join(", "),
            ]);
        }
        rows
    }

    fn to_json_value(&self) -> serde_json::Value {
        serialize_row(self)
    }
}

pub(super) fn shell_arg(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_arg_quotes_only_when_needed() {
        assert_eq!(shell_arg("main"), "main");
        assert_eq!(shell_arg("my wallet"), "'my wallet'");
        assert_eq!(shell_arg("it's"), "'it'\\''s'");
    }
}
