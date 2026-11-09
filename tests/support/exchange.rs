//! A mock Hyperliquid API for commands that read markets or place orders.

use std::ops::Deref;

use serde_json::{Value, json};
use wiremock::matchers::{body_json, body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::wire::{self, MarginRule};

/// A builder-deployed perp DEX on the mock exchange.
pub struct Hip3Dex {
    pub name: &'static str,
    /// The dex's `meta` universe: names carry the `dex:` prefix, as on the wire.
    pub universe: Vec<Value>,
    /// The dex-scoped `allMids` reply.
    pub mids: Value,
}

impl Hip3Dex {
    /// The dex `xyz` with the single market `xyz:XYZ100` (2 size decimals) at 123.5.
    pub fn xyz() -> Self {
        Self {
            name: "xyz",
            universe: vec![wire::perp("xyz:XYZ100", MarginRule::Either)],
            mids: json!({ "xyz:XYZ100": "123.5" }),
        }
    }
}

/// The exchange as the CLI sees it behind `HYPERLIQUID_API_BASE_URL`: BTC and
/// ETH on the default dex, a USDC-only spot universe, and any [`Hip3Dex`]es.
///
/// Every `/info` read is matched on its exact body, so an unscoped read and a
/// dex-scoped one of the same type are answered separately. That includes the
/// unscoped `allMids` the CLI sends once to probe an API override. Mount more
/// with [`Self::mount_info`] and [`Self::mount_exchange`]; the fixture derefs to
/// the [`MockServer`] for everything else.
pub struct ExchangeFixture {
    server: MockServer,
}

impl Deref for ExchangeFixture {
    type Target = MockServer;

    fn deref(&self) -> &MockServer {
        &self.server
    }
}

impl ExchangeFixture {
    /// The default dex only.
    pub async fn start() -> Self {
        Self::start_with(Vec::new()).await
    }

    pub async fn start_with(dexes: Vec<Hip3Dex>) -> Self {
        let fixture = Self {
            server: MockServer::start().await,
        };
        let mut listed = vec![Value::Null];
        listed.extend(
            dexes
                .iter()
                .map(|dex| json!({ "name": dex.name, "deployerFeeScale": "1" })),
        );
        fixture
            .mount_info(json!({"type": "allMids"}), wire::all_mids())
            .await;
        fixture
            .mount_info(json!({"type": "meta"}), wire::perp_meta_btc_eth())
            .await;
        fixture
            .mount_info(json!({"type": "spotMeta"}), wire::spot_meta_usdc_only())
            .await;
        fixture
            .mount_info(json!({"type": "perpDexs"}), Value::Array(listed))
            .await;
        for dex in dexes {
            fixture
                .mount_info(
                    json!({"type": "meta", "dex": dex.name}),
                    wire::perp_meta(dex.universe),
                )
                .await;
            fixture
                .mount_info(json!({"type": "allMids", "dex": dex.name}), dex.mids)
                .await;
        }
        fixture
    }

    /// Answer the `/info` request whose body is exactly `request`.
    pub async fn mount_info(&self, request: Value, response: Value) {
        Mock::given(method("POST"))
            .and(path("/info"))
            .and(body_json(request))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&self.server)
            .await;
    }

    /// Answer every `/info` request whose body contains `fields`, for reads
    /// that carry fields (a user address, a time range) the test does not pin.
    /// A mount made before this one for the same request wins.
    pub async fn mount_info_containing(&self, fields: Value, response: Value) {
        Mock::given(method("POST"))
            .and(path("/info"))
            .and(body_partial_json(fields))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&self.server)
            .await;
    }

    /// Answer the `/info` request whose body contains `fields` with an HTTP
    /// error `status` and no body, for a read that fails.
    pub async fn mount_info_failure(&self, fields: Value, status: u16) {
        Mock::given(method("POST"))
            .and(path("/info"))
            .and(body_partial_json(fields))
            .respond_with(ResponseTemplate::new(status))
            .mount(&self.server)
            .await;
    }

    /// Like [`Self::mount_info`], but outranking every mount made without it,
    /// to change an answer the fixture already gave.
    pub async fn override_info(&self, request: Value, response: Value) {
        Mock::given(method("POST"))
            .and(path("/info"))
            .and(body_json(request))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .with_priority(1)
            .mount(&self.server)
            .await;
    }

    /// Answer every `/exchange` request with `response`.
    pub async fn mount_exchange(&self, response: Value) {
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&self.server)
            .await;
    }

    /// Answer the next `/exchange` request, once, with `response`; mounted
    /// before [`Self::mount_exchange`] it gives a command a different answer
    /// for its first action than for the rest.
    pub async fn mount_exchange_once(&self, response: Value) {
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .up_to_n_times(1)
            .mount(&self.server)
            .await;
    }

    /// Answer every `/exchange` request with an HTTP `status` and a plain-text
    /// `body`, for a transport failure rather than an exchange reply.
    pub async fn mount_exchange_failure(&self, status: u16, body: &str) {
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&self.server)
            .await;
    }

    /// Answer every `/exchange` request with an order reply carrying `statuses`.
    pub async fn mount_order_statuses(&self, statuses: Vec<Value>) {
        self.mount_exchange(order_reply(statuses)).await;
    }

    pub async fn info_requests(&self, request_type: &str) -> Vec<Value> {
        super::info_requests(&self.server, request_type).await
    }

    pub async fn exchange_actions(&self) -> Vec<Value> {
        super::exchange_actions(&self.server).await
    }

    pub async fn exchange_bodies(&self) -> Vec<Value> {
        super::exchange_bodies(&self.server).await
    }
}

/// An `/exchange` order reply carrying `statuses`.
pub fn order_reply(statuses: Vec<Value>) -> Value {
    json!({
        "status": "ok",
        "response": {"type": "order", "data": {"statuses": statuses}}
    })
}
