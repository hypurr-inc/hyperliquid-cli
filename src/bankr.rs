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
//! user-signed action is refused by [`covers`] before a request is sent. The
//! signature a request returns is recovered against the payload's digest by
//! the selected signer, so a Bankr reply can never sign for a different account
//! than the one selected.

use std::time::Duration;

use alloy::primitives::B256;
use hypersdk::Address;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::types::Signature as HyperliquidSignature;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bankr_credentials::KeySource;
use crate::config::BankrLogin;
use crate::env;
use crate::errors::{CliError, network_error};
use crate::hints::wallet::WALLET_LOGIN;
use crate::network::is_loopback_url;
use crate::resolvers::parse_hex_address;
use crate::response_sanitization::Untrusted;
use crate::settings::Settings;
use crate::signable::{Payload, Signs};
use crate::signature::Provider;

/// Bankr's public API, used when no other URL is configured.
pub const DEFAULT_API_URL: &str = "https://api.bankr.bot";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

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

#[derive(Deserialize)]
struct WalletMeResponse {
    #[serde(default)]
    wallets: Vec<WalletEntry>,
}

#[derive(Deserialize)]
struct WalletEntry {
    chain: String,
    address: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignRequest<'a> {
    signature_type: &'static str,
    typed_data: &'a Value,
}

/// The refusal for a signature Bankr does not cover, before a request is sent.
pub fn ensure_covers(signs: Signs<'_>) -> Result<(), CliError> {
    if signs.bankr_covers() {
        Ok(())
    } else {
        Err(not_covered())
    }
}

fn not_covered() -> CliError {
    CliError::Unsupported(
        "Bankr signs trading actions (orders, cancels, modifies, leverage, margin, trailing stops, TWAP, vault and subaccount actions) and spot-to-perp USDC moves; this action needs a user-signed payload Bankr does not cover, so use a local key or an OWS wallet for it"
            .to_string(),
    )
}

/// The typed-data document Bankr is asked to sign for `payload`, or the refusal
/// for a payload it does not cover.
pub(crate) fn document(payload: &Payload<'_>) -> Result<Value, CliError> {
    ensure_covers(payload.signs())?;
    match payload {
        Payload::Agent {
            chain,
            connection_id,
        } => Ok(agent_document(*chain, *connection_id)),
        Payload::UserSigned(typed_data) => {
            let mut document = serde_json::to_value(typed_data).map_err(|err| {
                CliError::Internal(anyhow::anyhow!("failed to encode typed data: {err}"))
            })?;
            decimal_chain_id(&mut document);
            Ok(document)
        }
        Payload::Digest(_) => Err(not_covered()),
    }
}

/// Write the domain chain id as the decimal number `eth_signTypedData_v4`
/// clients expect; hypersdk serializes it as hex text.
fn decimal_chain_id(document: &mut Value) {
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
        block_on(self.sign_async(payload))
    }

    pub(crate) async fn sign_async(
        &self,
        payload: &Payload<'_>,
    ) -> Result<HyperliquidSignature, CliError> {
        self.client.sign(&document(payload)?, self.address).await
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
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self, CliError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hyperliquid-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| {
                CliError::Internal(anyhow::anyhow!(
                    "failed to build the Bankr HTTP client: {err}"
                ))
            })?;
        Ok(Self {
            http,
            api_url: api_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        })
    }

    /// The client for this run's credentials (see [`Settings::bankr_credentials`]),
    /// and where they came from.
    pub fn from_settings(settings: &Settings) -> Result<(Self, KeySource), CliError> {
        let credentials = settings
            .bankr_credentials()?
            .ok_or(CliError::AuthRequired)?;
        let api_url = validate_api_url(credentials.api_url.as_deref().unwrap_or(DEFAULT_API_URL))?;
        Ok((Self::new(api_url, credentials.api_key)?, credentials.source))
    }

    #[must_use]
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    /// The EVM address of the wallet linked to this API key.
    pub async fn wallet(&self) -> Result<Address, CliError> {
        let response = self
            .http
            .get(format!("{}/wallet/me", self.api_url))
            .header("X-API-Key", &self.api_key)
            .send()
            .await
            .map_err(|err| network_error(&err, Some(REQUEST_TIMEOUT)))?;

        let status = response.status();
        let body = read_body(response).await?;
        if !status.is_success() {
            return Err(credential_error(status, &body));
        }
        let me: WalletMeResponse = serde_json::from_str(&body).map_err(|err| {
            CliError::Unavailable(format!("bankr returned an unreadable wallet reply: {err}"))
        })?;
        me.wallets
            .iter()
            .find(|wallet| wallet.chain.eq_ignore_ascii_case("evm"))
            .and_then(|wallet| parse_hex_address(&wallet.address, false).ok())
            .ok_or_else(|| {
                CliError::Unavailable("bankr has no EVM wallet linked to this API key".to_string())
            })
    }

    /// Sign `document` with `eth_signTypedData_v4` and return the Hyperliquid
    /// signature. A reply that names a signer other than `expected` is refused;
    /// the caller recovers the signature against the digest it asked for.
    async fn sign(
        &self,
        document: &Value,
        expected: Address,
    ) -> Result<HyperliquidSignature, CliError> {
        let body = SignRequest {
            signature_type: "eth_signTypedData_v4",
            typed_data: document,
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
        let text = read_body(response).await?;
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
            && let Ok(signer) = parse_hex_address(signer, false)
            && signer != expected
        {
            return Err(Provider::BANKR
                .signer_mismatch(format!("bankr signed with {signer}, expected {expected}")));
        }
        let signature = signed
            .signature
            .as_deref()
            .ok_or_else(|| CliError::InvalidAuth("bankr returned no signature".to_string()))?;
        Provider::BANKR.parse_signature(signature, None)
    }

    pub fn blocking_wallet(&self) -> Result<Address, CliError> {
        block_on(self.wallet())
    }
}

/// The reply body. A body that fails to arrive is a transport failure, not an
/// empty reply.
async fn read_body(response: reqwest::Response) -> Result<String, CliError> {
    response
        .text()
        .await
        .map_err(|err| network_error(&err, Some(REQUEST_TIMEOUT)))
}

/// The typed data Hyperliquid signs for an L1 action that carries no user-signed
/// schema: the msgpack hash wrapped in the `Agent` struct. hypersdk builds this
/// document only inside its private `solidity` module and exposes just the
/// digest ([`hypersdk::hypercore::signing::agent_signing_hash`]), so the JSON
/// is written out here; a test hashes it against that digest, and every Bankr
/// signature is verified against the same digest at signing time.
fn agent_document(chain: Chain, connection_id: B256) -> Value {
    json!({
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
    })
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
            "bankr rejected the API key ({}): {}. Set {}, or run `{WALLET_LOGIN}` to store a bk_... key from https://bankr.bot/api-keys.",
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
    use alloy::dyn_abi::TypedData;
    use hypersdk::hypercore::types::{Action, ScheduleCancel};

    fn typed(document: &Value) -> TypedData {
        serde_json::from_value(document.clone()).unwrap()
    }

    #[test]
    fn agent_document_matches_the_hash_hypersdk_signs() {
        let action = Action::ScheduleCancel(ScheduleCancel { time: None });
        let nonce = 1_700_000_000_000_u64;
        for chain in [Chain::Mainnet, Chain::Testnet] {
            let connection_id = action.hash(nonce, None, None).unwrap();
            assert_eq!(
                typed(&agent_document(chain, connection_id))
                    .eip712_signing_hash()
                    .unwrap(),
                action.prehash(nonce, None, None, chain).unwrap(),
                "the Agent wrapper must hash exactly like hypersdk's own signing path"
            );
        }
    }

    #[test]
    fn agent_document_uses_the_chain_source_byte() {
        let connection_id = B256::repeat_byte(0x11);
        let mainnet = agent_document(Chain::Mainnet, connection_id);
        let testnet = agent_document(Chain::Testnet, connection_id);
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
        let original = typed(&serde_json::json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"},
                    {"name": "verifyingContract", "type": "address"},
                ],
                "HyperliquidTransaction:UsdClassTransfer": [
                    {"name": "amount", "type": "string"},
                ],
            },
            "primaryType": "HyperliquidTransaction:UsdClassTransfer",
            "domain": {
                "name": "Exchange",
                "version": "1",
                "chainId": "0x539",
                "verifyingContract": "0x0000000000000000000000000000000000000000",
            },
            "message": {"amount": "7"},
        }));
        let sent = document(&Payload::UserSigned(&original)).unwrap();
        assert_eq!(sent["domain"]["chainId"], 1337);
        assert_eq!(
            typed(&sent).eip712_signing_hash().unwrap(),
            original.eip712_signing_hash().unwrap()
        );
    }

    #[test]
    fn payloads_bankr_does_not_cover_are_refused_before_a_document_exists() {
        let err = document(&Payload::Digest(B256::ZERO)).unwrap_err();
        assert_eq!(err.exit_code(), 13);
    }

    #[test]
    fn blank_environment_keys_do_not_shadow_stored_credentials() {
        use crate::config::Config;
        const ADDRESS: &str = "0x0000000000000000000000000000000000000009";
        use crate::env::EnvSource;
        use crate::settings::Settings;

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
