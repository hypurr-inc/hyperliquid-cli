mod support;

use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::reads::{read, read_json};
use support::stderr_text;

/// BTC, the spot pair PURR/USDC and one outcome (40) whose Yes side is `#400`.
async fn metadata_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "spotMeta"}),
            json!({
                "universe": [{"name": "PURR/USDC", "index": 0, "tokens": [1, 0]}],
                "tokens": [
                    {"name": "USDC", "index": 0, "tokenId": "0x00000000000000000000000000000000", "szDecimals": 6, "weiDecimals": 6, "evmContract": null},
                    {"name": "PURR", "index": 1, "tokenId": "0x00000000000000000000000000000001", "szDecimals": 0, "weiDecimals": 0, "evmContract": null}
                ]
            }),
        )
        .await;
    server
        .mount_info(
            json!({"type": "outcomeMeta"}),
            json!({
                "outcomes": [{
                    "outcome": 40,
                    "name": "BTC price binary",
                    "description": "class:priceBinary|underlying:BTC|expiry:20260510-0000|targetPrice:100000|period:1d",
                    "sideSpecs": [
                        {"name": "Yes", "token": 400},
                        {"name": "No", "token": 401}
                    ]
                }],
                "questions": []
            }),
        )
        .await;
    server
}

#[tokio::test]
async fn asset_decode_resolves_perp_spot_and_outcome_ids() {
    let server = metadata_exchange().await;

    let perp = read_json(&server, &["asset", "decode", "0"]);
    assert_eq!(perp["asset_id"], 0);
    assert_eq!(perp["kind"], "perp");
    assert_eq!(perp["lookup_status"], "metadata_found");
    assert_eq!(perp["cli_input"], "BTC");
    assert_eq!(perp["perp_index"], 0);

    let spot = read_json(&server, &["asset", "decode", "10000"]);
    assert_eq!(spot["kind"], "spot");
    assert_eq!(spot["lookup_status"], "metadata_found");
    assert_eq!(spot["cli_input"], "PURR/USDC");
    assert_eq!(spot["spot_index"], 0);
    assert_eq!(spot["base"], "PURR");
    assert_eq!(spot["quote"], "USDC");

    let outcome = read_json(&server, &["asset", "decode", "100000400"]);
    assert_eq!(outcome["kind"], "outcome");
    assert_eq!(outcome["lookup_status"], "metadata_found");
    assert_eq!(outcome["coin"], "#400");
    assert_eq!(outcome["token"], "+400");
    assert_eq!(outcome["outcome"], 40);
    assert_eq!(outcome["side"], 0);
    assert_eq!(outcome["side_name"], "Yes");
    assert_eq!(outcome["market_title"], "BTC above 100000 Yes May 10 00:00");
    assert_eq!(outcome["slug"], "btc-above-100000-yes-may-10-0000");
    assert_eq!(outcome["condition"], "BTC above 100000 at May 10 00:00");
    assert_eq!(outcome["condition_class"], "priceBinary");
    assert_eq!(outcome["underlying"], "BTC");
    assert_eq!(outcome["target_price"], "100000");
    assert_eq!(outcome["expiry"], "20260510-0000");
    assert_eq!(outcome["period"], "1d");
    assert!(
        outcome["description"]
            .as_str()
            .unwrap()
            .contains("underlying:BTC")
    );
}

#[tokio::test]
async fn asset_decode_returns_formula_only_for_unresolved_but_valid_ids() {
    let server = metadata_exchange().await;

    let hip3 = read_json(&server, &["asset", "decode", "110035"]);
    assert_eq!(hip3["kind"], "hip3_perp");
    assert_eq!(hip3["lookup_status"], "formula_only");
    assert_eq!(hip3["cli_input"], Value::Null);
    assert_eq!(hip3["dex_slot"], 1);
    assert_eq!(hip3["perp_index"], 35);

    let outcome = read_json(&server, &["asset", "decode", "100000410"]);
    assert_eq!(outcome["kind"], "outcome");
    assert_eq!(outcome["lookup_status"], "formula_only");
    assert_eq!(outcome["cli_input"], "#410");
    assert_eq!(outcome["outcome"], 41);
    assert_eq!(outcome["side"], 0);
    assert_eq!(outcome["side_name"], "Yes");
}

#[tokio::test]
async fn asset_decode_metadata_failures_degrade_to_formula_only() {
    // Only the override probe is answered: every metadata read fails.
    let server = ExchangeFixture::probe_only().await;

    let perp = read_json(&server, &["asset", "decode", "0"]);
    assert_eq!(perp["kind"], "perp");
    assert_eq!(perp["lookup_status"], "formula_only");
    assert_eq!(perp["cli_input"], Value::Null);
    assert_eq!(perp["perp_index"], 0);

    let outcome = read_json(&server, &["asset", "decode", "100000401"]);
    assert_eq!(outcome["kind"], "outcome");
    assert_eq!(outcome["lookup_status"], "formula_only");
    assert_eq!(outcome["cli_input"], "#401");
    assert_eq!(outcome["side"], 1);
    assert_eq!(outcome["side_name"], "No");
}

#[tokio::test]
async fn asset_search_finds_symbols_pairs_and_outcome_slugs() {
    let server = metadata_exchange().await;

    let btc = read_json(&server, &["asset", "search", "BTC", "--limit", "1"]);
    assert_eq!(btc.as_array().unwrap().len(), 1);
    assert_eq!(btc[0]["asset_id"], 0);
    assert_eq!(btc[0]["kind"], "perp");
    assert_eq!(btc[0]["cli_input"], "BTC");

    let spot = read_json(&server, &["asset", "search", "PURR/USDC", "--limit", "1"]);
    assert_eq!(spot[0]["asset_id"], 10000);
    assert_eq!(spot[0]["kind"], "spot");
    assert_eq!(spot[0]["cli_input"], "PURR/USDC");

    let slug = "btc-above-100000-yes-may-10-0000";
    let outcome = read_json(&server, &["asset", "search", slug, "--limit", "1"]);
    assert_eq!(outcome[0]["asset_id"], 100000400);
    assert_eq!(outcome[0]["kind"], "outcome");
    assert_eq!(outcome[0]["slug"], slug);
    assert_eq!(outcome[0]["cli_input"], "#400");
}

#[tokio::test]
async fn asset_search_numeric_query_falls_back_to_decode() {
    let server = metadata_exchange().await;

    let rows = read_json(&server, &["asset", "search", "110035", "--limit", "1"]);

    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["asset_id"], 110035);
    assert_eq!(rows[0]["kind"], "hip3_perp");
    assert_eq!(rows[0]["lookup_status"], "formula_only");
    assert_eq!(rows[0]["dex_slot"], 1);
    assert_eq!(rows[0]["perp_index"], 35);
}

#[tokio::test]
async fn asset_decode_supports_json_projection_for_agents() {
    let server = metadata_exchange().await;

    let projected = read_json(
        &server,
        &[
            "--select",
            "asset_id,kind,cli_input",
            "asset",
            "decode",
            "0",
        ],
    );

    assert_eq!(
        projected,
        json!({"asset_id": 0, "kind": "perp", "cli_input": "BTC"})
    );
}

#[tokio::test]
async fn asset_decode_invalid_outcome_side_is_structured_not_a_hard_failure() {
    let server = metadata_exchange().await;

    let decoded = read_json(&server, &["asset", "decode", "100000402"]);

    assert_eq!(decoded["kind"], "outcome");
    assert_eq!(decoded["lookup_status"], "invalid_id");
    assert_eq!(decoded["cli_input"], Value::Null);
    assert_eq!(decoded["encoding"], 402);
    assert_eq!(decoded["outcome"], 40);
    assert_eq!(decoded["side"], 2);
    assert_eq!(
        decoded["reason"],
        "only binary outcome sides 0 and 1 are supported"
    );
}

#[tokio::test]
async fn asset_search_says_on_stderr_when_metadata_is_unavailable() {
    let server = ExchangeFixture::probe_only().await;

    let assert = read(&server, &["asset", "search", "110035"]).success();

    let rows = support::json_stdout(&assert);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["lookup_status"], "formula_only");
    let stderr = stderr_text(&assert);
    assert!(stderr.contains("asset metadata unavailable"), "{stderr}");
    assert!(stderr.contains("outcome metadata unavailable"), "{stderr}");
}

#[tokio::test]
async fn asset_search_names_why_a_dex_s_metadata_is_unavailable() {
    // `xyz` is listed but its `meta` is not served, so the mock answers 404.
    let server = ExchangeFixture::start().await;
    server
        .override_info(
            json!({"type": "perpDexs"}),
            json!([null, {"name": "xyz", "deployerFeeScale": "1"}]),
        )
        .await;

    let assert = read(&server, &["asset", "search", "BTC"]).success();

    let stderr = stderr_text(&assert);
    let notice = stderr
        .lines()
        .find(|line| line.contains("HIP-3 dex xyz metadata unavailable, results are limited: "))
        .unwrap_or_else(|| panic!("{stderr}"));
    let reason = notice.split("results are limited: ").nth(1).unwrap();
    assert!(!reason.trim().is_empty(), "{notice}");
}
