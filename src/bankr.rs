//! Bankr hosted-wallet login and signing.
//!
//! Bankr holds the key. The CLI authenticates with a `bk_...` API key, reads the
//! linked wallet from `GET /wallet/me`, and asks Bankr to sign Hyperliquid L1
//! actions as EIP-712 typed data (`POST /wallet/sign`).
//!
//! Most signing is synchronous in this CLI (command handlers sign through
//! [`crate::signing::SelectedSigner`]), so those Bankr requests run on a fresh
//! current-thread runtime on a scoped thread: the handler's own runtime is never
//! blocked on a future it would have to poll. Async callers await the request.
//!
//! Bankr signs trading actions and spot-to-perp USDC moves; every other
//! user-signed action is refused by [`BankrTypedData::for_payload`] before a
//! request is sent.
//!
//! Every returned signature is recovered locally against the expected wallet
//! address before it is used, so a Bankr reply can never sign for a different
//! account than the one selected.

use std::time::Duration;

use alloy::dyn_abi::TypedData;
use alloy::primitives::B256;
use hypersdk::Address;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::types::Signature as HyperliquidSignature;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::BankrLogin;
use crate::errors::{CliError, network_error};
use crate::response_sanitization::Untrusted;
use crate::settings::{EnvSource, Settings, env, is_loopback_url};
use crate::signing::Payload;
use crate::signing::signature::Provider;

/// Bankr's public API, used when no other URL is configured.
pub const DEFAULT_API_URL: &str = "https://api.bankr.bot";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the API key came from, which decides whether a stored address belongs
/// to it: a key from the environment or the Bankr CLI is not the wallet that
/// `wallet login` recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Environment,
    Config,
    BankrCli,
}

/// One resolved set of Bankr credentials.
#[derive(Clone)]
pub struct BankrClient {
    http: reqwest::Client,
    api_url: String,
    api_key: String,
}

impl std::fmt::Debug for BankrClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BankrClient")
            .field("api_url", &self.api_url)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

/// The wallet Bankr reports for an API key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BankrWallet {
    pub address: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_address: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub social_accounts: Vec<BankrSocial>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referral_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club_active: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BankrSocial {
    pub platform: String,
    pub username: String,
}

#[derive(Deserialize)]
struct WalletMeResponse {
    #[serde(default)]
    wallets: Vec<WalletEntry>,
    #[serde(default)]
    social_accounts: Vec<BankrSocial>,
    #[serde(default)]
    ref_code: Option<String>,
    #[serde(default)]
    bankr_club: Option<BankrClub>,
}

#[derive(Deserialize)]
struct WalletEntry {
    chain: String,
    address: String,
}

#[derive(Deserialize)]
struct BankrClub {
    #[serde(default)]
    active: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignRequest<'a> {
    signature_type: &'static str,
    typed_data: &'a Value,
}

/// Typed data in the two shapes one signature needs: the JSON document that goes
/// to Bankr, and the parsed form used to hash and verify it. The parsed form is
/// always produced from the document, so a signature can't be checked against a
/// different message than the one that was sent.
#[derive(Debug, Clone)]
pub struct BankrTypedData {
    document: Value,
    typed: TypedData,
}

impl BankrTypedData {
    /// Parse the JSON document that will be posted, so hashing and verification
    /// always run over the message Bankr was asked to sign.
    pub fn parse(document: Value) -> Result<Self, CliError> {
        let typed = serde_json::from_value(document.clone())
            .map_err(|err| CliError::Internal(anyhow::anyhow!("invalid typed data: {err}")))?;
        Ok(Self { document, typed })
    }

    /// A document built from typed data hypersdk produced, with the domain chain
    /// id written as the decimal number `eth_signTypedData_v4` clients expect.
    pub fn from_typed(typed_data: &TypedData) -> Result<Self, CliError> {
        let mut document = serde_json::to_value(typed_data).map_err(|err| {
            CliError::Internal(anyhow::anyhow!("failed to encode typed data: {err}"))
        })?;
        if let Some(chain_id) = document
            .get_mut("domain")
            .and_then(|domain| domain.get_mut("chainId"))
            && let Some(text) = chain_id.as_str()
            && let Some(decimal) = text
                .strip_prefix("0x")
                .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        {
            *chain_id = Value::from(decimal);
        }
        Self::parse(document)
    }

    /// The JSON document sent as `typedData`.
    #[must_use]
    pub fn document(&self) -> &Value {
        &self.document
    }

    /// The EIP-712 digest a signature must recover against.
    pub fn signing_hash(&self) -> Result<B256, CliError> {
        self.typed
            .eip712_signing_hash()
            .map_err(|err| CliError::Internal(anyhow::anyhow!("failed to hash typed data: {err}")))
    }
}

/// The user-signed actions Bankr signs, by EIP-712 primary type: a spot↔perp
/// USDC move. Every other user-signed action (withdrawals, sends, staking,
/// approvals, multisig) is refused before a request is sent.
const SIGNED_USER_ACTIONS: &[&str] = &["HyperliquidTransaction:UsdClassTransfer"];

impl BankrTypedData {
    /// The document Bankr is asked to sign for `payload`, or the refusal for a
    /// payload it does not cover.
    pub(crate) fn for_payload(payload: &Payload<'_>) -> Result<Self, CliError> {
        match payload {
            Payload::Agent {
                chain,
                connection_id,
            } => Ok(agent_typed_data(*chain, *connection_id)),
            Payload::UserSigned(typed_data)
                if SIGNED_USER_ACTIONS.contains(&typed_data.primary_type.as_str()) =>
            {
                Self::from_typed(typed_data)
            }
            Payload::UserSigned(_) | Payload::Digest(_) => Err(CliError::Unsupported(
                "Bankr signs trading actions (orders, cancels, modifies, leverage, margin, trailing stops, TWAP, vault and subaccount actions) and spot-to-perp USDC moves; this action needs a user-signed payload Bankr does not cover, so use a local key or an OWS wallet for it"
                    .to_string(),
            )),
        }
    }
}

/// A Bankr hosted wallet: the key stays with Bankr and every signature is an
/// API call.
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

    /// Sign from synchronous code, on a scoped runtime thread.
    pub(crate) fn sign(&self, payload: &Payload<'_>) -> Result<HyperliquidSignature, CliError> {
        self.client
            .blocking_sign(&BankrTypedData::for_payload(payload)?, self.address)
    }

    pub(crate) async fn sign_async(
        &self,
        payload: &Payload<'_>,
    ) -> Result<HyperliquidSignature, CliError> {
        self.client
            .sign(&BankrTypedData::for_payload(payload)?, self.address)
            .await
    }
}

#[derive(Deserialize)]
struct SignResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    signer: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl BankrClient {
    /// A client for `api_url` (already validated) and `api_key`.
    #[must_use]
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("hyperliquid-cli/", env!("CARGO_PKG_VERSION")))
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("failed to build Bankr HTTP client"),
            api_url: api_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// Credentials for this run: `BANKR_API_KEY` first, then this CLI's config,
    /// then the Bankr CLI's own `~/.bankr/config.json`, so an existing login works
    /// without repeating it. `BANKR_API_URL` overrides the host in every case.
    pub fn from_settings(settings: &Settings) -> Result<(Self, KeySource), CliError> {
        let env_url = settings.env().get_trimmed(env::BANKR_API_URL);
        let login = settings.bankr_login()?;
        let stored_url = login.api_url().map(str::to_string);

        let (api_key, file_url, source) =
            if let Some(key) = settings.env().get_trimmed(env::BANKR_API_KEY) {
                (key, stored_url, KeySource::Environment)
            } else if let Some(key) = login.api_key() {
                (key.to_string(), stored_url, KeySource::Config)
            } else if let Some((key, url)) = bankr_cli_credentials(settings.env()) {
                (key, url.or(stored_url), KeySource::BankrCli)
            } else {
                return Err(CliError::AuthRequired);
            };

        let api_url = validate_api_url(
            &env_url
                .or(file_url)
                .unwrap_or_else(|| DEFAULT_API_URL.to_string()),
        )?;
        Ok((Self::new(api_url, api_key), source))
    }

    #[must_use]
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    /// The wallet linked to this API key.
    pub async fn wallet(&self) -> Result<BankrWallet, CliError> {
        let response = self
            .http
            .get(format!("{}/wallet/me", self.api_url))
            .header("X-API-Key", &self.api_key)
            .send()
            .await
            .map_err(|err| network_error(&err, Some(REQUEST_TIMEOUT)))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(credential_error(status, &body));
        }
        let me: WalletMeResponse = serde_json::from_str(&body).map_err(|err| {
            CliError::Unavailable(format!("bankr returned an unreadable wallet reply: {err}"))
        })?;
        let address = me
            .wallets
            .iter()
            .find(|wallet| wallet.chain.eq_ignore_ascii_case("evm"))
            .and_then(|wallet| wallet.address.parse::<Address>().ok())
            .ok_or_else(|| {
                CliError::Unavailable("bankr has no EVM wallet linked to this API key".to_string())
            })?;

        Ok(BankrWallet {
            address,
            solana_address: me
                .wallets
                .iter()
                .find(|wallet| wallet.chain.eq_ignore_ascii_case("solana"))
                .map(|wallet| wallet.address.clone()),
            social_accounts: me.social_accounts,
            referral_code: me.ref_code,
            club_active: me.bankr_club.map(|club| club.active),
        })
    }

    /// Sign `typed_data` with `eth_signTypedData_v4` and return the Hyperliquid
    /// signature, after checking it recovers to `expected`.
    pub async fn sign(
        &self,
        typed_data: &BankrTypedData,
        expected: Address,
    ) -> Result<HyperliquidSignature, CliError> {
        let body = SignRequest {
            signature_type: "eth_signTypedData_v4",
            typed_data: typed_data.document(),
        };
        let response = self
            .http
            .post(format!("{}/wallet/sign", self.api_url))
            .header("X-API-Key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|err| network_error(&err, Some(REQUEST_TIMEOUT)))?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(credential_error(status, &text));
        }
        let signed: SignResponse = serde_json::from_str(&text).map_err(|err| {
            CliError::Unavailable(format!(
                "bankr returned an unreadable signature reply: {err}"
            ))
        })?;
        if !signed.success {
            let reason = signed
                .error
                .or(signed.message)
                .unwrap_or_else(|| "no reason given".to_string());
            return Err(CliError::InvalidAuth(format!(
                "bankr refused to sign: {}",
                Untrusted::from_remote(&reason)
            )));
        }
        if let Some(signer) = signed.signer.as_deref()
            && let Ok(signer) = signer.parse::<Address>()
            && signer != expected
        {
            return Err(CliError::InvalidAuth(format!(
                "bankr_signer_mismatch: bankr signed with {signer}, expected {expected}"
            )));
        }
        let signature = signed
            .signature
            .as_deref()
            .ok_or_else(|| CliError::InvalidAuth("bankr returned no signature".to_string()))?;
        let signature = Provider::BANKR.parse_signature(signature, None)?;
        Provider::BANKR.verify(&signature, typed_data.signing_hash()?, expected)?;
        Ok(signature)
    }

    /// Run a Bankr request from synchronous signing code.
    pub fn blocking_wallet(&self) -> Result<BankrWallet, CliError> {
        block_on(self.wallet())
    }

    pub fn blocking_sign(
        &self,
        typed_data: &BankrTypedData,
        expected: Address,
    ) -> Result<HyperliquidSignature, CliError> {
        block_on(self.sign(typed_data, expected))
    }
}

/// The typed data Hyperliquid signs for an L1 action that carries no user-signed
/// schema: the msgpack hash wrapped in the `Agent` struct.
#[must_use]
pub fn agent_typed_data(chain: Chain, connection_id: B256) -> BankrTypedData {
    let value = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"},
            ],
            "Agent": [
                {"name": "source", "type": "string"},
                {"name": "connectionId", "type": "bytes32"},
            ],
        },
        "primaryType": "Agent",
        "domain": {
            "name": "Exchange",
            "version": "1",
            "chainId": 1337,
            "verifyingContract": "0x0000000000000000000000000000000000000000",
        },
        "message": {
            "source": if chain.is_mainnet() { "a" } else { "b" },
            "connectionId": format!("{connection_id:#x}"),
        },
    });
    BankrTypedData::parse(value).expect("the Agent typed data is a fixed, valid document")
}

/// The endpoint `wallet login` recorded, normalised, so a cached address is only
/// reused for the API that produced it.
pub fn login_endpoint(login: &BankrLogin) -> Result<String, CliError> {
    validate_api_url(login.api_url().unwrap_or(DEFAULT_API_URL))
}

/// Accept a Bankr API base URL: HTTPS anywhere, HTTP only on loopback (a local
/// stub), so the API key is never sent over plaintext to a remote host.
pub fn validate_api_url(raw: &str) -> Result<String, CliError> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|err| CliError::Configuration(format!("invalid Bankr API URL '{raw}': {err}")))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_url(&url) => {}
        "http" => {
            return Err(CliError::Configuration(format!(
                "invalid Bankr API URL '{raw}': plain HTTP is only allowed for a loopback host, so the API key is never sent in the clear"
            )));
        }
        scheme => {
            return Err(CliError::Configuration(format!(
                "invalid Bankr API URL '{raw}': HTTP or HTTPS is required, not '{scheme}'"
            )));
        }
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Read the key the Bankr CLI stores, when it exists.
fn bankr_cli_credentials(env: &EnvSource) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(crate::paths::bankr_cli_config(env)?).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let key = value.get("apiKey")?.as_str()?.trim();
    if key.is_empty() {
        return None;
    }
    let url = value
        .get("apiUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string);
    Some((key.to_string(), url))
}

/// A `401`/`403` is a credential problem; anything else is the API failing.
fn credential_error(status: reqwest::StatusCode, body: &str) -> CliError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "no reason given".to_string());
    match status.as_u16() {
        401 | 403 => CliError::InvalidAuth(format!(
            "bankr rejected the API key ({}): {}. Set {}, or run `hyperliquid wallet login` to store a bk_... key from https://bankr.bot/api-keys.",
            status.as_u16(),
            Untrusted::from_remote(&message),
            env::BANKR_API_KEY
        )),
        429 => CliError::rate_limited(),
        _ => CliError::api_status(status, body),
    }
}

/// Run `future` to completion on a fresh current-thread runtime, in its own
/// thread, so a synchronous caller inside an existing runtime doesn't deadlock.
fn block_on<F>(future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread runtime always builds");
                runtime.block_on(future)
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response_sanitization::UNTRUSTED_LABEL;
    use hypersdk::hypercore::types::{Action, ScheduleCancel};

    #[test]
    fn agent_typed_data_matches_the_hash_hypersdk_signs() {
        let action = Action::ScheduleCancel(ScheduleCancel { time: None });
        let nonce = 1_700_000_000_000_u64;
        for chain in [Chain::Mainnet, Chain::Testnet] {
            let connection_id = action.hash(nonce, None, None).unwrap();
            let typed_data = agent_typed_data(chain, connection_id);
            assert_eq!(
                typed_data.signing_hash().unwrap(),
                action.prehash(nonce, None, None, chain).unwrap(),
                "the Agent wrapper must hash exactly like hypersdk's own signing path"
            );
        }
    }

    #[test]
    fn agent_typed_data_uses_the_chain_source_byte() {
        let connection_id = B256::repeat_byte(0x11);
        let mainnet = agent_typed_data(Chain::Mainnet, connection_id);
        let testnet = agent_typed_data(Chain::Testnet, connection_id);
        let mainnet = mainnet.document();
        let testnet = testnet.document();
        assert_eq!(mainnet["message"]["source"], "a");
        assert_eq!(testnet["message"]["source"], "b");
        assert_eq!(
            mainnet["domain"]["chainId"], 1337,
            "chain id is sent as a number"
        );
    }

    #[test]
    fn documents_are_sent_in_the_form_they_are_verified_in() {
        // A hypersdk-shaped document (hex chain id) must hash identically after
        // the decimal normalisation the Bankr request uses.
        let value = serde_json::json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"},
                    {"name": "verifyingContract", "type": "address"},
                ],
                "Agent": [
                    {"name": "source", "type": "string"},
                    {"name": "connectionId", "type": "bytes32"},
                ],
            },
            "primaryType": "Agent",
            "domain": {
                "name": "Exchange",
                "version": "1",
                "chainId": "0x539",
                "verifyingContract": "0x0000000000000000000000000000000000000000",
            },
            "message": {"source": "a", "connectionId": format!("{:#x}", B256::repeat_byte(7))},
        });
        let original: TypedData = serde_json::from_value(value).unwrap();
        let normalized = BankrTypedData::from_typed(&original).unwrap();
        assert_eq!(normalized.document()["domain"]["chainId"], 1337);
        assert_eq!(
            normalized.signing_hash().unwrap(),
            original.eip712_signing_hash().unwrap()
        );
    }

    #[test]
    fn blank_environment_keys_do_not_shadow_stored_credentials() {
        use crate::config::Config;
        const ADDRESS: &str = "0x0000000000000000000000000000000000000009";
        use crate::settings::{EnvSource, Settings};

        let config = Config {
            bankr: BankrLogin::new("bk_from_config", None, ADDRESS.parse().unwrap()),
            ..Config::default()
        };
        let settings = Settings::detached(
            EnvSource::fixed([(env::BANKR_API_KEY, "   "), (env::BANKR_API_URL, "  ")]),
            Some(config),
        );
        let (client, source) = BankrClient::from_settings(&settings).unwrap();
        assert_eq!(source, KeySource::Config);
        assert_eq!(client.api_url(), DEFAULT_API_URL);

        // A padded key is selected trimmed, so authentication sees the key itself.
        let padded = Settings::detached(
            EnvSource::fixed([(env::BANKR_API_KEY, "  bk_padded  ")]),
            None,
        );
        let (_, source) = BankrClient::from_settings(&padded).unwrap();
        assert_eq!(source, KeySource::Environment);
    }

    #[test]
    fn api_urls_allow_https_and_loopback_http_only() {
        assert!(validate_api_url("https://api.bankr.bot").is_ok());
        assert_eq!(
            validate_api_url("https://api.bankr.bot/").unwrap(),
            "https://api.bankr.bot"
        );
        assert!(validate_api_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_api_url("http://localhost:8080").is_ok());
        assert!(validate_api_url("http://[::1]:8080").is_ok());
        assert!(validate_api_url("http://[2001:db8::1]:8080").is_err());

        for rejected in ["http://api.bankr.bot", "ftp://127.0.0.1", "not a url", ""] {
            let err = validate_api_url(rejected).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{rejected}");
        }
    }

    #[test]
    fn remote_refusal_text_is_labelled_and_capped() {
        let hostile = format!("\u{1b}[31mdenied\u{1b}[0m {}", "x".repeat(400));
        let text = Untrusted::from_remote(&hostile).to_string();
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(text.starts_with(UNTRUSTED_LABEL), "{text}");
        assert!(text.chars().count() < hostile.chars().count());
    }

    #[test]
    fn rejected_keys_name_the_login_command() {
        let err = credential_error(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":"Invalid API key"}"#,
        );
        assert_eq!(err.exit_code(), 10);
        assert!(err.to_string().contains("wallet login"));
        assert_eq!(
            credential_error(reqwest::StatusCode::TOO_MANY_REQUESTS, "").exit_code(),
            11
        );
        assert_eq!(
            credential_error(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"message":"boom"}"#
            )
            .exit_code(),
            12
        );
    }
}
