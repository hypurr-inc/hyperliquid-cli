//! The `hypersdk` reads the CLI makes, retried like an `/info` read.
//!
//! The SDK client sends each request once and has no retry hook, so a rate
//! limit would surface at once, and differently from a read through
//! [`ApiClient::info`](super::ApiClient::info). Every read the CLI makes through
//! the SDK therefore goes through [`Sdk`], which retries a 429, a 5xx and a
//! transport failure under the same [`ReadPolicy`] (backoff, budget, retry
//! notice) as `/info` reads. When the retries run out the SDK's own error is
//! classified here (a 429 is `rate_limited`, exit 11), so every read returns a
//! [`CliError`].
//!
//! Two limits of the SDK show here. Its errors keep the status and body but not
//! the headers, so a `Retry-After` cannot be honoured and a 429 waits the
//! rate-limit backoff. And its market-metadata calls (`perps`, `spot`,
//! `perp_dexes`, ...) decode the body without looking at the status, so a
//! rate-limit reply surfaces as a decode error: they are deliberately not
//! offered here, and metadata is read with `ApiClient::info` (see `assets`).

use hypersdk::hypercore::{
    ApiAgent, ApiError, BasicOrder, Candle, CandleInterval, ClearinghouseState, Fill, FundingRate,
    GossipPriorityAuctionStatus, HttpClient, OidOrCloid, OrderUpdate, UserVaultEquity,
};
use hypersdk::{Address, Decimal};
use reqwest::StatusCode;
use std::collections::HashMap;

use super::{ReadPolicy, Transient, is_transient_status};
use crate::errors::{CliError, from_sdk, http_status_in_text};

/// The SDK client for reads, handed out by [`ApiClient::sdk`](super::ApiClient::sdk).
#[derive(Clone, Copy)]
pub struct Sdk<'a> {
    client: &'a HttpClient,
    policy: ReadPolicy,
}

/// Whether a failed SDK read is worth another attempt, and under which status.
fn transient_failure(err: &anyhow::Error) -> Option<Transient> {
    if let Some(api) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiError>())
    {
        let status = StatusCode::from_u16(http_status_in_text(&api.0)?).ok()?;
        return is_transient_status(status).then_some(Transient {
            status: Some(status),
            retry_after: None,
        });
    }
    // A reply that does not decode will not decode the next time either.
    let transport = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())?;
    (!transport.is_builder() && !transport.is_decode()).then_some(Transient {
        status: None,
        retry_after: None,
    })
}

/// A reply whose JSON parsed but does not have the shape the SDK types.
fn is_shape_mismatch(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<serde_json::Error>()
            .is_some_and(|decode| decode.classify() == serde_json::error::Category::Data)
    })
}

/// The SDK reads that check the reply status, with the SDK's signatures.
macro_rules! sdk_reads {
    ($($(#[$meta:meta])* $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        impl Sdk<'_> {
            $(
                $(#[$meta])*
                pub async fn $name(&self, $($arg: $ty),*) -> Result<$ret, CliError> {
                    self.read(async || self.client.$name($(Clone::clone(&$arg)),*).await).await
                }
            )*
        }
    };
}

sdk_reads! {
    all_mids(dex_name: Option<String>) -> HashMap<String, Decimal>;
    api_agents(user: Address) -> Vec<ApiAgent>;
    clearinghouse_state(user: Address, dex_name: Option<String>) -> ClearinghouseState;
    gossip_priority_auction_status() -> GossipPriorityAuctionStatus;
    open_orders(user: Address, dex_name: Option<String>) -> Vec<BasicOrder>;
    order_status(user: Address, oid: OidOrCloid) -> Option<OrderUpdate<BasicOrder>>;
    user_fills(user: Address) -> Vec<Fill>;
    user_vault_equities(user: Address) -> Vec<UserVaultEquity>;
}

impl<'a> Sdk<'a> {
    pub(super) fn new(client: &'a HttpClient, policy: ReadPolicy) -> Self {
        Self { client, policy }
    }

    pub async fn candle_snapshot(
        &self,
        coin: impl Into<String>,
        interval: CandleInterval,
        start_time: u64,
        end_time: u64,
    ) -> Result<Vec<Candle>, CliError> {
        let coin = coin.into();
        self.read(async || {
            self.client
                .candle_snapshot(coin.clone(), interval, start_time, end_time)
                .await
        })
        .await
    }

    pub async fn funding_history(
        &self,
        coin: impl Into<String>,
        start_time: u64,
        end_time: Option<u64>,
    ) -> Result<Vec<FundingRate>, CliError> {
        let coin = coin.into();
        self.read(async || {
            self.client
                .funding_history(coin.clone(), start_time, end_time)
                .await
        })
        .await
    }

    /// `historicalOrders` as the SDK types it (`OrderUpdate<BasicOrder>`), or
    /// `None` when the reply is a bare order list the SDK cannot decode (a serde
    /// data error), which the caller reads raw instead.
    pub async fn historical_orders(
        &self,
        user: Address,
    ) -> Result<Option<Vec<OrderUpdate<BasicOrder>>>, CliError> {
        match self
            .read_raw(async || self.client.historical_orders(user).await)
            .await
        {
            Ok(updates) => Ok(Some(updates)),
            Err(err) if is_shape_mismatch(&err) => Ok(None),
            Err(err) => Err(read_error(err)),
        }
    }

    async fn read<T>(&self, call: impl AsyncFnMut() -> anyhow::Result<T>) -> Result<T, CliError> {
        self.read_raw(call).await.map_err(read_error)
    }

    async fn read_raw<T>(
        &self,
        mut call: impl AsyncFnMut() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.policy
            .retrying(async || {
                let result = call().await;
                let transient = result.as_ref().err().and_then(transient_failure);
                (result, transient)
            })
            .await
    }
}

/// An SDK read is always safe to resend, so its rate limit says so.
fn read_error(err: anyhow::Error) -> CliError {
    match from_sdk(err) {
        CliError::RateLimited { retry_after, .. } => CliError::RateLimited {
            retry_after,
            retryable: true,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::http_api::ApiClient;
    use crate::network::Network;

    static FAST_BACKOFF: [Duration; 2] = [Duration::from_millis(1), Duration::from_millis(2)];

    const USER: Address = Address::repeat_byte(1);
    const ADDRESS_LIMIT: &str = "Too many cumulative requests sent (37986 > 10436) for cumulative volume traded $437.92. Place taker orders to free up 1 request per USDC traded.";

    fn client_for(uri: &str) -> ApiClient {
        ApiClient::new(Network::Mainnet, Some(reqwest::Url::parse(uri).unwrap()))
            .with_retry_backoff(&FAST_BACKOFF)
    }

    async fn requests(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    /// The 429 shapes the exchange answers with: a JSON `null`, an nginx page,
    /// and the address-limit text.
    fn rate_limit_replies() -> [ResponseTemplate; 3] {
        [
            ResponseTemplate::new(429)
                .set_body_raw("null", "application/json"),
            ResponseTemplate::new(429).set_body_raw(
                "<html><head><title>429 Too Many Requests</title></head><body>429 Too Many Requests</body></html>",
                "text/html",
            ),
            ResponseTemplate::new(429).set_body_string(ADDRESS_LIMIT),
        ]
    }

    #[tokio::test]
    async fn a_rate_limited_read_is_retried_and_then_succeeds() {
        for limited in rate_limit_replies() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/info"))
                .respond_with(limited)
                .up_to_n_times(2)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/info"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&server)
                .await;

            let orders = client_for(&server.uri())
                .sdk()
                .open_orders(USER, None)
                .await
                .expect("the third attempt succeeds");

            assert!(orders.is_empty());
            assert_eq!(requests(&server).await, 3);
        }
    }

    #[tokio::test]
    async fn a_read_that_stays_rate_limited_ends_as_rate_limited_never_a_decode_error() {
        for limited in rate_limit_replies() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/info"))
                .respond_with(limited)
                .mount(&server)
                .await;

            let err = client_for(&server.uri())
                .sdk()
                .open_orders(USER, None)
                .await
                .expect_err("every attempt is limited");

            assert_eq!(err.exit_code(), 11, "{err}");
            assert!(
                matches!(
                    err,
                    CliError::RateLimited { .. } | CliError::AddressRateLimited(_)
                ),
                "{err:?}"
            );
            if matches!(err, CliError::RateLimited { .. }) {
                assert_eq!(err.retryable(), Some(true), "{err:?}");
            }
            assert!(!err.to_string().contains("Couldn't reach"), "{err}");
            assert_eq!(requests(&server).await, 3, "one attempt and two retries");
        }
    }

    #[tokio::test]
    async fn every_sdk_read_shape_retries_a_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(429).set_body_raw("null", "application/json"))
            .mount(&server)
            .await;
        let api = client_for(&server.uri());
        let sdk = api.sdk();

        let reads = [
            sdk.all_mids(None).await.map(drop),
            sdk.api_agents(USER).await.map(drop),
            sdk.candle_snapshot("BTC", CandleInterval::OneMinute, 0, 1)
                .await
                .map(drop),
            sdk.clearinghouse_state(USER, None).await.map(drop),
            sdk.funding_history("BTC", 0, None).await.map(drop),
            sdk.gossip_priority_auction_status().await.map(drop),
            sdk.historical_orders(USER).await.map(drop),
            sdk.open_orders(USER, None).await.map(drop),
            sdk.user_fills(USER).await.map(drop),
            sdk.user_vault_equities(USER).await.map(drop),
        ];

        for read in reads {
            let err = read.expect_err("limited");
            assert!(matches!(err, CliError::RateLimited { .. }), "{err:?}");
        }
        assert_eq!(requests(&server).await, 10 * 3);
    }

    #[tokio::test]
    async fn a_server_error_is_retried_like_an_info_read() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        client_for(&server.uri())
            .sdk()
            .open_orders(USER, None)
            .await
            .expect("the retry succeeds");

        assert_eq!(requests(&server).await, 2);
    }

    #[tokio::test]
    async fn a_client_error_and_an_undecodable_reply_are_not_retried() {
        for reply in [
            ResponseTemplate::new(422).set_body_string("bad request"),
            ResponseTemplate::new(200).set_body_string("not json"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/info"))
                .respond_with(reply)
                .mount(&server)
                .await;

            client_for(&server.uri())
                .sdk()
                .open_orders(USER, None)
                .await
                .expect_err("not a transient failure");

            assert_eq!(requests(&server).await, 1);
        }
    }

    #[tokio::test]
    async fn only_a_shape_mismatch_is_left_for_the_caller_to_read_raw() {
        for (body, shape_mismatch) in [(r#"{"not":"a list"}"#, true), ("not json", false)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/info"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
                .mount(&server)
                .await;

            let read = client_for(&server.uri())
                .sdk()
                .historical_orders(USER)
                .await;

            match (shape_mismatch, read) {
                (true, Ok(None)) => {}
                (false, Err(_)) => {}
                (_, other) => panic!("{body}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_refused_connection_is_retried_then_reported_as_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let started = std::time::Instant::now();
        let err = client_for(&format!("http://127.0.0.1:{port}"))
            .sdk()
            .open_orders(USER, None)
            .await
            .expect_err("refused");

        assert!(matches!(err, CliError::Unavailable(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
