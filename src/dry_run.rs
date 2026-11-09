//! The dry-run preview envelope: what a command prints under `--dry-run`.
//!
//! Dry-run output is a public CLI contract. Commands do not build envelopes
//! themselves; `commands::support::print_dry_run` assembles one from the
//! invocation (command name, payload, network, signer) and the command's
//! `would_execute` name and `args`.

use serde::Serialize;

use crate::network::Network;
use crate::output::TableData;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionReversibility {
    Reversible,
    PartiallyReversible,
    Irreversible,
}

impl ActionReversibility {
    /// Snake-case label used in previews and receipt rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reversible => "reversible",
            Self::PartiallyReversible => "partially_reversible",
            Self::Irreversible => "irreversible",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunSigning {
    /// The signing address, or why it could not be resolved without prompting.
    signer: Result<String, String>,
    acting_as: Option<String>,
    vault_address: Option<String>,
}

impl DryRunSigning {
    #[must_use]
    pub fn new(
        signer: Result<String, String>,
        acting_as: Option<String>,
        vault_address: Option<String>,
    ) -> Self {
        Self {
            signer,
            acting_as,
            vault_address,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DryRunEnvelope {
    command: String,
    would_execute: String,
    args: serde_json::Value,
    payload: Option<serde_json::Value>,
    signing: Option<DryRunSigning>,
    network: Option<Network>,
}

impl DryRunEnvelope {
    /// `args` is the command's own preview; `Null` leaves the key out.
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        would_execute: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self {
            command: command.into(),
            would_execute: would_execute.into(),
            args,
            payload: None,
            signing: None,
            network: None,
        }
    }

    /// The `--payload-json` document, already redacted.
    #[must_use]
    pub fn with_payload(mut self, payload: Option<serde_json::Value>) -> Self {
        self.payload = payload;
        self
    }

    #[must_use]
    pub fn with_signing(mut self, signing: DryRunSigning) -> Self {
        self.signing = Some(signing);
        self
    }

    #[must_use]
    pub fn with_network(mut self, network: Option<Network>) -> Self {
        self.network = network;
        self
    }

    fn json_object(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut object = serde_json::Map::new();
        object.insert("dry_run".to_string(), serde_json::json!(true));
        object.insert("command".to_string(), serde_json::json!(self.command));
        object.insert(
            "would_execute".to_string(),
            serde_json::json!(self.would_execute),
        );
        if !self.args.is_null() {
            object.insert("args".to_string(), self.args.clone());
        }
        if let Some(payload) = &self.payload {
            object.insert("payload".to_string(), payload.clone());
        }
        if let Some(signing) = &self.signing {
            match &signing.signer {
                Ok(address) => {
                    object.insert("signer".to_string(), serde_json::json!(address));
                }
                Err(reason) => {
                    object.insert("signer".to_string(), serde_json::Value::Null);
                    object.insert("signer_warning".to_string(), serde_json::json!(reason));
                }
            }
            object.insert(
                "acting_as".to_string(),
                serde_json::json!(signing.acting_as),
            );
            object.insert(
                "vault_address".to_string(),
                serde_json::json!(signing.vault_address),
            );
        }
        if let Some(network) = self.network {
            object.insert(
                "network".to_string(),
                serde_json::json!(network.to_string()),
            );
        }
        object
    }
}

fn field_label(key: &str) -> String {
    let spaced = key.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn display_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "none".to_string(),
        other => other.to_string(),
    }
}

impl TableData for DryRunEnvelope {
    fn headers(&self) -> Vec<&str> {
        vec!["Field", "Value"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let mut rows = vec![
            vec!["Command".to_string(), self.command.clone()],
            vec!["Would execute".to_string(), self.would_execute.clone()],
        ];
        match &self.args {
            serde_json::Value::Object(args) => {
                for (key, value) in args {
                    rows.push(vec![field_label(key), display_value(value)]);
                }
            }
            serde_json::Value::Null => {}
            other => rows.push(vec!["Args".to_string(), display_value(other)]),
        }
        if let Some(signing) = &self.signing {
            rows.push(vec![
                "Signer".to_string(),
                match &signing.signer {
                    Ok(address) => address.clone(),
                    Err(_) => "not resolved".to_string(),
                },
            ]);
            if let Some(acting_as) = &signing.acting_as {
                rows.push(vec!["Acting as".to_string(), acting_as.clone()]);
            }
            if let Some(vault) = &signing.vault_address {
                rows.push(vec!["Vault".to_string(), vault.clone()]);
            }
            if let Err(reason) = &signing.signer {
                rows.push(vec!["Warning".to_string(), reason.clone()]);
            }
        }
        if let Some(network) = self.network {
            rows.push(vec!["Network".to_string(), network.to_string()]);
        }
        rows
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::Value::Object(self.json_object())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_envelope() -> DryRunEnvelope {
        DryRunEnvelope::new(
            "orders create",
            "place_order",
            serde_json::json!({"coin": "BTC", "side": "buy", "size": "0.1", "price": null}),
        )
    }

    #[test]
    fn unsigned_envelope_has_no_signer_keys() {
        assert_eq!(
            sample_envelope()
                .with_network(Some(Network::Mainnet))
                .to_json_value(),
            serde_json::json!({
                "dry_run": true,
                "command": "orders create",
                "would_execute": "place_order",
                "args": {"coin": "BTC", "side": "buy", "size": "0.1", "price": null},
                "network": "mainnet"
            })
        );
    }

    #[test]
    fn null_args_are_left_out() {
        let envelope = DryRunEnvelope::new(
            "wallet create",
            "create_and_store_encrypted_wallet",
            serde_json::Value::Null,
        );
        assert_eq!(
            envelope.to_json_value(),
            serde_json::json!({
                "dry_run": true,
                "command": "wallet create",
                "would_execute": "create_and_store_encrypted_wallet"
            })
        );
    }

    #[test]
    fn resolved_signer_names_the_acting_account() {
        let json = sample_envelope()
            .with_signing(DryRunSigning::new(
                Ok("0xabc".to_string()),
                Some("0xvault".to_string()),
                Some("0xvault".to_string()),
            ))
            .with_network(Some(Network::Testnet))
            .to_json_value();

        assert_eq!(json["network"], "testnet");
        assert_eq!(json["signer"], "0xabc");
        assert_eq!(json["acting_as"], "0xvault");
        assert_eq!(json["vault_address"], "0xvault");
        assert!(json.get("signer_warning").is_none());
    }

    #[test]
    fn unresolved_signer_is_null_with_warning() {
        let json = sample_envelope()
            .with_signing(DryRunSigning::new(
                Err("no signer configured".to_string()),
                None,
                None,
            ))
            .to_json_value();

        assert!(json["signer"].is_null());
        assert_eq!(json["signer_warning"], "no signer configured");
        assert!(json["acting_as"].is_null());
        assert!(json["vault_address"].is_null());
    }

    #[test]
    fn human_rows_list_args_signer_and_network() {
        let rows = sample_envelope()
            .with_signing(DryRunSigning::new(Ok("0xabc".to_string()), None, None))
            .with_network(Some(Network::Testnet))
            .rows();
        let find = |label: &str| {
            rows.iter()
                .find(|row| row[0] == label)
                .map(|row| row[1].clone())
        };

        assert_eq!(find("Command").as_deref(), Some("orders create"));
        assert_eq!(find("Would execute").as_deref(), Some("place_order"));
        assert_eq!(find("Coin").as_deref(), Some("BTC"));
        assert_eq!(find("Side").as_deref(), Some("buy"));
        assert_eq!(find("Size").as_deref(), Some("0.1"));
        assert_eq!(find("Price").as_deref(), Some("none"));
        assert_eq!(find("Signer").as_deref(), Some("0xabc"));
        assert_eq!(find("Network").as_deref(), Some("testnet"));
    }

    #[test]
    fn human_rows_say_when_the_signer_is_unresolved() {
        let rows = sample_envelope()
            .with_signing(DryRunSigning::new(
                Err("Authentication required.".to_string()),
                None,
                None,
            ))
            .rows();

        assert!(rows.contains(&vec!["Signer".to_string(), "not resolved".to_string()]));
        assert!(rows.contains(&vec![
            "Warning".to_string(),
            "Authentication required.".to_string()
        ]));
    }
}
