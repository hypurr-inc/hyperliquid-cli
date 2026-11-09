//! `perps list` and `perps get` report each market's margin rule as
//! `margin_mode`, next to the unchanged deprecated `isolated_margin` flag.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::{IsolatedHome, fixture_perp_meta, json_stdout};

fn perp(name: &str, only_isolated: bool, margin_mode: Value) -> Value {
    json!({
        "name": name,
        "szDecimals": 2,
        "maxLeverage": 10,
        "onlyIsolated": only_isolated,
        "marginMode": margin_mode,
        "growthMode": "disabled"
    })
}

async fn exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start_with(vec![Hip3Dex {
        name: "xyz",
        universe: vec![
            perp("xyz:HOOD", true, json!("noCross")),
            perp("xyz:STRICT", true, json!("strictIsolated")),
            perp("xyz:OPEN", false, Value::Null),
        ],
        mids: json!({}),
    }])
    .await;
    server
        .override_info(
            json!({"type": "meta"}),
            fixture_perp_meta(vec![
                perp("BTC", false, Value::Null),
                perp("PUMP", true, json!("strictIsolated")),
                // `marginMode` absent: only the deprecated flag says anything.
                json!({
                    "name": "OLD",
                    "szDecimals": 2,
                    "maxLeverage": 5,
                    "onlyIsolated": true
                }),
            ]),
        )
        .await;
    server
}

fn run(env: &IsolatedHome, server: &ExchangeFixture, args: &[&str]) -> Value {
    let assert = env
        .command_with_server(server)
        .args(["--format", "json", "--testnet", "perps"])
        .args(args)
        .assert()
        .success();
    json_stdout(&assert)
}

fn modes(rows: &Value) -> Vec<(String, String, bool)> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap().to_string(),
                row["margin_mode"].as_str().unwrap().to_string(),
                row["isolated_margin"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn list_reports_the_margin_rule_of_every_default_dex_market() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let rows = run(&env, &server, &["list"]);

    assert_eq!(
        modes(&rows),
        [
            ("BTC".to_string(), "cross_or_isolated".to_string(), false),
            ("PUMP".to_string(), "strict_isolated".to_string(), true),
            ("OLD".to_string(), "isolated_only".to_string(), true),
        ]
    );
}

#[tokio::test]
async fn list_reports_the_margin_rule_of_every_hip3_market() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let rows = run(&env, &server, &["list", "--dex", "xyz"]);

    assert_eq!(
        modes(&rows),
        [
            ("xyz:HOOD".to_string(), "no_cross".to_string(), true),
            (
                "xyz:STRICT".to_string(),
                "strict_isolated".to_string(),
                true
            ),
            (
                "xyz:OPEN".to_string(),
                "cross_or_isolated".to_string(),
                false
            ),
        ]
    );
}

#[tokio::test]
async fn get_reports_the_margin_rule() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let default_dex = run(&env, &server, &["get", "PUMP"]);
    assert_eq!(default_dex["margin_mode"], "strict_isolated");
    assert_eq!(default_dex["isolated_margin"], true);

    let hip3 = run(&env, &server, &["get", "xyz:HOOD"]);
    assert_eq!(hip3["margin_mode"], "no_cross");
    assert_eq!(hip3["isolated_margin"], true);

    let flexible = run(&env, &server, &["get", "BTC"]);
    assert_eq!(flexible["margin_mode"], "cross_or_isolated");
    assert_eq!(flexible["isolated_margin"], false);
}
