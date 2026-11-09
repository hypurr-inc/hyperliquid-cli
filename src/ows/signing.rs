//! Signing with a vault wallet.
//!
//! [`OwsSigningConfig`] is a resolved wallet (or, for an address-only
//! `--ows-signer 0x...`, an identity placeholder that can't sign). Signing goes
//! through `ows-lib`, then checks that the recovered address is the one the
//! caller selected before returning a signature.

use alloy::dyn_abi::TypedData;
use alloy::primitives::B256;
use hypersdk::Address;
use hypersdk::hypercore::types::Signature as HyperliquidSignature;
use ows_lib::types::WalletInfo;

use super::vault::Vault;
use super::{HYPERLIQUID_CHAIN_ALIAS, WalletSource};
use crate::errors::CliError;
use crate::signable::Payload;
use crate::signature::Provider;

#[derive(Debug, Clone)]
struct SigningWallet {
    id: String,
    name: String,
    vault: Vault,
}

#[derive(Debug, Clone)]
enum Identity {
    Wallet(SigningWallet),
    /// An address named by `--ows-signer 0x...` that matches no wallet: usable
    /// for identity previews, never for live signing. Holds the text it was
    /// selected with.
    AddressOnly(String),
}

#[derive(Debug, Clone)]
pub struct OwsSigningConfig {
    address: Address,
    identity: Identity,
}

impl OwsSigningConfig {
    #[must_use]
    pub fn for_wallet(wallet: &WalletInfo, address: Address, vault: Vault) -> Self {
        Self {
            address,
            identity: Identity::Wallet(SigningWallet {
                id: wallet.id.clone(),
                name: wallet.name.clone(),
                vault,
            }),
        }
    }

    #[must_use]
    pub fn address_only(selector: String, address: Address) -> Self {
        Self {
            address,
            identity: Identity::AddressOnly(selector),
        }
    }

    #[must_use]
    pub fn address(&self) -> Address {
        self.address
    }

    /// The vault wallet's name and how this signer is described to the user.
    #[must_use]
    pub fn describe(&self) -> (Option<String>, String) {
        match &self.identity {
            Identity::Wallet(wallet) => (
                Some(wallet.name.clone()),
                WalletSource::Vault.describe(&wallet.name),
            ),
            Identity::AddressOnly(selector) => (None, format!("OWS signer ({selector})")),
        }
    }

    /// Whether this can sign at all: an address-only selector cannot.
    pub fn ensure_signable(&self) -> Result<(), CliError> {
        self.wallet().map(|_| ())
    }

    fn wallet(&self) -> Result<&SigningWallet, CliError> {
        match &self.identity {
            Identity::Wallet(wallet) => Ok(wallet),
            Identity::AddressOnly(selector) => Err(unsupported_live_signing(selector)),
        }
    }
}

fn unsupported_live_signing(selector: &str) -> CliError {
    CliError::Unsupported(format!(
        "OWS signer '{selector}' does not have a resolved wallet for live signing; use a stored wallet name/id, or select a local signing account, keystore, or private key for live submission"
    ))
}

/// Sign `payload` with the vault wallet: a user-signed document goes through
/// the vault's typed-data signer, every other payload as its EIP-712 `digest`.
/// The signature is returned unverified; [`crate::signing::SelectedSigner`]
/// proves it recovers to the selected address.
pub(crate) fn sign(
    config: &OwsSigningConfig,
    payload: &Payload<'_>,
    digest: B256,
) -> Result<HyperliquidSignature, CliError> {
    match payload {
        Payload::UserSigned(typed_data) => sign_typed_data(config, typed_data),
        _ => sign_hash(config, digest),
    }
}

fn sign_typed_data(
    config: &OwsSigningConfig,
    typed_data: &TypedData,
) -> Result<HyperliquidSignature, CliError> {
    let wallet = config.wallet()?;
    let typed_data_json = ows_typed_data_json(typed_data)?;
    let result = ows_lib::sign_typed_data(
        &wallet.id,
        HYPERLIQUID_CHAIN_ALIAS,
        &typed_data_json,
        wallet.vault.passphrase(),
        None,
        wallet.vault.path(),
    )
    .map_err(super::map_ows_error)?;
    Provider::OWS.parse_signature(&result.signature, result.recovery_id)
}

fn sign_hash(config: &OwsSigningConfig, hash: B256) -> Result<HyperliquidSignature, CliError> {
    let wallet = config.wallet()?;
    let result = ows_lib::sign_hash(
        &wallet.id,
        HYPERLIQUID_CHAIN_ALIAS,
        &format!("0x{}", hex::encode(hash)),
        wallet.vault.passphrase(),
        None,
        wallet.vault.path(),
    )
    .map_err(super::map_ows_error)?;
    Provider::OWS.parse_signature(&result.signature, result.recovery_id)
}

fn ows_typed_data_json(typed_data: &TypedData) -> Result<String, CliError> {
    let mut value = serde_json::to_value(typed_data).map_err(|err| {
        CliError::Internal(anyhow::anyhow!("failed to encode OWS typed data: {err}"))
    })?;
    ensure_eip712_domain_type(&mut value);
    pad_odd_hex_typed_values(&mut value);
    serde_json::to_string(&value).map_err(|err| {
        CliError::Internal(anyhow::anyhow!("failed to encode OWS typed data: {err}"))
    })
}

fn ensure_eip712_domain_type(value: &mut serde_json::Value) {
    let Some(domain) = value
        .get("domain")
        .and_then(serde_json::Value::as_object)
        .cloned()
    else {
        return;
    };
    let Some(types) = value
        .get_mut("types")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if types.contains_key("EIP712Domain") {
        return;
    }

    let mut fields = Vec::new();
    for (name, type_name) in [
        ("name", "string"),
        ("version", "string"),
        ("chainId", "uint256"),
        ("verifyingContract", "address"),
        ("salt", "bytes32"),
    ] {
        if domain.contains_key(name) {
            fields.push(serde_json::json!({"name": name, "type": type_name}));
        }
    }
    types.insert("EIP712Domain".to_string(), serde_json::Value::Array(fields));
}

fn pad_odd_hex_typed_values(value: &mut serde_json::Value) {
    let Some(types) = value
        .get("types")
        .and_then(serde_json::Value::as_object)
        .cloned()
    else {
        return;
    };
    if let Some(domain) = value.get_mut("domain") {
        pad_struct_odd_hex_values("EIP712Domain", domain, &types);
    }
    let Some(primary_type) = value
        .get("primaryType")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    if let Some(message) = value.get_mut("message") {
        pad_struct_odd_hex_values(&primary_type, message, &types);
    }
}

fn pad_struct_odd_hex_values(
    type_name: &str,
    value: &mut serde_json::Value,
    types: &serde_json::Map<String, serde_json::Value>,
) {
    let Some(fields) = types
        .get(type_name)
        .and_then(serde_json::Value::as_array)
        .cloned()
    else {
        return;
    };
    let Some(object) = value.as_object_mut() else {
        return;
    };

    for field in fields {
        let Some(name) = field.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(field_type) = field.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(field_value) = object.get_mut(name) else {
            continue;
        };
        pad_field_odd_hex_values(field_type, field_value, types);
    }
}

fn pad_field_odd_hex_values(
    field_type: &str,
    value: &mut serde_json::Value,
    types: &serde_json::Map<String, serde_json::Value>,
) {
    if let Some(element_type) = field_type.strip_suffix("[]") {
        if let Some(values) = value.as_array_mut() {
            for value in values {
                pad_field_odd_hex_values(element_type, value, types);
            }
        }
        return;
    }

    if types.contains_key(field_type) {
        pad_struct_odd_hex_values(field_type, value, types);
        return;
    }

    if is_hex_encoded_eip712_scalar(field_type)
        && let Some(text) = value.as_str()
        && let Some(hex) = text.strip_prefix("0x")
        && hex.len() % 2 == 1
    {
        *value = serde_json::Value::String(format!("0x0{hex}"));
    }
}

fn is_hex_encoded_eip712_scalar(field_type: &str) -> bool {
    field_type == "bytes"
        || field_type.starts_with("bytes")
        || field_type.starts_with("uint")
        || field_type.starts_with("int")
}

#[cfg(test)]
mod tests {
    use super::super::vault::test_support::{KEY, import_test_wallet};
    use super::*;

    fn signing_config(wallet: &WalletInfo, vault_path: &std::path::Path) -> OwsSigningConfig {
        OwsSigningConfig::for_wallet(wallet, test_signer_address(), Vault::at(vault_path))
    }

    fn test_signer_address() -> Address {
        KEY.parse::<hypersdk::hypercore::PrivateKeySigner>()
            .unwrap()
            .address()
    }

    #[test]
    fn address_only_config_exposes_identity_but_rejects_signing() {
        let address: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let config = OwsSigningConfig::address_only(address.to_string(), address);

        assert_eq!(config.address(), address);
        assert_eq!(config.ensure_signable().unwrap_err().exit_code(), 13);
        assert_eq!(config.describe(), (None, format!("OWS signer ({address})")));
        assert_eq!(sign_hash(&config, B256::ZERO).unwrap_err().exit_code(), 13);
    }

    #[test]
    fn ows_hash_signing_recovers_selected_wallet_address() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wallet = import_test_wallet(tmp.path(), "test-wallet");
        let config = signing_config(&wallet, tmp.path());
        let hash = B256::repeat_byte(0x42);

        let signature = sign_hash(&config, hash).unwrap();
        let alloy_signature: alloy::primitives::Signature = signature.into();
        let recovered = alloy_signature.recover_address_from_prehash(&hash).unwrap();

        assert_eq!(recovered, config.address());
    }

    #[test]
    fn ows_typed_data_signing_recovers_selected_wallet_address() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wallet = import_test_wallet(tmp.path(), "test-wallet");
        let config = signing_config(&wallet, tmp.path());
        let typed_data: TypedData = serde_json::from_value(serde_json::json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "chainId", "type": "uint256"}
                ],
                "TestAction": [
                    {"name": "message", "type": "string"}
                ]
            },
            "primaryType": "TestAction",
            "domain": {"name": "Hyperliquid", "chainId": "0x3e7"},
            "message": {"message": "hello ows"}
        }))
        .unwrap();
        let signing_hash = typed_data.eip712_signing_hash().unwrap();

        let signature = sign_typed_data(&config, &typed_data).unwrap();
        let alloy_signature: alloy::primitives::Signature = signature.into();
        let recovered = alloy_signature
            .recover_address_from_prehash(&signing_hash)
            .unwrap();

        assert_eq!(recovered, config.address());
    }

    #[test]
    fn ows_signs_hyperliquid_user_signed_documents_without_a_domain_type() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wallet = import_test_wallet(tmp.path(), "test-wallet");
        let config = signing_config(&wallet, tmp.path());
        let typed_data: TypedData = serde_json::from_value(serde_json::json!({
            "types": {
                "HyperliquidTransaction:UsdClassTransfer": [
                    {"name": "hyperliquidChain", "type": "string"},
                    {"name": "amount", "type": "string"},
                    {"name": "toPerp", "type": "bool"},
                    {"name": "memo", "type": "string"},
                    {"name": "nonce", "type": "uint64"}
                ]
            },
            "primaryType": "HyperliquidTransaction:UsdClassTransfer",
            "domain": {
                "name": "HyperliquidSignTransaction",
                "version": "1",
                "chainId": "0x66eee",
                "verifyingContract": "0x0000000000000000000000000000000000000000"
            },
            "message": {
                "hyperliquidChain": "Testnet",
                "amount": "5",
                "toPerp": true,
                "memo": "0xabc",
                "nonce": 1_700_000_000_123_u64
            }
        }))
        .unwrap();

        let payload = Payload::UserSigned(&typed_data);
        let signature = sign(&config, &payload, payload.digest().unwrap()).unwrap();

        let recovered = alloy::primitives::Signature::from(signature)
            .recover_address_from_prehash(&typed_data.eip712_signing_hash().unwrap())
            .unwrap();
        assert_eq!(recovered, config.address());
    }
}
