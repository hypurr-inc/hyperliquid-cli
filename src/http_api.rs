//! The one HTTP edge to the Hyperliquid API.
//!
//! An [`ApiClient`] knows the chain, the resolved base URL (the loopback
//! override or the public endpoint), one `reqwest` client for raw requests and
//! the `hypersdk` client for the typed SDK calls. Nothing else builds a request
//! URL for the API.
//!
//! `/info` reads retry transient failures (connect error, timeout, 5xx, 429)
//! with jittered backoff. `/exchange` posts are sent exactly once: after the
//! request leaves, a timeout, reset, 5xx, or unreadable reply is an unknown
//! outcome ([`CliError::UnknownOutcome`]), never a reason to resend.

use std::sync::OnceLock;
use std::time::Duration;

use hypersdk::hypercore::{Chain, HttpClient};
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::errors::{
    CliError, address_rate_limit_in_body, http_response_indicates_rate_limit, is_server_status,
    network_error, transport_cause,
};
use crate::network::Network;
use crate::response_sanitization::Untrusted;

const DEFAULT_API_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Backoff before each automatic read retry (the read is retried at most this many times).
const READ_RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(750)];
/// Backoff before each retry that follows a 429 without `Retry-After`. The
/// limit is a weight spent over a one-minute window, so a retry a few hundred
/// milliseconds later is refused again and only spends more of it.
const RATE_LIMIT_RETRY_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
/// Longest `Retry-After` the CLI will wait out. A longer ask is reported, not waited.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(5);
/// Upper bound on the total time spent sleeping between read attempts. It
/// admits one capped `Retry-After` wait; a second one would pass the bound, so
/// the read stops retrying and reports the failure.
const RETRY_TOTAL_BUDGET: Duration = Duration::from_secs(6);

const _: () = {
    assert!(RETRY_TOTAL_BUDGET.as_millis() >= RETRY_AFTER_CAP.as_millis());
    assert!(RETRY_TOTAL_BUDGET.as_millis() < 2 * RETRY_AFTER_CAP.as_millis());
    // Both retries at the longest jitter (+25%) still fit the budget, so a
    // 429 without `Retry-After` is retried twice, not once.
    assert!(
        (RATE_LIMIT_RETRY_BACKOFF[0].as_millis() + RATE_LIMIT_RETRY_BACKOFF[1].as_millis()) * 5 / 4
            <= RETRY_TOTAL_BUDGET.as_millis()
    );
};

/// Whether a failed API call was a read (safe to resend) or a mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Read,
    Mutation,
}

struct Reply {
    status: StatusCode,
    body: String,
    /// `Retry-After` in whole seconds, when the server sent one.
    retry_after: Option<u64>,
}

/// A failed attempt, with whether another attempt may succeed.
struct AttemptError {
    error: CliError,
    transient: bool,
}

/// An answered `/info` read whose transport failures and rate limits are
/// already classified. The status is left for the caller to read, because
/// some endpoints say "unsupported request" with a 4xx.
#[derive(Debug, Clone)]
pub struct InfoReply {
    status: StatusCode,
    body: String,
}

impl InfoReply {
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Decode a 2xx body. Any other status is [`CliError::ApiStatus`].
    pub fn decode<T: DeserializeOwned>(&self, context: &'static str) -> Result<T, CliError> {
        if !self.status.is_success() {
            return Err(CliError::api_status(self.status, &self.body));
        }
        decode_json(&self.body, context)
    }
}

pub struct ApiClient {
    chain: Chain,
    base_url: reqwest::Url,
    /// The base URL as shown to users: the override as parsed, else the public endpoint.
    label: String,
    has_override: bool,
    sdk: HttpClient,
    http: OnceLock<Result<reqwest::Client, String>>,
    timeout: Duration,
    /// Wait before each automatic read retry; the read is retried once per entry.
    retry_backoff: &'static [Duration],
    /// Wait before each read retry that follows a 429 without `Retry-After`.
    rate_limit_backoff: &'static [Duration],
    on_retry: Option<fn()>,
    /// `--expires-after`: how long after signing a signed action stays valid.
    request_lifetime: Option<Duration>,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("chain", &self.chain)
            .field("base_url", &self.label)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    #[must_use]
    pub fn new(network: Network, api_override: Option<reqwest::Url>) -> Self {
        let has_override = api_override.is_some();
        let (base_url, label) = match &api_override {
            Some(url) => (url.clone(), url.to_string()),
            None => (network.api_url(), network.api_base_url()),
        };
        Self {
            chain: network.chain(),
            base_url,
            label,
            has_override,
            sdk: network.http_client(api_override),
            http: OnceLock::new(),
            timeout: DEFAULT_API_TIMEOUT,
            retry_backoff: &READ_RETRY_BACKOFF,
            rate_limit_backoff: &RATE_LIMIT_RETRY_BACKOFF,
            on_retry: None,
            request_lifetime: None,
        }
    }

    /// Call `observer` once when a read is first retried, so a caller can tell
    /// a person the wait is deliberate.
    #[must_use]
    pub fn on_retry(mut self, observer: fn()) -> Self {
        self.on_retry = Some(observer);
        self
    }

    /// Give every signed action sent through this client an `expiresAfter` of
    /// `lifetime` after the moment it is signed.
    #[must_use]
    pub fn with_request_lifetime(mut self, lifetime: Option<Duration>) -> Self {
        self.request_lifetime = lifetime;
        self
    }

    /// The `--expires-after` lifetime, when one was asked for.
    #[must_use]
    pub fn request_lifetime(&self) -> Option<Duration> {
        self.request_lifetime
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_retry_backoff(mut self, backoff: &'static [Duration]) -> Self {
        self.retry_backoff = backoff;
        self.rate_limit_backoff = backoff;
        self
    }

    #[cfg(test)]
    fn with_rate_limit_backoff(mut self, backoff: &'static [Duration]) -> Self {
        self.rate_limit_backoff = backoff;
        self
    }

    /// The backoff table for the retry after `status`. A 429 that names its
    /// own wait (`Retry-After`) is waited out by that wait; one that doesn't
    /// gets the longer rate-limit backoff.
    fn backoff_for(
        &self,
        status: Option<StatusCode>,
        retry_after: Option<u64>,
    ) -> &'static [Duration] {
        if status == Some(StatusCode::TOO_MANY_REQUESTS) && retry_after.is_none() {
            self.rate_limit_backoff
        } else {
            self.retry_backoff
        }
    }

    #[must_use]
    pub fn chain(&self) -> Chain {
        self.chain
    }

    #[must_use]
    pub fn sdk(&self) -> &HttpClient {
        &self.sdk
    }

    /// The URL this client talks to: the loopback override, else the public endpoint.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.label
    }

    /// POST `request` to `/info` and decode the JSON answer.
    pub async fn info<T: DeserializeOwned>(
        &self,
        request: &impl Serialize,
        context: &'static str,
    ) -> Result<T, CliError> {
        self.info_raw(request).await?.decode(context)
    }

    /// POST `request` to `/info` and return the answer without judging its status.
    pub async fn info_raw(&self, request: &impl Serialize) -> Result<InfoReply, CliError> {
        let reply = self.read(request).await?;
        if let Some(err) = rate_limit_error(&reply, Disposition::Read) {
            return Err(err);
        }
        Ok(InfoReply {
            status: reply.status,
            body: reply.body,
        })
    }

    /// POST a signed `request` to `/exchange` and decode the JSON answer.
    ///
    /// Sent exactly once. See the module docs for how failures after the send are
    /// classified.
    pub async fn exchange<T: DeserializeOwned>(
        &self,
        request: &impl Serialize,
    ) -> Result<T, CliError> {
        let url = self.endpoint("/exchange");
        let reply = self
            .send_once(&url, request, self.timeout, Disposition::Mutation)
            .await
            .map_err(|attempt| attempt.error)?;
        ensure_success(&reply, Disposition::Mutation)?;
        serde_json::from_str(&reply.body).map_err(|_| {
            CliError::unknown_outcome("The exchange replied with a response the CLI couldn't read")
        })
    }

    /// Probe a loopback API override with an `allMids` read before any command
    /// uses it. Sent once, with a short timeout. Without an override there is
    /// nothing to probe.
    pub async fn probe_override(&self) -> Result<(), CliError> {
        if !self.has_override {
            return Ok(());
        }
        let url = self.endpoint("/info");
        let reply = self
            .send_once(
                &url,
                &serde_json::json!({ "type": "allMids" }),
                PROBE_TIMEOUT,
                Disposition::Read,
            )
            .await
            .map_err(|attempt| attempt.error)?;
        ensure_success(&reply, Disposition::Read)
    }

    fn endpoint(&self, path: &str) -> reqwest::Url {
        let mut url = self.base_url.clone();
        url.set_path(path);
        url.set_query(None);
        url
    }

    fn http(&self) -> Result<&reqwest::Client, CliError> {
        self.http
            .get_or_init(|| {
                reqwest::Client::builder()
                    .tcp_nodelay(true)
                    .build()
                    .map_err(|err| err.to_string())
            })
            .as_ref()
            .map_err(|message| CliError::Internal(anyhow::anyhow!("{message}")))
    }

    /// Send one request. `disposition` decides how a failure after the send is mapped.
    async fn send_once(
        &self,
        url: &reqwest::Url,
        request: &impl Serialize,
        timeout: Duration,
        disposition: Disposition,
    ) -> Result<Reply, AttemptError> {
        let classify = |err: reqwest::Error| classify_transport_error(&err, timeout, disposition);
        let client = self.http().map_err(|error| AttemptError {
            error,
            transient: false,
        })?;

        let response = client
            .post(url.clone())
            .timeout(timeout)
            .json(request)
            .send()
            .await
            .map_err(classify)?;
        let status = response.status();
        let retry_after = retry_after_seconds(response.headers());
        let body = response.text().await.map_err(|err| {
            // The server answered; a body that dies mid-read after a mutation leaves
            // the result unknown, whatever the failure looked like.
            let mut attempt = classify_transport_error(&err, timeout, disposition);
            if disposition == Disposition::Mutation && !err.is_builder() {
                attempt.error = CliError::unknown_outcome(format!(
                    "The connection failed while reading the reply. {}",
                    transport_cause(&err, Some(timeout))
                ));
            }
            attempt
        })?;

        Ok(Reply {
            status,
            body,
            retry_after,
        })
    }

    /// Send a read to `/info`, retrying transient failures with jittered backoff.
    async fn read(&self, request: &impl Serialize) -> Result<Reply, CliError> {
        let url = self.endpoint("/info");

        let mut waited = Duration::ZERO;
        let mut retries = 0;
        let mut announced = false;
        loop {
            let (outcome, retry_after, status) = match self
                .send_once(&url, request, self.timeout, Disposition::Read)
                .await
            {
                Ok(reply) if is_transient_status(reply.status) => {
                    let (retry_after, status) = (reply.retry_after, Some(reply.status));
                    (Ok(reply), retry_after, status)
                }
                Ok(reply) => return Ok(reply),
                Err(attempt) if attempt.transient => (Err(attempt.error), None, None),
                Err(attempt) => return Err(attempt.error),
            };

            let backoff = self.backoff_for(status, retry_after);
            let Some(delay) = read_retry_delay(backoff, retries, retry_after) else {
                return outcome;
            };
            if waited + delay > RETRY_TOTAL_BUDGET {
                return outcome;
            }
            if !announced {
                announced = true;
                if let Some(observer) = self.on_retry {
                    observer();
                }
            }
            tokio::time::sleep(delay).await;
            waited += delay;
            retries += 1;
        }
    }
}

fn classify_transport_error(
    err: &reqwest::Error,
    timeout: Duration,
    disposition: Disposition,
) -> AttemptError {
    if err.is_builder() {
        return AttemptError {
            error: CliError::Internal(anyhow::anyhow!("invalid API request: {err}")),
            transient: false,
        };
    }
    match disposition {
        Disposition::Read => AttemptError {
            error: network_error(err, Some(timeout)),
            transient: true,
        },
        Disposition::Mutation => {
            // A connect failure (refused, DNS, TLS, connect timeout) means the
            // request never reached the exchange. Anything else may have.
            let error = if err.is_connect() {
                network_error(err, Some(timeout))
            } else {
                CliError::unknown_outcome(transport_cause(err, Some(timeout)))
            };
            AttemptError {
                error,
                transient: false,
            }
        }
    }
}

fn is_transient_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || is_server_status(status.as_u16())
}

/// `Retry-After` in whole seconds, when the server sent a numeric one.
fn retry_after_seconds(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// Delay before retry number `retries + 1`, or `None` when out of retries or
/// the server asked for a wait longer than [`RETRY_AFTER_CAP`].
fn read_retry_delay(
    backoff: &[Duration],
    retries: usize,
    retry_after: Option<u64>,
) -> Option<Duration> {
    let base = *backoff.get(retries)?;
    let jitter = 0.75 + 0.5 * rand::random::<f64>();
    let backoff = base.mul_f64(jitter);
    match retry_after.map(Duration::from_secs) {
        Some(asked) if asked > RETRY_AFTER_CAP => None,
        Some(asked) => Some(backoff.max(asked)),
        None => Some(backoff),
    }
}

fn rate_limit_error(reply: &Reply, disposition: Disposition) -> Option<CliError> {
    http_response_indicates_rate_limit(reply.status.as_u16(), &reply.body).then(|| {
        address_rate_limit_in_body(&reply.body).unwrap_or(CliError::RateLimited {
            retry_after: reply.retry_after,
            retryable: disposition == Disposition::Read,
        })
    })
}

fn ensure_success(reply: &Reply, disposition: Disposition) -> Result<(), CliError> {
    if let Some(err) = rate_limit_error(reply, disposition) {
        return Err(err);
    }

    let status = reply.status;
    if status.is_success() {
        return Ok(());
    }

    if disposition == Disposition::Mutation && is_server_status(status.as_u16()) {
        return Err(CliError::unknown_outcome(format!(
            "The exchange returned HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        )));
    }
    Err(CliError::api_status(status, &reply.body))
}

/// Decode a JSON response body with the CLI's standard internal-error envelope.
fn decode_json<T: DeserializeOwned>(body: &str, context: &'static str) -> Result<T, CliError> {
    serde_json::from_str::<T>(body).map_err(|err| {
        let body = Untrusted::from_remote(body);
        CliError::internal_with_remote(
            format!("decode failed while {context}: {err}; body="),
            &body,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Retries wait milliseconds here, so a test never sleeps the real backoff.
    static FAST_BACKOFF: [Duration; 2] = [Duration::from_millis(1), Duration::from_millis(2)];

    fn client_for(uri: &str) -> ApiClient {
        ApiClient::new(Network::Mainnet, Some(reqwest::Url::parse(uri).unwrap()))
            .with_retry_backoff(&FAST_BACKOFF)
    }

    fn reply(status: StatusCode, body: &str) -> Reply {
        Reply {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    #[test]
    fn ensure_success_maps_rate_limits_before_api_status() {
        let err = ensure_success(
            &reply(StatusCode::TOO_MANY_REQUESTS, "too many requests"),
            Disposition::Read,
        )
        .expect_err("429 should be rate limited");
        assert_eq!(err.exit_code(), 11);
    }

    #[test]
    fn ensure_success_maps_server_errors_without_blaming_the_network() {
        let err = ensure_success(
            &reply(StatusCode::BAD_GATEWAY, "upstream down"),
            Disposition::Read,
        )
        .expect_err("502 should fail");
        assert_eq!(err.exit_code(), 12);
        assert!(matches!(err, CliError::ApiStatus { status: 502, .. }));
        assert_eq!(
            err.to_string(),
            "The Hyperliquid API failed to process the request."
        );
        assert_eq!(err.detail().as_deref(), Some("HTTP 502 Bad Gateway"));
        assert!(!err.to_string().contains("network"));
    }

    #[test]
    fn decode_json_reports_body() {
        let err = decode_json::<serde_json::Value>("not json", "testing")
            .expect_err("malformed json should fail");
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("decode failed while testing"));
        assert!(
            err.to_string()
                .contains("body=[untrusted remote data] not json")
        );
    }

    #[test]
    fn retry_delay_is_jittered_and_bounded() {
        for _ in 0..50 {
            for (retry, base) in READ_RETRY_BACKOFF.iter().enumerate() {
                let delay = read_retry_delay(&READ_RETRY_BACKOFF, retry, None).unwrap();
                assert!(
                    delay >= base.mul_f64(0.75) && delay <= base.mul_f64(1.25),
                    "retry {retry}: {delay:?} around {base:?}"
                );
            }
        }
        assert!(
            read_retry_delay(&READ_RETRY_BACKOFF, READ_RETRY_BACKOFF.len(), None).is_none(),
            "no retries beyond the backoff table"
        );
    }

    #[test]
    fn retry_delay_honors_retry_after_up_to_the_cap() {
        let delay = |asked| read_retry_delay(&READ_RETRY_BACKOFF, 0, Some(asked));
        assert_eq!(delay(2), Some(Duration::from_secs(2)));
        assert_eq!(delay(5), Some(Duration::from_secs(5)));
        assert!(delay(6).is_none(), "a longer ask is reported, not waited");
    }

    #[tokio::test]
    async fn info_read_retries_503_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;

        let value: serde_json::Value = client_for(&server.uri())
            .info(&json!({"type": "meta"}), "testing")
            .await
            .expect("second attempt succeeds");

        assert_eq!(value, json!({"ok": true}));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn info_read_gives_up_after_two_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .info::<serde_json::Value>(&json!({}), "testing")
            .await
            .expect_err("persistent 503 fails");

        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert_eq!(err.exit_code(), 12);
        assert!(matches!(err, CliError::ApiStatus { status: 503, .. }));
    }

    #[tokio::test]
    async fn the_retry_observer_hears_of_a_retrying_read_once() {
        static NOTICES: AtomicUsize = AtomicUsize::new(0);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let _ = client_for(&server.uri())
            .on_retry(|| {
                NOTICES.fetch_add(1, Ordering::SeqCst);
            })
            .info::<serde_json::Value>(&json!({}), "testing")
            .await;

        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert_eq!(NOTICES.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn info_read_429_with_long_retry_after_is_reported_not_waited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7"))
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .info::<serde_json::Value>(&json!({}), "testing")
            .await
            .expect_err("429 fails");

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(err.exit_code(), 11);
        assert_eq!(err.retry_after(), Some(7));
        assert_eq!(err.retryable(), Some(true));
        assert_eq!(
            err.to_string(),
            "Too many requests to the Hyperliquid API. Try again in 7s."
        );
    }

    /// Stands in for the rate-limit backoff, long enough to measure and short
    /// enough to keep the test fast.
    static SLOW_RATE_LIMIT_BACKOFF: [Duration; 2] =
        [Duration::from_millis(120), Duration::from_millis(120)];

    #[test]
    fn a_429_without_retry_after_gets_the_longer_backoff() {
        let client = ApiClient::new(Network::Mainnet, None);
        let tables = |status, retry_after| client.backoff_for(status, retry_after);

        assert_eq!(
            tables(Some(StatusCode::TOO_MANY_REQUESTS), None),
            &RATE_LIMIT_RETRY_BACKOFF
        );
        // A 429 that names its wait is waited out by that wait.
        assert_eq!(
            tables(Some(StatusCode::TOO_MANY_REQUESTS), Some(2)),
            &READ_RETRY_BACKOFF
        );
        assert_eq!(
            tables(Some(StatusCode::SERVICE_UNAVAILABLE), None),
            &READ_RETRY_BACKOFF
        );
        assert_eq!(tables(None, None), &READ_RETRY_BACKOFF);
        assert!(
            RATE_LIMIT_RETRY_BACKOFF[0] > READ_RETRY_BACKOFF[1],
            "the first rate-limit wait outlasts every plain read retry"
        );
    }

    #[tokio::test]
    async fn info_read_429_without_retry_after_waits_the_rate_limit_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let started = std::time::Instant::now();
        let err = client_for(&server.uri())
            .with_rate_limit_backoff(&SLOW_RATE_LIMIT_BACKOFF)
            .info::<serde_json::Value>(&json!({}), "testing")
            .await
            .expect_err("persistent 429 fails");

        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        // Two waits of at least 75% of 120 ms; the plain 1 ms and 2 ms backoff would end far sooner.
        assert!(
            started.elapsed() >= Duration::from_millis(180),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(err.exit_code(), 11);
    }

    #[tokio::test]
    async fn exchange_200_cumulative_request_limit_is_the_address_rate_limit_and_sent_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "err",
                "response": "Too many cumulative requests sent (37986 > 10436) for cumulative volume traded $437.92. Place taker orders to free up 1 request per USDC traded."
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .exchange::<serde_json::Value>(&json!({}))
            .await
            .expect_err("rate limited");

        assert!(matches!(err, CliError::AddressRateLimited(_)), "{err:?}");
        assert_eq!(err.exit_code(), 11);
        assert_eq!(err.retry_after(), Some(10));
        assert_eq!(err.retryable(), None, "mutations never claim retryable");
        assert!(err.hint().is_some());
    }

    #[tokio::test]
    async fn raw_info_read_returns_a_rejected_status_for_the_caller_to_judge() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(422).set_body_string("unknown request type"))
            .expect(1)
            .mount(&server)
            .await;

        let reply = client_for(&server.uri())
            .info_raw(&json!({"type": "nope"}))
            .await
            .expect("a 422 is an answer");

        assert_eq!(reply.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.body(), "unknown request type");
        let err = reply.decode::<serde_json::Value>("testing").unwrap_err();
        assert!(matches!(err, CliError::ApiStatus { status: 422, .. }));
    }

    #[tokio::test]
    async fn raw_info_read_maps_a_rate_limit_before_the_caller_sees_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "9"))
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .info_raw(&json!({}))
            .await
            .expect_err("429");

        assert_eq!(err.exit_code(), 11);
        assert_eq!(err.retry_after(), Some(9));
    }

    #[tokio::test]
    async fn exchange_503_is_unknown_outcome_and_sent_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .exchange::<serde_json::Value>(&json!({}))
            .await
            .expect_err("503 is unknown");

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(matches!(err, CliError::UnknownOutcome(_)));
        assert_eq!(err.exit_code(), 12);
        assert_eq!(err.reason(), "unknown_outcome");
        assert_eq!(err.retryable(), Some(false));
        assert!(
            err.to_string()
                .starts_with("Can't confirm whether the exchange applied this action.")
        );
    }

    #[tokio::test]
    async fn exchange_timeout_is_unknown_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .expect(1)
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .with_timeout(Duration::from_millis(200))
            .exchange::<serde_json::Value>(&json!({}))
            .await
            .expect_err("timeout is unknown");

        assert!(matches!(err, CliError::UnknownOutcome(_)), "{err:?}");
        assert!(err.to_string().contains("Timed out after 200ms."));
    }

    #[tokio::test]
    async fn exchange_undecodable_200_is_unknown_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .exchange::<serde_json::Value>(&json!({}))
            .await
            .expect_err("undecodable reply is unknown");

        assert!(matches!(err, CliError::UnknownOutcome(_)));
        assert!(!err.to_string().contains("oops"));
    }

    #[tokio::test]
    async fn exchange_429_is_rate_limited_not_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "3"))
            .expect(1)
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .exchange::<serde_json::Value>(&json!({}))
            .await
            .expect_err("429");

        assert_eq!(err.exit_code(), 11);
        assert_eq!(err.retry_after(), Some(3));
        assert_eq!(err.retryable(), None, "mutations never claim retryable");
    }

    #[tokio::test]
    async fn connect_failure_names_the_host_and_stays_unavailable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = client_for(&format!("http://127.0.0.1:{port}"));

        let exchange = client
            .exchange::<serde_json::Value>(&json!({}))
            .await
            .expect_err("refused");
        assert!(matches!(exchange, CliError::Unavailable(_)), "{exchange:?}");
        let message = exchange.to_string();
        assert!(
            message.starts_with(&format!("Couldn't reach 127.0.0.1:{port}. ")),
            "{message}"
        );
        assert!(message.contains("Connection refused"), "{message}");

        let read = client
            .info::<serde_json::Value>(&json!({}), "testing")
            .await
            .expect_err("refused");
        assert!(matches!(read, CliError::Unavailable(_)));
        assert_eq!(read.exit_code(), 12);
    }

    #[tokio::test]
    async fn override_probe_reads_once_and_reports_a_failing_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .expect(1)
            .mount(&server)
            .await;

        let err = client_for(&server.uri())
            .probe_override()
            .await
            .expect_err("503 fails the probe");

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(matches!(err, CliError::ApiStatus { status: 503, .. }));
    }
}
