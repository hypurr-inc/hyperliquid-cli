//! Rows and receipts printed by the wallet and account commands.

use serde::Serialize;

use crate::invocation::Invocation;
use crate::output::{OutputFormat, TableData};
use crate::settings::Settings;
use ows_lib::types::WalletInfo as VaultWalletInfo;

use crate::ows::vault::{HyperliquidAccount, Vault};

/// `type` of every `account ls` row: all listed wallets live in the OWS vault.
const OWS_WALLET_TYPE: &str = "ows-wallet";

/// One `account ls` row. The shape predates the vault: `id` is a 1-based
/// display index.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AccountRow {
    pub id: i64,
    pub alias: String,
    pub address: String,
    #[serde(rename = "type")]
    pub account_type: String,
    pub is_default: bool,
    pub created_at: String,
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
            account_type: OWS_WALLET_TYPE.to_string(),
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

fn account_type_label(account_type: &str) -> &str {
    if account_type == OWS_WALLET_TYPE {
        "OWS wallet"
    } else {
        account_type
    }
}

impl TableData for AccountsOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "ID",
            "Alias",
            "Address",
            "Type",
            "Default",
            "Created At",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.accounts
            .iter()
            .map(|account| {
                vec![
                    account.id.to_string(),
                    account.alias.clone(),
                    account.address.clone(),
                    account_type_label(&account.account_type).to_string(),
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
        serde_json::to_value(&self.accounts).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct WalletReceipt {
    message: String,
    address: String,
    alias: Option<String>,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_default: Option<bool>,
    config_path: String,
    vault_path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config_keys_written: Vec<String>,
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
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
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
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
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
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// What a wallet command reports about one wallet.
pub(super) struct Receipt {
    pub message: String,
    pub address: String,
    pub alias: Option<String>,
    pub source: String,
    pub is_default: Option<bool>,
    pub config_keys_written: Vec<String>,
}

impl Receipt {
    pub fn new(message: &str, address: &str, alias: Option<String>, source: &str) -> Self {
        Self {
            message: message.to_string(),
            address: address.to_string(),
            alias,
            source: source.to_string(),
            is_default: None,
            config_keys_written: Vec::new(),
        }
    }
}

pub(super) fn print_receipt(inv: &Invocation, settings: &Settings, receipt: Receipt) {
    let info = WalletReceipt {
        message: receipt.message,
        address: receipt.address,
        alias: receipt.alias,
        source: receipt.source,
        is_default: receipt.is_default,
        config_path: config_path(settings),
        vault_path: Vault::from_settings(settings).display_path(),
        config_keys_written: receipt.config_keys_written,
    };
    inv.print_untimed(&info);
}

fn config_path(settings: &Settings) -> String {
    settings.config_file().map_or_else(
        || "unavailable".to_string(),
        |file| file.path().display().to_string(),
    )
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

/// Report a declined confirmation. Human runs get a stderr note; JSON keeps its status document.
pub(super) fn print_declined(inv: &Invocation, message: &str) {
    if inv.format() == OutputFormat::Json {
        print_status(inv, "cancelled", message);
    } else {
        eprintln!("{message}");
    }
}

/// What `wallet login` and `wallet logout` report about the Bankr link.
pub(super) struct BankrReceipt {
    pub status: &'static str,
    pub message: String,
    pub address: Option<String>,
    pub api_url: Option<String>,
    pub config_keys_written: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BankrStatusOutput {
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_url: Option<String>,
    config_path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config_keys_written: Vec<String>,
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
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

pub(super) fn print_bankr_receipt(inv: &Invocation, settings: &Settings, receipt: BankrReceipt) {
    inv.print_untimed(&BankrStatusOutput {
        status: receipt.status.to_string(),
        message: receipt.message,
        address: receipt.address,
        api_url: receipt.api_url,
        config_path: config_path(settings),
        config_keys_written: receipt.config_keys_written,
    });
}

/// Quote a value for display in a copy-pasteable shell command.
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
    fn accounts_output_hides_secrets_and_labels_vault_wallets() {
        let output = AccountsOutput::new(vec![AccountRow {
            id: 1,
            alias: "main".to_string(),
            address: "0xabc".to_string(),
            account_type: OWS_WALLET_TYPE.to_string(),
            is_default: true,
            created_at: "now".to_string(),
        }]);
        let rendered = crate::output::Session::for_test(OutputFormat::Json).render(&output);

        assert!(rendered.contains("main"));
        assert!(rendered.contains("\"type\":\"ows-wallet\""));
        assert!(!rendered.contains("private_key"));
        assert!(!rendered.contains("encrypted"));
        assert_eq!(output.rows()[0][3], "OWS wallet");
    }

    #[test]
    fn shell_arg_quotes_only_when_needed() {
        assert_eq!(shell_arg("main"), "main");
        assert_eq!(shell_arg("my wallet"), "'my wallet'");
        assert_eq!(shell_arg("it's"), "'it'\\''s'");
    }
}
