mod support;

use predicates::prelude::*;
use std::net::TcpListener;
use support::{API_OVERRIDE_ENV, IsolatedHome};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cli_exits_11_for_actual_http_429_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string("Too many requests: rate limit exceeded"),
        )
        .mount(&server)
        .await;

    let env = IsolatedHome::new();
    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["perps", "list"])
        .assert()
        .code(11)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Too many requests to the Hyperliquid API",
        ))
        .stderr(predicate::str::contains("Couldn't reach").not());
}

#[tokio::test]
async fn cli_exits_11_for_structured_rate_limit_error_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "err",
            "response": "rate limit exceeded"
        })))
        .mount(&server)
        .await;

    let env = IsolatedHome::new();
    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["status"])
        .assert()
        .code(11)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Too many requests to the Hyperliquid API",
        ))
        .stderr(predicate::str::contains("Couldn't reach").not());
}

#[test]
fn cli_exits_12_for_unreachable_override_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let env = IsolatedHome::new();
    env.command()
        .env(API_OVERRIDE_ENV, url)
        .args(["perps", "list"])
        .assert()
        .code(12)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("127.0.0.1:"))
        .stderr(predicate::str::contains("Too many requests to the Hyperliquid API").not());
}
