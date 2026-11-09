//! Read commands for info endpoints the CLI did not expose, and the fields it
//! dropped from the ones it did.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::{IsolatedHome, fixture_perp, fixture_perp_meta};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USER: &str = "0x0000000000000000000000000000000000000001";
const VAULT: &str = "0x0000000000000000000000000000000000000002";
const START: u64 = 1_700_000_000_000;

fn run_args(server: &MockServer, args: &[&str]) -> assert_cmd::assert::Assert {
    let env = IsolatedHome::new();
    env.command_with_server(server)
        .args(["--format", "json"])
        .args(args)
        .assert()
}

fn json_of(server: &MockServer, args: &[&str]) -> Value {
    support::json_stdout(&run_args(server, args).success())
}

fn stderr_text(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

// ---- account orders --dex ------------------------------------------------

#[tokio::test]
async fn account_orders_reads_the_requested_hip3_dex() {
    let server = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;
    let mut order = support::fixture_basic_order("xyz:XYZ100", 9);
    order["limitPx"] = json!("123.5");
    server
        .mount_info(
            json!({"type": "frontendOpenOrders", "user": USER, "dex": "xyz"}),
            json!([order]),
        )
        .await;

    let rows = json_of(&server, &["account", "orders", USER, "--dex", "xyz"]);

    assert_eq!(rows[0]["coin"], "xyz:XYZ100");
    assert_eq!(rows[0]["oid"], 9);
}

#[tokio::test]
async fn account_orders_refuses_an_unknown_dex_before_reading_orders() {
    let server = ExchangeFixture::start().await;

    let assert = run_args(&server, &["account", "orders", USER, "--dex", "nope"]).failure();

    assert_eq!(assert.get_output().status.code(), Some(13));
    assert!(server.info_requests("frontendOpenOrders").await.is_empty());
}

// ---- account portfolio ---------------------------------------------------

#[tokio::test]
async fn portfolio_positions_carry_return_on_equity_cumulative_funding_and_max_leverage() {
    let server = MockServer::start().await;
    support::mount_override_healthcheck(&server).await;
    support::mount_account_state(&server, vec![], vec![], "9000", "100", 5).await;

    let portfolio = json_of(&server, &["account", "portfolio", USER]);

    let position = &portfolio["positions"][0];
    assert_eq!(position["return_on_equity"], "0.1");
    assert_eq!(position["max_leverage"], 50);
    assert_eq!(
        position["cum_funding"],
        json!({"all_time": "0", "since_open": "0", "since_change": "0"})
    );
    assert_eq!(position["leverage"], "5x cross");
}

// ---- account role / multisig / asset data --------------------------------

#[tokio::test]
async fn account_role_names_the_master_of_a_subaccount() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "userRole", "user": USER}),
            json!({"role": "subAccount", "data": {"master": VAULT}}),
        )
        .await;

    let role = json_of(&server, &["account", "role", USER]);

    assert_eq!(role["role"], "subAccount");
    assert_eq!(role["data"]["master"], VAULT);
}

#[tokio::test]
async fn multisig_signers_of_a_plain_account_say_so_on_stderr_and_print_null() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "userToMultiSigSigners", "user": USER}),
            Value::Null,
        )
        .await;

    let assert = run_args(&server, &["account", "multisig-signers", USER]).success();

    assert_eq!(support::json_stdout(&assert), Value::Null);
}

#[tokio::test]
async fn multisig_signers_are_listed_with_their_threshold() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "userToMultiSigSigners", "user": USER}),
            json!({"authorizedUsers": [VAULT], "threshold": 1}),
        )
        .await;

    let signers = json_of(&server, &["account", "multisig-signers", USER]);

    assert_eq!(signers["authorized_users"][0], VAULT);
    assert_eq!(signers["threshold"], 1);
}

#[tokio::test]
async fn asset_data_reads_leverage_and_trade_capacity_without_a_position() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "activeAssetData", "user": USER, "coin": "BTC"}),
            json!({
                "user": USER,
                "coin": "BTC",
                "leverage": {"type": "cross", "value": 5},
                "maxTradeSzs": ["0.37018", "0.12019"],
                "availableToTrade": ["5517.6", "1791.4"],
                "markPx": "74526.0"
            }),
        )
        .await;

    let data = json_of(&server, &["account", "asset-data", USER, "--coin", "btc"]);

    assert_eq!(data["leverage"]["value"], 5);
    assert_eq!(data["max_trade_szs"][1], "0.12019");
    assert_eq!(data["mark_px"], "74526.0");
}

#[tokio::test]
async fn asset_data_qualifies_a_hip3_coin_with_its_dex() {
    let server = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;
    server
        .mount_info(
            json!({"type": "activeAssetData", "user": USER, "coin": "xyz:XYZ100"}),
            json!({"user": USER, "coin": "xyz:XYZ100", "leverage": {"type": "isolated", "value": 20, "rawUsd": "0.0"}, "maxTradeSzs": ["0.0", "0.0"], "availableToTrade": ["0.0", "0.0"], "markPx": "25451.0"}),
        )
        .await;

    let data = json_of(
        &server,
        &["account", "asset-data", USER, "--coin", "xyz:XYZ100"],
    );

    assert_eq!(data["leverage"]["type"], "isolated");
}

// ---- perps list / get / meta ---------------------------------------------

#[tokio::test]
async fn perps_list_flags_delisted_markets_and_hides_none() {
    let server = ExchangeFixture::start().await;
    let mut gone = fixture_perp("OLD", 2, 10);
    gone["isDelisted"] = json!(true);
    server
        .override_info(
            json!({"type": "meta"}),
            fixture_perp_meta(vec![fixture_perp("BTC", 5, 50), gone]),
        )
        .await;

    let rows = json_of(&server, &["perps", "list"]);

    assert_eq!(rows.as_array().unwrap().len(), 2, "nothing is hidden");
    assert_eq!(rows[0]["delisted"], false);
    assert_eq!(rows[1]["delisted"], true);
    assert!(rows[0].get("deployer_fee_scale").is_none());
}

#[tokio::test]
async fn hip3_markets_show_their_deployer_fee_scale() {
    let mut market = fixture_perp("xyz:XYZ100", 2, 10);
    market["deployerFeeScale"] = json!("0.5");
    let server = ExchangeFixture::start_with(vec![Hip3Dex {
        name: "xyz",
        universe: vec![market],
        mids: json!({"xyz:XYZ100": "1"}),
    }])
    .await;

    let rows = json_of(&server, &["perps", "list", "--dex", "xyz"]);

    assert_eq!(rows[0]["deployer_fee_scale"], "0.5");
}

async fn mount_meta_contexts(server: &ExchangeFixture) {
    server
        .mount_info(
            json!({"type": "metaAndAssetCtxs"}),
            json!([
                {"universe": [{"name": "BTC", "szDecimals": 5, "maxLeverage": 50}]},
                [{"markPx": "1", "midPx": "1"}]
            ]),
        )
        .await;
}

fn dex_with_scales(scales: &[(&str, &str)]) -> Hip3Dex {
    Hip3Dex {
        name: "xyz",
        universe: scales
            .iter()
            .map(|(name, scale)| {
                let mut market = fixture_perp(name, 2, 10);
                market["deployerFeeScale"] = json!(scale);
                market
            })
            .collect(),
        mids: json!({}),
    }
}

#[tokio::test]
async fn meta_reads_the_deployer_fee_scale_from_the_dex_markets() {
    let server = ExchangeFixture::start_with(vec![dex_with_scales(&[
        ("xyz:A", "0.1111"),
        ("xyz:B", "0.1111"),
    ])])
    .await;
    mount_meta_contexts(&server).await;

    let meta = json_of(&server, &["meta"]);

    assert_eq!(meta["dexes"][0]["name"], "xyz");
    assert_eq!(meta["dexes"][0]["deployer_fee_scale"], "0.1111");
    assert!(
        meta["dexes"][0]
            .get("deployer_fee_scale_by_market")
            .is_none()
    );
}

#[tokio::test]
async fn meta_lists_each_markets_scale_when_a_dex_mixes_them() {
    let server =
        ExchangeFixture::start_with(vec![dex_with_scales(&[("xyz:A", "1.0"), ("xyz:B", "0.5")])])
            .await;
    mount_meta_contexts(&server).await;

    let dex = json_of(&server, &["meta"])["dexes"][0].clone();

    assert_eq!(dex["deployer_fee_scale"], Value::Null);
    assert_eq!(
        dex["deployer_fee_scale_by_market"],
        json!([
            {"coin": "xyz:A", "deployer_fee_scale": "1.0"},
            {"coin": "xyz:B", "deployer_fee_scale": "0.5"}
        ])
    );
}

// ---- perps dexs / dex-limits / at-oi-cap ---------------------------------

#[tokio::test]
async fn perps_dexs_lists_deployer_fee_recipient_caps_and_multipliers() {
    let server = ExchangeFixture::start_with(vec![dex_with_scales(&[("xyz:A", "0.5")])]).await;
    server
        .override_info(
            json!({"type": "perpDexs"}),
            json!([null, {
                "name": "xyz",
                "fullName": "XYZ",
                "deployer": USER,
                "oracleUpdater": null,
                "feeRecipient": VAULT,
                "assetToStreamingOiCap": [["xyz:A", "25000000.0"]],
                "assetToFundingMultiplier": [["xyz:A", "2.0"]]
            }]),
        )
        .await;

    let dexs = json_of(&server, &["perps", "dexs"]);

    assert_eq!(
        dexs.as_array().unwrap().len(),
        1,
        "the null slot is skipped"
    );
    let dex = &dexs[0];
    assert_eq!(dex["name"], "xyz");
    assert_eq!(dex["index"], 1, "the slot fixes the dex's asset ids");
    assert_eq!(dex["full_name"], "XYZ");
    assert_eq!(dex["deployer"], USER);
    assert_eq!(dex["oracle_updater"], Value::Null);
    assert_eq!(dex["fee_recipient"], VAULT);
    assert_eq!(dex["deployer_fee_scale"], "0.5");
    assert_eq!(
        dex["oi_caps"],
        json!([{"coin": "xyz:A", "oi_cap": "25000000.0"}])
    );
    assert_eq!(
        dex["funding_multipliers"],
        json!([{"coin": "xyz:A", "multiplier": "2.0"}])
    );
}

#[tokio::test]
async fn dex_limits_joins_the_limits_and_the_net_deposit() {
    let server = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;
    server
        .mount_info(
            json!({"type": "perpDexLimits", "dex": "xyz"}),
            json!({
                "totalOiCap": "10000000000.0",
                "oiSzCapPerPerp": "20000000000.0",
                "maxTransferNtl": "3000000000.0",
                "coinToOiCap": [["xyz:AAPL", "200000000.0"]]
            }),
        )
        .await;
    server
        .mount_info(
            json!({"type": "perpDexStatus", "dex": "xyz"}),
            json!({"totalNetDeposit": "1209140392.19"}),
        )
        .await;

    let limits = json_of(&server, &["perps", "dex-limits", "XYZ"]);

    assert_eq!(
        limits["dex"], "xyz",
        "the dex is sent as the exchange spells it"
    );
    assert_eq!(limits["total_oi_cap"], "10000000000.0");
    assert_eq!(limits["oi_sz_cap_per_perp"], "20000000000.0");
    assert_eq!(limits["max_transfer_ntl"], "3000000000.0");
    assert_eq!(limits["total_net_deposit"], "1209140392.19");
    assert_eq!(
        limits["oi_caps"],
        json!([{"coin": "xyz:AAPL", "oi_cap": "200000000.0"}])
    );
}

#[tokio::test]
async fn dex_limits_refuses_an_unknown_dex_before_reading_limits() {
    let server = ExchangeFixture::start().await;

    let assert = run_args(&server, &["perps", "dex-limits", "nope"]).failure();

    assert_eq!(assert.get_output().status.code(), Some(13));
    assert!(server.info_requests("perpDexLimits").await.is_empty());
}

#[tokio::test]
async fn at_oi_cap_lists_the_capped_perps_of_a_dex() {
    let server = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;
    server
        .mount_info(
            json!({"type": "perpsAtOpenInterestCap"}),
            json!(["CANTO", "FTM"]),
        )
        .await;
    server
        .mount_info(
            json!({"type": "perpsAtOpenInterestCap", "dex": "xyz"}),
            json!(["xyz:XYZ100"]),
        )
        .await;

    assert_eq!(
        json_of(&server, &["perps", "at-oi-cap"]),
        json!([{"coin": "CANTO"}, {"coin": "FTM"}])
    );
    assert_eq!(
        json_of(&server, &["perps", "at-oi-cap", "--dex", "xyz"]),
        json!([{"coin": "xyz:XYZ100"}])
    );
}

// ---- perps funding-predicted / funding-history ---------------------------

#[tokio::test]
async fn predicted_fundings_flatten_venue_pairs_with_the_interval_when_stated() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "predictedFundings"}),
            json!([
                ["BTC", [
                    ["BinPerp", {"fundingRate": "0.00005", "nextFundingTime": 1794240000000_u64, "fundingIntervalHours": 4}],
                    ["HlPerp", {"fundingRate": "0.0000125", "nextFundingTime": 1794232800000_u64}],
                    ["BybitPerp", null]
                ]],
                ["ETH", [["HlPerp", {"fundingRate": "0.00001", "nextFundingTime": 1794232800000_u64}]]]
            ]),
        )
        .await;

    let all = json_of(&server, &["perps", "funding-predicted"]);
    assert_eq!(all.as_array().unwrap().len(), 3, "a null venue is skipped");
    assert_eq!(
        all[0],
        json!({"coin": "BTC", "venue": "BinPerp", "funding_rate": "0.00005", "next_funding_time": 1794240000000_u64, "funding_interval_hours": 4})
    );
    assert!(all[1].get("funding_interval_hours").is_none());

    let btc = json_of(&server, &["perps", "funding-predicted", "--coin", "btc"]);
    assert_eq!(btc.as_array().unwrap().len(), 2);
}

fn funding_rate(time: u64) -> Value {
    json!({"coin": "BTC", "fundingRate": "0.0000125", "premium": "0.0001", "time": time})
}

fn funding_page(from: u64, count: u64) -> Vec<Value> {
    (from..from + count).map(funding_rate).collect()
}

fn history_request(start: u64) -> Value {
    json!({"type": "fundingHistory", "coin": "BTC", "startTime": start})
}

#[tokio::test]
async fn funding_history_lists_rates_and_announces_a_full_page() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(history_request(START), json!(funding_page(START, 500)))
        .await;

    let assert = run_args(
        &server,
        &[
            "perps",
            "funding-history",
            "BTC",
            "--start",
            &START.to_string(),
        ],
    )
    .success();

    let rows = support::json_stdout(&assert);
    assert_eq!(rows.as_array().unwrap().len(), 500);
    assert_eq!(
        rows[0],
        json!({"coin": "BTC", "funding_rate": "0.0000125", "premium": "0.0001", "time": START})
    );
    let warning: Value = serde_json::from_str(stderr_text(&assert).trim()).unwrap();
    assert_eq!(warning["warning"]["cap"], 500);
    assert_eq!(warning["warning"]["next_start"], START + 499);
}

#[tokio::test]
async fn funding_history_all_pages_through_the_window() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(history_request(START), json!(funding_page(START, 500)))
        .await;
    server
        .mount_info(
            history_request(START + 499),
            json!([funding_rate(START + 499), funding_rate(START + 500)]),
        )
        .await;

    let assert = run_args(
        &server,
        &[
            "perps",
            "funding-history",
            "BTC",
            "--start",
            &START.to_string(),
            "--all",
        ],
    )
    .success();

    assert_eq!(support::json_stdout(&assert).as_array().unwrap().len(), 501);
    assert_eq!(stderr_text(&assert), "");
}

#[tokio::test]
async fn funding_history_sends_the_end_of_the_window() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "fundingHistory", "coin": "BTC", "startTime": START, "endTime": START + 10}),
            json!([funding_rate(START)]),
        )
        .await;

    let rows = json_of(
        &server,
        &[
            "perps",
            "funding-history",
            "BTC",
            "--start",
            &START.to_string(),
            "--end",
            &(START + 10).to_string(),
        ],
    );

    assert_eq!(rows.as_array().unwrap().len(), 1);
}

// ---- spot token / spot get -----------------------------------------------

fn spot_meta_with_hype() -> Value {
    json!({
        "universe": [
            {"name": "PURR/USDC", "index": 0, "tokens": [1, 0]},
            {"name": "@107", "index": 107, "tokens": [2, 0]}
        ],
        "tokens": [
            {"name": "USDC", "index": 0, "tokenId": "0x6d1e7cde53ba9467b783cb7c530ce054", "szDecimals": 8, "weiDecimals": 8, "evmContract": null},
            {"name": "PURR", "index": 1, "tokenId": "0xc1fb593aeffbeb02f85e0308e9956a90", "szDecimals": 0, "weiDecimals": 5, "evmContract": null},
            {"name": "HYPE", "index": 2, "tokenId": "0x0D01DC56DCAACA66AD901C959B4011EC", "szDecimals": 2, "weiDecimals": 8, "evmContract": null}
        ]
    })
}

#[tokio::test]
async fn spot_token_resolves_a_name_to_its_token_id() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), spot_meta_with_hype())
        .await;
    server
        .mount_info(
            json!({"type": "tokenDetails", "tokenId": "0x0d01dc56dcaaca66ad901c959b4011ec"}),
            json!({"name": "HYPE", "maxSupply": "1000000000.0", "szDecimals": 2, "midPx": "73.7", "genesis": {"userBalances": [[USER, "1.0"]], "existingTokenBalances": []}}),
        )
        .await;

    let details = json_of(&server, &["spot", "token", "hype"]);

    assert_eq!(details["name"], "HYPE");
    assert_eq!(details["max_supply"], "1000000000.0");
    assert_eq!(details["genesis"]["user_balances"][0][1], "1.0");
}

#[tokio::test]
async fn spot_token_takes_a_token_id_as_given() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "tokenDetails", "tokenId": "0x0d01dc56dcaaca66ad901c959b4011ec"}),
            json!({"name": "HYPE"}),
        )
        .await;

    let details = json_of(
        &server,
        &["spot", "token", "0x0D01DC56DCAACA66AD901C959B4011EC"],
    );

    assert_eq!(details["name"], "HYPE");
}

#[tokio::test]
async fn spot_token_refuses_a_malformed_token_id_and_an_unknown_name() {
    let server = ExchangeFixture::start().await;

    let malformed = run_args(&server, &["spot", "token", "0x1234"]).failure();
    assert_eq!(malformed.get_output().status.code(), Some(2));

    let unknown = run_args(&server, &["spot", "token", "NOPE"]).failure();
    assert_eq!(unknown.get_output().status.code(), Some(13));
    assert!(server.info_requests("tokenDetails").await.is_empty());
}

#[tokio::test]
async fn spot_get_adds_prices_and_volume_from_the_asset_context() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), spot_meta_with_hype())
        .await;
    server
        .mount_info(
            json!({"type": "spotMetaAndAssetCtxs"}),
            json!([
                {"universe": [], "tokens": []},
                [
                    {"coin": "@107", "markPx": "9", "midPx": "9"},
                    {"coin": "PURR/USDC", "markPx": "0.09155", "midPx": "0.0915055", "prevDayPx": "0.099126", "dayNtlVlm": "990489.1359", "dayBaseVlm": "10566932.0"}
                ]
            ]),
        )
        .await;

    let pair = json_of(&server, &["spot", "get", "PURR/USDC"]);

    assert_eq!(pair["symbol"], "PURR/USDC");
    assert_eq!(pair["mark_price"], "0.09155");
    assert_eq!(pair["mid_price"], "0.0915055");
    assert_eq!(pair["prev_day_price"], "0.099126");
    assert_eq!(pair["day_notional_volume"], "990489.1359");
    assert_eq!(pair["day_base_volume"], "10566932.0");
    assert_eq!(pair["tick_size_at_price_1"], json!("0.0001"));
}

#[tokio::test]
async fn spot_get_leaves_prices_null_for_a_market_with_no_book() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), spot_meta_with_hype())
        .await;
    server
        .mount_info(
            json!({"type": "spotMetaAndAssetCtxs"}),
            json!([{"universe": [], "tokens": []}, [{"coin": "PURR/USDC", "markPx": "0.1", "midPx": null}]]),
        )
        .await;

    let pair = json_of(&server, &["spot", "get", "PURR/USDC"]);

    assert_eq!(pair["mark_price"], "0.1");
    assert_eq!(pair["mid_price"], Value::Null);
}

#[tokio::test]
async fn spot_list_does_not_read_asset_contexts() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), spot_meta_with_hype())
        .await;

    json_of(&server, &["spot", "list"]);

    assert!(
        server
            .info_requests("spotMetaAndAssetCtxs")
            .await
            .is_empty()
    );
}

// ---- vaults --------------------------------------------------------------

async fn mount_stats(server: &MockServer, network: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/stats-data/{network}/vaults")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn stats_vault(name: &str, address: &str, tvl: &str, apr: Value, kind: &str) -> Value {
    json!({
        "apr": apr,
        "pnls": [["day", ["0.0"]]],
        "summary": {
            "name": name,
            "vaultAddress": address,
            "leader": USER,
            "tvl": tvl,
            "isClosed": false,
            "relationship": {"type": kind},
            "createTimeMillis": 1_700_000_000_000_i64
        }
    })
}

#[tokio::test]
async fn vault_list_reads_the_stats_dataset_not_the_empty_info_summaries() {
    let server = ExchangeFixture::start().await;
    mount_stats(
        &server,
        "Mainnet",
        json!([
            stats_vault(
                "Small",
                "0x00000000000000000000000000000000000000a1",
                "10.5",
                json!(-1.8525797436137323e-6),
                "normal"
            ),
            stats_vault(
                "Big",
                "0x00000000000000000000000000000000000000a2",
                "149271215.28",
                json!(0.2496),
                "parent"
            ),
        ]),
    )
    .await;

    let rows = json_of(&server, &["vault", "list"]);

    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["name"], "Big", "sorted by TVL");
    assert_eq!(rows[0]["tvl"], "149271215.28");
    assert_eq!(rows[0]["apr"], "0.2496");
    assert_eq!(rows[0]["kind"], "parent");
    assert_eq!(rows[1]["apr"], "-0.0000018525797436137323");
    assert!(server.info_requests("vaultSummaries").await.is_empty());
}

#[tokio::test]
async fn vault_search_filters_the_stats_dataset() {
    let server = ExchangeFixture::start().await;
    mount_stats(
        &server,
        "Mainnet",
        json!([
            stats_vault(
                "Alpha Carry",
                "0x00000000000000000000000000000000000000a1",
                "10",
                json!(0.1),
                "normal"
            ),
            stats_vault(
                "Beta",
                "0x00000000000000000000000000000000000000a2",
                "20",
                json!(0.2),
                "normal"
            ),
        ]),
    )
    .await;

    let rows = json_of(&server, &["vault", "search", "carry"]);

    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["name"], "Alpha Carry");
}

#[tokio::test]
async fn vault_list_reads_the_testnet_dataset_on_testnet() {
    let server = ExchangeFixture::start().await;
    mount_stats(
        &server,
        "Testnet",
        json!([stats_vault(
            "Test Vault",
            "0x00000000000000000000000000000000000000a1",
            "1",
            json!(0),
            "normal"
        )]),
    )
    .await;

    let rows = json_of(&server, &["--testnet", "vault", "list"]);

    assert_eq!(rows[0]["name"], "Test Vault");
}

#[tokio::test]
async fn vault_list_with_a_user_adds_their_deposit_from_their_vault_equities() {
    let server = ExchangeFixture::start().await;
    mount_stats(
        &server,
        "Mainnet",
        json!([
            stats_vault(
                "Held",
                "0x00000000000000000000000000000000000000A1",
                "10",
                json!(0.1),
                "normal"
            ),
            stats_vault(
                "Other",
                "0x00000000000000000000000000000000000000a2",
                "20",
                json!(0.2),
                "normal"
            ),
        ]),
    )
    .await;
    server
        .mount_info(
            json!({"type": "userVaultEquities", "user": USER}),
            json!([{"vaultAddress": "0x00000000000000000000000000000000000000a1", "equity": "316235.93", "lockedUntilTimestamp": 1}]),
        )
        .await;

    let rows = json_of(
        &server,
        &["vault", "list", "--user", USER, "--sort", "name"],
    );

    assert_eq!(rows[0]["name"], "Held");
    assert_eq!(rows[0]["user_deposit"], "316235.93");
    assert_eq!(rows[1]["user_deposit"], Value::Null);
}

#[tokio::test]
async fn vault_list_fails_loudly_when_the_stats_host_does() {
    let server = ExchangeFixture::start().await;
    Mock::given(method("GET"))
        .and(path("/stats-data/Mainnet/vaults"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let assert = run_args(&server, &["vault", "list"]).failure();

    // The failure is the JSON error envelope, never an empty array that reads
    // as "no vaults".
    let document: Value = serde_json::from_slice(&assert.get_output().stdout)
        .unwrap_or_else(|_| serde_json::from_str(stderr_text(&assert).trim()).unwrap());
    assert!(document.is_object(), "{document}");
    assert_eq!(document["exit_code"], 12, "{document}");
    assert_eq!(document["data"]["http_status"], 404, "{document}");
}

fn followers(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| json!({"user": format!("0x{index:040x}"), "vaultEquity": "1.0", "pnl": "0.0", "allTimePnl": "0.0", "daysFollowing": 1, "vaultEntryTime": 1, "lockupUntil": 2}))
        .collect()
}

fn vault_details(followers: Vec<Value>, follower_state: Value) -> Value {
    json!({
        "name": "HLP",
        "vaultAddress": VAULT,
        "leader": USER,
        "description": "d",
        "portfolio": [],
        "apr": 0.25,
        "followerState": follower_state,
        "leaderFraction": 0.0,
        "leaderCommission": 0.0,
        "followers": followers,
        "maxDistributable": 1000.0,
        "maxWithdrawable": 0.0,
        "isClosed": false,
        "allowDeposits": true
    })
}

#[tokio::test]
async fn vault_get_says_the_follower_list_is_capped_at_the_api_limit() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "vaultDetails", "vaultAddress": VAULT}),
            vault_details(followers(100), Value::Null),
        )
        .await;

    let vault = json_of(&server, &["vault", "get", VAULT]);

    assert_eq!(vault["followers_returned"], 100);
    assert_eq!(vault["followers_truncated"], true);
    assert!(vault.get("followers").is_none());
    assert!(
        vault.get("follower_state").is_none(),
        "absent without --user"
    );
}

#[tokio::test]
async fn vault_get_with_a_short_follower_list_is_not_truncated() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "vaultDetails", "vaultAddress": VAULT}),
            vault_details(followers(3), Value::Null),
        )
        .await;

    let vault = json_of(&server, &["vault", "get", VAULT]);

    assert_eq!(vault["followers_returned"], 3);
    assert_eq!(vault["followers_truncated"], false);
}

#[tokio::test]
async fn vault_get_with_a_user_sends_it_and_shows_their_stake() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "vaultDetails", "vaultAddress": VAULT, "user": USER}),
            vault_details(
                followers(1),
                json!({"user": USER, "vaultEquity": "316235.93", "pnl": "66235.9", "allTimePnl": "73688.7", "daysFollowing": 688, "vaultEntryTime": 1734786976034_u64, "lockupUntil": 1747658825285_u64}),
            ),
        )
        .await;

    let vault = json_of(&server, &["vault", "get", VAULT, "--user", USER]);

    assert_eq!(
        vault["follower_state"],
        json!({
            "vault_equity": "316235.93",
            "pnl": "66235.9",
            "all_time_pnl": "73688.7",
            "days_following": 688,
            "vault_entry_time": 1734786976034_u64,
            "lockup_until": 1747658825285_u64
        })
    );
}

#[tokio::test]
async fn vault_get_resolves_a_stored_account_alias_for_its_user() {
    let env = IsolatedHome::new();
    let address = support::expected_address(support::VALID_PRIVATE_KEY);
    for db in env.accounts_db_candidates() {
        let key_file = db.parent().unwrap().join("accounts.key");
        support::legacy_accounts_fixture::create_accounts_db_with_key_file(
            &db,
            &key_file,
            &[support::legacy_accounts_fixture::Row::new(
                "trader",
                &address,
                support::VALID_PRIVATE_KEY,
            )],
        );
    }
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "vaultDetails", "vaultAddress": VAULT, "user": address.to_lowercase()}),
            vault_details(followers(1), Value::Null),
        )
        .await;

    let mut command = env.account_command_without_passphrase();
    command.env(support::API_OVERRIDE_ENV, server.uri()).args([
        "--format", "json", "vault", "get", VAULT, "--user", "trader",
    ]);
    let vault = support::json_stdout(&command.assert().success());

    assert_eq!(vault["follower_state"], Value::Null);
    assert!(vault.get("follower_state").is_some());
}

#[tokio::test]
async fn vault_get_with_a_user_who_holds_nothing_shows_a_null_stake() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "vaultDetails", "vaultAddress": VAULT, "user": USER}),
            vault_details(followers(1), Value::Null),
        )
        .await;

    let vault = json_of(&server, &["vault", "get", VAULT, "--user", USER]);

    assert_eq!(vault["follower_state"], Value::Null);
}

// ---- status ----------------------------------------------------------------

#[tokio::test]
async fn status_keeps_its_constant_rate_limit_field_and_points_at_the_account_read() {
    let server = ExchangeFixture::start().await;

    let status = json_of(&server, &["status"]);

    assert_eq!(status["rate_limit_status"], "ok");
    assert!(
        status["rate_limit_note"]
            .as_str()
            .unwrap()
            .contains("hyperliquid account rate-limit"),
        "{status}"
    );
}

#[tokio::test]
async fn account_rate_limit_reads_the_request_budget() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "userRateLimit", "user": USER}),
            json!({"cumVlm": "2.0", "nRequestsUsed": 7, "nRequestsCap": 10000, "nRequestsSurplus": 0}),
        )
        .await;

    let limit = json_of(&server, &["account", "rate-limit", USER]);

    assert_eq!(limit["n_requests_used"], 7);
    assert_eq!(limit["n_requests_cap"], 10000);
    assert_eq!(limit["n_requests_surplus"], 0);
    assert_eq!(limit["cum_vlm"], "2.0");
}

#[tokio::test]
async fn portfolio_cumulative_funding_reports_each_horizon() {
    let server = MockServer::start().await;
    support::mount_override_healthcheck(&server).await;
    support::mount_account_state(&server, vec![], vec![], "9000", "100", 5).await;
    let mut state = support::fixture_clearinghouse_state("9000", 5);
    state["assetPositions"][0]["position"]["cumFunding"] =
        json!({"allTime": "-12.5", "sinceOpen": "3.25", "sinceChange": "0.5"});
    Mock::given(method("POST"))
        .and(path("/info"))
        .and(wiremock::matchers::body_partial_json(
            json!({"type": "clearinghouseState"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(state))
        .with_priority(1)
        .mount(&server)
        .await;

    let portfolio = json_of(&server, &["account", "portfolio", USER]);

    assert_eq!(
        portfolio["positions"][0]["cum_funding"],
        json!({"all_time": "-12.5", "since_open": "3.25", "since_change": "0.5"})
    );
}

#[tokio::test]
async fn spot_get_matches_a_pair_to_its_context_by_the_at_index_key() {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), spot_meta_with_hype())
        .await;
    server
        .mount_info(
            json!({"type": "spotMetaAndAssetCtxs"}),
            json!([
                {"universe": [], "tokens": []},
                [
                    {"coin": "PURR/USDC", "markPx": "0.1", "midPx": "0.1"},
                    {"coin": "@107", "markPx": "73.724", "midPx": "73.7235", "dayNtlVlm": "5.5"}
                ]
            ]),
        )
        .await;

    let pair = json_of(&server, &["spot", "get", "HYPE/USDC"]);

    assert_eq!(pair["mark_price"], "73.724");
    assert_eq!(pair["day_notional_volume"], "5.5");
}

#[tokio::test]
async fn account_orders_lists_trigger_fields_and_children_like_orders_open() {
    let server = ExchangeFixture::start().await;
    let mut trigger = support::fixture_basic_order("BTC", 11);
    trigger["isTrigger"] = json!(true);
    trigger["triggerPx"] = json!("48000.0");
    trigger["triggerCondition"] = json!("Price below 48000");
    trigger["isPositionTpsl"] = json!(true);
    trigger["orderType"] = json!("Stop Market");
    trigger["children"] = json!([]);
    server
        .mount_info(
            json!({"type": "frontendOpenOrders", "user": USER}),
            json!([trigger]),
        )
        .await;

    let rows = json_of(&server, &["account", "orders", USER]);

    assert_eq!(rows[0]["is_trigger"], true);
    assert_eq!(rows[0]["trigger_price"], "48000.0");
    assert_eq!(rows[0]["trigger_condition"], "Price below 48000");
    assert_eq!(rows[0]["is_position_tpsl"], true);
}
