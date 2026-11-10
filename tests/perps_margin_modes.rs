//! `perps list` and `perps get` report each market's margin rule as
//! `margin_mode`, next to the unchanged deprecated `isolated_margin` flag.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::reads::read_json;
use support::wire::{self, MarginRule};

async fn exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start_with(vec![Hip3Dex {
        name: "xyz",
        universe: vec![
            wire::perp("xyz:HOOD", MarginRule::NoCross),
            wire::perp("xyz:STRICT", MarginRule::StrictIsolated),
            wire::perp("xyz:OPEN", MarginRule::Either),
        ],
        mids: json!({}),
    }])
    .await;
    server
        .override_info(
            json!({"type": "meta"}),
            wire::perp_meta(vec![
                wire::perp("BTC", MarginRule::Either),
                wire::perp("PUMP", MarginRule::StrictIsolated),
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

fn perps(server: &ExchangeFixture, args: &[&str]) -> Value {
    read_json(server, &[&["perps"], args].concat())
}

fn modes(rows: &Value) -> Vec<(&str, &str, bool)> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap(),
                row["margin_mode"].as_str().unwrap(),
                row["isolated_margin"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn list_reports_the_margin_rule_of_every_default_dex_market() {
    let server = exchange().await;

    assert_eq!(
        modes(&perps(&server, &["list"])),
        [
            ("BTC", "cross_or_isolated", false),
            ("PUMP", "strict_isolated", true),
            ("OLD", "isolated_only", true),
        ]
    );
}

#[tokio::test]
async fn list_reports_the_margin_rule_of_every_hip3_market() {
    let server = exchange().await;

    assert_eq!(
        modes(&perps(&server, &["list", "--dex", "xyz"])),
        [
            ("xyz:HOOD", "no_cross", true),
            ("xyz:STRICT", "strict_isolated", true),
            ("xyz:OPEN", "cross_or_isolated", false),
        ]
    );
}

#[tokio::test]
async fn get_reports_the_margin_rule() {
    let server = exchange().await;

    for (market, mode, isolated) in [
        ("PUMP", "strict_isolated", true),
        ("xyz:HOOD", "no_cross", true),
        ("BTC", "cross_or_isolated", false),
    ] {
        let reported = perps(&server, &["get", market]);
        assert_eq!(reported["margin_mode"], mode, "{market}");
        assert_eq!(reported["isolated_margin"], isolated, "{market}");
    }
}
