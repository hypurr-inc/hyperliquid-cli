//! The raw `/info` paths share the API client's retry and status policy.

mod support;

use serde_json::json;
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, stdout_json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
async fn meta_retries_a_failing_read_then_reports_an_api_failure() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_failure(json!({"type": "metaAndAssetCtxs"}), 503)
        .await;

    let output = env
        .command_with_server(&server)
        .args(["--format", "json", "meta"])
        .assert()
        .code(12)
        .get_output()
        .stdout
        .clone();

    let error = stdout_json(&output);
    assert_eq!(error["reason"], "api_error");
    assert_eq!(error["retryable"], true);
    assert_eq!(server.info_requests("metaAndAssetCtxs").await.len(), 3);
}

#[tokio::test]
async fn raw_historical_orders_fallback_reports_a_rejected_status_as_an_api_failure() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    // The SDK decode fails on this shape, which sends the CLI to the raw read.
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(body_partial_json(json!({"type": "historicalOrders"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([1])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    server
        .mount_info_failure(json!({"type": "historicalOrders"}), 422)
        .await;

    let output = env
        .live_json(&server)
        .args(["orders", "history"])
        .assert()
        .code(12)
        .get_output()
        .stdout
        .clone();

    assert_eq!(stdout_json(&output)["reason"], "api_error");
    assert_eq!(server.info_requests("historicalOrders").await.len(), 2);
}
