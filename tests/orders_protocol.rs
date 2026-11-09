//! Order commands against the exchange protocol: the trigger fields and status
//! the listings carry, the zero cloid, the builder approval check, order write
//! priority, sub-builders and `orders batch-modify`.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, order_reply};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout, wire};

const BUILDER: &str = "0x00000000000000000000000000000000000000bb";
const SUB_A: &str = "0x00000000000000000000000000000000000000cc";
const SUB_B: &str = "0x00000000000000000000000000000000000000dd";

fn master() -> String {
    expected_address(VALID_PRIVATE_KEY).to_lowercase()
}

/// A stop order as `frontendOpenOrders` lists it on mainnet.
fn stop_order(oid: u64, children: Vec<Value>) -> Value {
    json!({
        "coin": "ETH", "side": "B", "limitPx": "2370.5", "sz": "1000.0", "oid": oid,
        "timestamp": 1794219546695_u64, "triggerCondition": "Price above 2155",
        "isTrigger": true, "triggerPx": "2155.0", "children": children,
        "isPositionTpsl": false, "reduceOnly": false, "orderType": "Stop Market",
        "origSz": "1000.0", "tif": null, "cloid": null
    })
}

/// A resting limit order as `frontendOpenOrders` lists it on mainnet: the
/// exchange's placeholders stand where a trigger would be.
fn limit_order(oid: u64) -> Value {
    json!({
        "coin": "ETH", "side": "B", "limitPx": "2015.0", "sz": "2500.0", "oid": oid,
        "timestamp": 1794211529144_u64, "triggerCondition": "N/A", "isTrigger": false,
        "triggerPx": "0.0", "children": [], "isPositionTpsl": false, "reduceOnly": false,
        "orderType": "Limit", "origSz": "2500.0", "tif": "Gtc", "cloid": null
    })
}

// ---- order listings ----

#[tokio::test]
async fn open_orders_carry_their_trigger_fields_and_children() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let child = json!({
        "coin": "ETH", "side": "A", "limitPx": "1900.0", "sz": "1000.0", "oid": 22,
        "timestamp": 1794219546695_u64, "triggerCondition": "Price below 1900",
        "isTrigger": true, "triggerPx": "1900.0", "children": [], "isPositionTpsl": true,
        "reduceOnly": true, "orderType": "Stop Market", "origSz": "1000.0", "tif": null,
        "cloid": null
    });
    server
        .mount_info_containing(
            json!({"type": "frontendOpenOrders"}),
            json!([stop_order(21, vec![child]), limit_order(23)]),
        )
        .await;

    let assert = env
        .live_json(&server)
        .args(["orders", "open"])
        .assert()
        .success();
    let rows = json_stdout(&assert);

    assert_eq!(rows[0]["oid"], 21);
    assert_eq!(rows[0]["is_trigger"], true);
    assert_eq!(rows[0]["trigger_price"], "2155.0");
    assert_eq!(rows[0]["trigger_condition"], "Price above 2155");
    assert_eq!(rows[0]["is_position_tpsl"], false);
    assert_eq!(rows[0]["reduce_only"], false);
    let children = rows[0]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["oid"], 22);
    assert_eq!(children[0]["trigger_price"], "1900.0");
    assert_eq!(children[0]["is_position_tpsl"], true);
    assert_eq!(children[0]["reduce_only"], true);
}

#[tokio::test]
async fn an_order_that_is_not_a_trigger_reads_no_trigger_price_or_condition() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "frontendOpenOrders"}),
            json!([limit_order(23)]),
        )
        .await;

    let assert = env
        .live_json(&server)
        .args(["orders", "open"])
        .assert()
        .success();
    let row = &json_stdout(&assert)[0];

    assert_eq!(row["is_trigger"], false);
    assert_eq!(row["trigger_price"], Value::Null);
    assert_eq!(row["trigger_condition"], Value::Null);
    assert!(row.get("children").is_none(), "no children, no key: {row}");
}

#[tokio::test]
async fn orders_open_table_has_trigger_and_children_columns() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "frontendOpenOrders"}),
            json!([stop_order(21, vec![limit_order(24)])]),
        )
        .await;

    let output = env
        .live_command(&server)
        .args(["--format", "table", "--testnet", "orders", "open"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();

    let header = text.lines().next().unwrap();
    assert!(header.contains("Trigger Price"), "{text}");
    assert!(header.contains("Children"), "{text}");
    assert!(text.contains("Price above 2155"), "{text}");
    assert!(text.contains("2155.0"), "{text}");
    assert!(text.contains("24"), "{text}");
}

#[tokio::test]
async fn order_history_rows_carry_status_and_status_time() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "historicalOrders"}),
            json!([
                {"order": stop_order(31, vec![]), "status": "triggered", "statusTimestamp": 1794219546700_u64},
                {"order": limit_order(32), "status": "marginCanceled", "statusTimestamp": 1794219546800_u64},
            ]),
        )
        .await;

    let assert = env
        .live_json(&server)
        .args(["orders", "history"])
        .assert()
        .success();
    let rows = json_stdout(&assert);

    assert_eq!(rows[0]["oid"], 31);
    assert_eq!(rows[0]["status"], "triggered");
    assert_eq!(rows[0]["status_timestamp"], 1794219546700_u64);
    assert_eq!(rows[0]["trigger_price"], "2155.0");
    assert_eq!(rows[1]["status"], "marginCanceled");
    assert_eq!(rows[1]["status_timestamp"], 1794219546800_u64);
}

#[tokio::test]
async fn order_history_raw_fallback_has_no_status_to_report() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "historicalOrders"}),
            json!([wire::basic_order("BTC", 5)]),
        )
        .await;

    let assert = env
        .live_json(&server)
        .args(["orders", "history"])
        .assert()
        .success();
    let row = &json_stdout(&assert)[0];

    assert_eq!(row["oid"], 5);
    assert!(row.get("status").is_none(), "{row}");
}

#[tokio::test]
async fn order_history_status_comes_from_the_raw_list_when_the_sdk_shape_fails() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    // A status the SDK has no variant for fails its decode; the raw read keeps it.
    server
        .mount_info_containing(
            json!({"type": "historicalOrders"}),
            json!([{"order": limit_order(33), "status": "someNewStatus", "statusTimestamp": 7}]),
        )
        .await;

    let assert = env
        .live_json(&server)
        .args(["orders", "history"])
        .assert()
        .success();
    let row = &json_stdout(&assert)[0];

    assert_eq!(row["status"], "someNewStatus");
    assert_eq!(row["status_timestamp"], 7);
}

// ---- cloid ----

#[tokio::test]
async fn a_zero_cloid_is_refused_before_anything_is_sent() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;

    let assert = env
        .live_json(&server)
        .args([
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--price",
            "50000",
            "--size",
            "0.1",
            "--cloid",
            "0x00000000000000000000000000000000",
        ])
        .assert()
        .code(2);

    assert!(
        String::from_utf8_lossy(&assert.get_output().stdout).contains("means 'no cloid'"),
        "{}",
        String::from_utf8_lossy(&assert.get_output().stdout)
    );
    assert!(server.exchange_actions().await.is_empty());
}

// ---- builder approval ----

fn create_with_builder<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "orders",
        "create",
        "--coin",
        "BTC",
        "--side",
        "buy",
        "--price",
        "50000",
        "--size",
        "0.1",
        "--builder",
        BUILDER,
        "--builder-fee-rate",
        "0.01%",
    ];
    args.extend_from_slice(extra);
    args
}

async fn approved_max_fee(server: &ExchangeFixture, tenths_bps: u64) {
    server
        .mount_info(
            json!({"type": "maxBuilderFee", "user": master(), "builder": BUILDER}),
            json!(tenths_bps),
        )
        .await;
}

#[tokio::test]
async fn a_builder_fee_above_the_approved_maximum_fails_before_the_order_is_sent() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    approved_max_fee(&server, 5).await;

    let assert = env
        .live_json(&server)
        .args(create_with_builder(&[]))
        .assert()
        .code(13);

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains(&format!(
            "hyperliquid builder approve --builder {BUILDER} --max-fee-rate 0.01%"
        )),
        "{stdout}"
    );
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn a_builder_fee_within_the_approved_maximum_is_sent() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    approved_max_fee(&server, 10).await;

    env.live_json(&server)
        .args(create_with_builder(&[]))
        .assert()
        .success();

    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["builder"], json!({"b": BUILDER, "f": 10}));
    assert_eq!(
        server.info_requests("maxBuilderFee").await,
        [json!({"type": "maxBuilderFee", "user": master(), "builder": BUILDER})]
    );
}

#[tokio::test]
async fn an_unreadable_approval_does_not_stop_the_order() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    // No maxBuilderFee answer is mounted: the read fails.

    env.live_json(&server)
        .args(create_with_builder(&[]))
        .assert()
        .success();

    assert_eq!(server.exchange_actions().await.len(), 1);
}

#[tokio::test]
async fn the_default_builder_is_checked_like_an_explicit_one() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    approved_max_fee(&server, 5).await;

    env.live_json(&server)
        .args([
            "orders", "create", "--coin", "BTC", "--side", "buy", "--price", "50000", "--size",
            "0.1",
        ])
        .env(hyperliquid_cli::env::DEFAULT_BUILDER_ADDRESS, BUILDER)
        .env(hyperliquid_cli::env::DEFAULT_BUILDER_FEE_RATE, "0.01%")
        .assert()
        .code(13);

    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn an_order_for_a_vault_is_not_checked_against_the_signers_approval() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    approved_max_fee(&server, 0).await;

    env.live_json(&server)
        .args(create_with_builder(&["--on-behalf-of", SUB_A]))
        .assert()
        .success();

    assert!(server.info_requests("maxBuilderFee").await.is_empty());
    assert_eq!(server.exchange_actions().await.len(), 1);
}

#[tokio::test]
async fn the_dry_run_reports_the_approval_and_survives_an_unreadable_one() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    approved_max_fee(&server, 5).await;

    let assert = env
        .live_json(&server)
        .args(create_with_builder(&["--dry-run"]))
        .assert()
        .success();
    let approval = &json_stdout(&assert)["args"]["builder_approval"];
    assert_eq!(approval["status"], "insufficient");
    assert_eq!(approval["approved_tenths_bps"], 5);
    assert_eq!(approval["requested_tenths_bps"], 10);

    let unreadable = ExchangeFixture::start().await;
    let assert = env
        .live_json(&unreadable)
        .args(create_with_builder(&["--dry-run"]))
        .assert()
        .success();
    assert_eq!(
        json_stdout(&assert)["args"]["builder_approval"]["status"],
        "unknown"
    );
}

// ---- sub-builders ----

#[tokio::test]
async fn sub_builders_ride_in_the_builder_object() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    approved_max_fee(&server, 10).await;

    env.live_json(&server)
        .args(create_with_builder(&[
            "--sub-builder",
            &format!("{SUB_A}:25%"),
            "--sub-builder",
            &format!("{SUB_B}:12.5%"),
        ]))
        .assert()
        .success();

    let actions = server.exchange_actions().await;
    assert_eq!(
        actions[0]["builder"],
        json!({
            "b": BUILDER,
            "f": 10,
            "s": [{"b": SUB_A, "s": 25000}, {"b": SUB_B, "s": 12500}]
        })
    );
}

#[tokio::test]
async fn sub_builder_limits_are_enforced_before_anything_is_sent() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    approved_max_fee(&server, 10).await;

    let six: Vec<String> = (0..6)
        .map(|index| format!("{SUB_A}:{}%", index + 1))
        .collect();
    let mut too_many: Vec<&str> = Vec::new();
    for entry in &six {
        too_many.extend(["--sub-builder", entry]);
    }
    let over = [
        ("0xnot-an-address:10%", 13),
        (&format!("{SUB_A}:10"), 2),
        (&format!("{SUB_A}:0%"), 2),
        (&format!("{SUB_A}:0.0001%"), 2),
        (&format!("{SUB_A}:100.001%"), 2),
    ];
    for (entry, code) in over {
        env.live_json(&server)
            .args(create_with_builder(&["--sub-builder", entry]))
            .assert()
            .code(code);
    }
    env.live_json(&server)
        .args(create_with_builder(&too_many))
        .assert()
        .code(2);
    env.live_json(&server)
        .args(create_with_builder(&[
            "--sub-builder",
            &format!("{SUB_A}:60%"),
            "--sub-builder",
            &format!("{SUB_B}:40.001%"),
        ]))
        .assert()
        .code(2);

    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn a_sub_builder_needs_a_builder() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    env.live_json(&server)
        .args([
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--price",
            "50000",
            "--size",
            "0.1",
            "--sub-builder",
            &format!("{SUB_A}:10%"),
        ])
        .assert()
        .code(2);
}

// ---- order write priority ----

fn ioc_order<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "orders", "create", "--coin", "BTC", "--side", "buy", "--price", "50000", "--size", "0.1",
        "--tif", "ioc",
    ];
    args.extend_from_slice(extra);
    args
}

#[tokio::test]
async fn an_ioc_order_with_a_priority_rate_sends_the_priority_grouping() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;

    env.live_json(&server)
        .args(ioc_order(&["--priority-rate", "80000"]))
        .assert()
        .success();

    assert_eq!(
        server.exchange_actions().await,
        [json!({
            "type": "order",
            "orders": [{
                "a": 0, "b": true, "p": "50000", "s": "0.1", "r": false,
                "t": {"limit": {"tif": "Ioc"}}
            }],
            "grouping": {"p": 80000}
        })]
    );
}

#[tokio::test]
async fn a_post_only_order_may_pay_priority_unless_it_is_reduce_only() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    fn alo<'a>(extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = ioc_order(&[]);
        args.truncate(args.len() - 1);
        args.push("alo");
        args.extend_from_slice(extra);
        args
    }

    env.live_json(&server)
        .args(alo(&["--priority-rate", "1"]))
        .assert()
        .success();
    assert_eq!(
        server.exchange_actions().await[0]["grouping"],
        json!({"p": 1})
    );

    env.live_json(&server)
        .args(alo(&["--priority-rate", "1", "--reduce-only"]))
        .assert()
        .code(13);
    assert_eq!(server.exchange_actions().await.len(), 1);
}

#[tokio::test]
async fn priority_is_refused_for_orders_the_exchange_does_not_accept_it_for() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;

    // A resting GTC order, a market order and a trigger order are neither IOC nor ALO.
    for args in [
        vec![
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--price",
            "50000",
            "--size",
            "0.1",
            "--priority-rate",
            "5",
        ],
        vec![
            "buy",
            "--coin",
            "BTC",
            "--size",
            "0.1",
            "--priority-rate",
            "5",
        ],
        vec![
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "sell",
            "--type",
            "stop-loss",
            "--trigger-price",
            "40000",
            "--size",
            "0.1",
            "--priority-rate",
            "5",
        ],
    ] {
        env.live_json(&server).args(&args).assert().code(13);
    }
    // Outcome markets never take priority.
    env.live_json(&server)
        .args([
            "outcome-buy",
            "--coin",
            "#10",
            "--price",
            "0.5",
            "--size",
            "1",
            "--tif",
            "ioc",
            "--priority-rate",
            "5",
        ])
        .assert()
        .code(13);
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn priority_cannot_be_combined_with_a_tpsl_grouping_or_exceed_a_hundred_percent() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    env.live_json(&server)
        .args(ioc_order(&[
            "--priority-rate",
            "5",
            "--take-profit",
            "60000",
            "--grouping",
            "normal-tpsl",
        ]))
        .assert()
        .code(2);
    env.live_json(&server)
        .args(ioc_order(&["--priority-rate", "100000001"]))
        .assert()
        .code(2);
    env.live_json(&server)
        .args(ioc_order(&["--priority-rate", "0"]))
        .assert()
        .code(2);
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn a_batch_pays_one_priority_for_every_leg_and_refuses_a_mixed_batch() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1), wire::resting_status(2)])
        .await;

    env.live_json(&server)
        .args([
            "orders",
            "scale",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--start-price",
            "49000",
            "--end-price",
            "50000",
            "--total-size",
            "0.2",
            "--orders",
            "2",
            "--tif",
            "ioc",
            "--priority-rate",
            "80000",
        ])
        .assert()
        .success();
    assert_eq!(
        server.exchange_actions().await[0]["grouping"],
        json!({"p": 80000})
    );

    let mixed = env.tmp_path().join("mixed.json");
    std::fs::write(
        &mixed,
        json!([
            {"coin": "BTC", "side": "buy", "price": "49000", "size": "0.1", "tif": "ioc"},
            {"coin": "BTC", "side": "buy", "price": "48000", "size": "0.1", "tif": "alo"}
        ])
        .to_string(),
    )
    .unwrap();
    env.live_json(&server)
        .args([
            "orders",
            "batch-create",
            "--orders-file",
            "mixed.json",
            "--priority-rate",
            "5",
        ])
        .current_dir(env.tmp_path())
        .assert()
        .code(13);
    assert_eq!(server.exchange_actions().await.len(), 1);

    let uniform = env.tmp_path().join("uniform.json");
    std::fs::write(
        &uniform,
        json!([
            {"coin": "BTC", "side": "buy", "price": "49000", "size": "0.1", "tif": "alo"},
            {"coin": "BTC", "side": "buy", "price": "48000", "size": "0.1", "tif": "alo"}
        ])
        .to_string(),
    )
    .unwrap();
    env.live_json(&server)
        .args([
            "orders",
            "batch-create",
            "--orders-file",
            "uniform.json",
            "--priority-rate",
            "5",
        ])
        .current_dir(env.tmp_path())
        .assert()
        .success();
    assert_eq!(
        server.exchange_actions().await[1]["grouping"],
        json!({"p": 5})
    );
}

#[tokio::test]
async fn priority_and_a_builder_share_one_action() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_order_statuses(vec![wire::resting_status(1)])
        .await;
    approved_max_fee(&server, 10).await;

    env.live_json(&server)
        .args(ioc_order(&[
            "--priority-rate",
            "80000",
            "--builder",
            BUILDER,
            "--builder-fee-rate",
            "0.01%",
        ]))
        .assert()
        .success();

    let action = &server.exchange_actions().await[0];
    assert_eq!(action["grouping"], json!({"p": 80000}));
    assert_eq!(action["builder"], json!({"b": BUILDER, "f": 10}));
}

#[tokio::test]
async fn the_dry_run_names_the_priority_grouping() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let assert = env
        .live_json(&server)
        .args(ioc_order(&["--priority-rate", "80000", "--dry-run"]))
        .assert()
        .success();
    let args = &json_stdout(&assert)["args"];

    assert_eq!(args["priority_rate"], 80000);
    assert_eq!(args["grouping"], "priority-rate");
    assert_eq!(args["grouping_wire"], json!({"p": 80000}));
}

// ---- orders batch-modify ----

async fn mount_open_order(server: &ExchangeFixture, oid: u64, cloid: Option<&str>) {
    let order = support::wire::basic_order_with_cloid("BTC", oid, cloid);
    let oid_or_cloid = cloid.map_or(json!(oid), |cloid| json!(cloid));
    server
        .mount_info(
            json!({"type": "orderStatus", "user": master(), "oid": oid_or_cloid}),
            wire::order_status_reply(order),
        )
        .await;
}

fn write_orders(env: &IsolatedHome, name: &str, orders: Value) {
    std::fs::write(env.tmp_path().join(name), orders.to_string()).unwrap();
}

#[tokio::test]
async fn batch_modify_sends_every_modification_in_one_batch_modify_action() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let cloid = "0x00000000000000000000000000000abc";
    mount_open_order(&server, 11, None).await;
    mount_open_order(&server, 12, Some(cloid)).await;
    server
        .mount_exchange(order_reply(vec![
            wire::resting_status(111),
            wire::resting_status(112),
        ]))
        .await;
    write_orders(
        &env,
        "mods.json",
        json!({"orders": [
            {"order_id": 11, "price": "49000.123456"},
            {"cloid": cloid, "size": "0.25"}
        ]}),
    );

    let assert = env
        .live_json(&server)
        .args(["orders", "batch-modify", "--orders-file", "mods.json"])
        .current_dir(env.tmp_path())
        .assert()
        .success();

    assert_eq!(
        server.exchange_actions().await,
        [json!({
            "type": "batchModify",
            "modifies": [
                {"oid": 11, "order": {
                    "a": 0, "b": true, "p": "49000", "s": "0.1", "r": false,
                    "t": {"limit": {"tif": "Gtc"}}
                }},
                {"oid": cloid, "order": {
                    "a": 0, "b": true, "p": "50000", "s": "0.25", "r": false,
                    "t": {"limit": {"tif": "Gtc"}}, "c": cloid
                }}
            ]
        })]
    );
    let rows = json_stdout(&assert);
    assert_eq!(rows[0]["order_id"], 111);
    assert_eq!(rows[0]["status"], "modified");
    assert_eq!(rows[0]["price"], "49000");
    assert_eq!(rows[1]["order_id"], 112);
    assert_eq!(rows[1]["cloid"], cloid);
}

#[tokio::test]
async fn batch_modify_keeps_a_trigger_orders_trigger_price_on_a_size_only_change() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let stop = json!({
        "timestamp": 1700000000000_u64, "coin": "BTC", "side": "A", "limitPx": "44000",
        "sz": "0.1", "oid": 15, "origSz": "0.1", "cloid": null, "orderType": "Stop Market",
        "tif": null, "reduceOnly": true, "isTrigger": true, "triggerPx": "45000",
        "triggerCondition": "Price below 45000", "isPositionTpsl": false
    });
    server
        .mount_info(
            json!({"type": "orderStatus", "user": master(), "oid": 15}),
            wire::order_status_reply(stop),
        )
        .await;
    server
        .mount_exchange(order_reply(vec![json!("waitingForTrigger")]))
        .await;
    write_orders(&env, "mods.json", json!([{"order_id": 15, "size": "0.05"}]));

    env.live_json(&server)
        .args([
            "orders",
            "batch-modify",
            "--orders-file",
            "mods.json",
            "--always-place",
        ])
        .current_dir(env.tmp_path())
        .assert()
        .success();

    let action = &server.exchange_actions().await[0];
    assert_eq!(action["a"], true);
    assert_eq!(
        action["modifies"][0]["order"]["t"],
        json!({"trigger": {"isMarket": true, "triggerPx": "45000", "tpsl": "sl"}})
    );
    assert_eq!(action["modifies"][0]["order"]["s"], "0.05");
}

#[tokio::test]
async fn batch_modify_reports_a_partial_failure_per_modification() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    mount_open_order(&server, 11, None).await;
    mount_open_order(&server, 12, None).await;
    server
        .mount_exchange(order_reply(vec![
            wire::resting_status(111),
            json!({"error": "Order has invalid size."}),
        ]))
        .await;
    write_orders(
        &env,
        "mods.json",
        json!([{"order_id": 11, "price": "49000"}, {"order_id": 12, "price": "48000"}]),
    );

    let assert = env
        .live_json(&server)
        .args(["orders", "batch-modify", "--orders-file", "mods.json"])
        .current_dir(env.tmp_path())
        .assert()
        .code(15);

    let document = json_stdout(&assert);
    assert_eq!(document["succeeded"], 1);
    assert_eq!(document["failed"], 1);
    assert_eq!(document["orders"][0]["status"], "modified");
    assert_eq!(document["orders"][1]["status"], "failed");
    assert!(
        document["orders"][1]["error"]
            .as_str()
            .unwrap()
            .contains("Order has invalid size."),
        "{document}"
    );
}

#[tokio::test]
async fn batch_modify_validates_the_file_before_reading_the_exchange() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(order_reply(vec![
            wire::resting_status(1),
            wire::resting_status(2),
        ]))
        .await;
    let run = |name: &str, orders: Value| {
        write_orders(&env, name, orders);
        env.live_json(&server)
            .args(["orders", "batch-modify", "--orders-file", name])
            .current_dir(env.tmp_path())
            .assert()
            .code(2)
            .get_output()
            .stdout
            .clone()
    };

    let text = String::from_utf8(run(
        "bad.json",
        json!([
            {"price": "1"},
            {"order_id": 1},
            {"order_id": 2, "cloid": "0x00000000000000000000000000000abc", "price": "1"},
            {"order_id": 3, "price": "-1"},
            {"cloid": "0xabc", "price": "1"},
        ]),
    ))
    .unwrap();
    for expected in [
        "orders[0].order_id or cloid is required",
        "orders[1] needs price, trigger_price, or size to replace",
        "orders[2] names the order with order_id or cloid, not both",
        "orders[3].price must be greater than zero",
        "orders[4].cloid",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    let text = String::from_utf8(run(
        "dup.json",
        json!([{"order_id": 4, "price": "1"}, {"order_id": 4, "size": "1"}]),
    ))
    .unwrap();
    assert!(
        text.contains("orders[1] modifies the same order as orders[0]"),
        "{text}"
    );
    let text = String::from_utf8(run("empty.json", json!([]))).unwrap();
    assert!(text.contains("at least one order"), "{text}");

    assert!(server.exchange_actions().await.is_empty());
    assert!(server.info_requests("orderStatus").await.is_empty());
}

#[tokio::test]
async fn batch_modify_names_the_modification_a_planning_error_belongs_to() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    let stop = json!({
        "timestamp": 1700000000000_u64, "coin": "BTC", "side": "A", "limitPx": "44000",
        "sz": "0.1", "oid": 15, "origSz": "0.1", "cloid": null, "orderType": "Stop Limit",
        "tif": null, "reduceOnly": true, "isTrigger": true, "triggerPx": "45000",
        "triggerCondition": "Price below 45000", "isPositionTpsl": false
    });
    mount_open_order(&server, 11, None).await;
    server
        .mount_info(
            json!({"type": "orderStatus", "user": master(), "oid": 15}),
            wire::order_status_reply(stop),
        )
        .await;
    server
        .mount_exchange(order_reply(vec![wire::resting_status(1)]))
        .await;
    write_orders(
        &env,
        "mods.json",
        json!([{"order_id": 11, "price": "49000"}, {"order_id": 15, "size": "0.05"}]),
    );

    let assert = env
        .live_json(&server)
        .args([
            "orders",
            "batch-modify",
            "--orders-file",
            "mods.json",
            "--always-place",
        ])
        .current_dir(env.tmp_path())
        .assert()
        .code(2);

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(stdout.contains("orders[1]: "), "{stdout}");
    assert!(stdout.contains("--trigger-price"), "{stdout}");
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn batch_modify_dry_run_previews_without_reading_orders() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    write_orders(
        &env,
        "mods.json",
        json!([{"order_id": 11, "price": "49000"}, {"cloid": "0x00000000000000000000000000000abc", "size": "2"}]),
    );

    let assert = env
        .live_json(&server)
        .args([
            "--dry-run",
            "orders",
            "batch-modify",
            "--orders-file",
            "mods.json",
        ])
        .current_dir(env.tmp_path())
        .assert()
        .success();

    let preview = json_stdout(&assert);
    assert_eq!(preview["command"], "orders batch-modify");
    assert_eq!(preview["would_execute"], "modify_orders");
    assert_eq!(preview["args"]["order_count"], 2);
    assert_eq!(preview["args"]["modifications"][0]["order_id"], 11);
    assert_eq!(preview["args"]["modifications"][0]["price"], "49000");
    assert_eq!(
        preview["args"]["modifications"][1]["cloid"],
        "0x00000000000000000000000000000abc"
    );
    assert!(server.info_requests("orderStatus").await.is_empty());
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn batch_modify_on_mainnet_needs_approval_unless_yes() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    mount_open_order(&server, 11, None).await;
    server
        .mount_exchange(order_reply(vec![wire::resting_status(111)]))
        .await;
    write_orders(
        &env,
        "mods.json",
        json!([{"order_id": 11, "price": "49000"}]),
    );

    let mut command = env.live_command(&server);
    command
        .args([
            "--format",
            "json",
            "orders",
            "batch-modify",
            "--orders-file",
            "mods.json",
        ])
        .current_dir(env.tmp_path());
    // A run that cannot prompt (JSON output) stops at the confirmation.
    command.assert().failure();
    assert!(server.exchange_actions().await.is_empty());

    let mut command = env.live_command(&server);
    command
        .args([
            "--format",
            "json",
            "orders",
            "batch-modify",
            "--orders-file",
            "mods.json",
            "--yes",
        ])
        .current_dir(env.tmp_path());
    command.assert().success();
    assert_eq!(server.exchange_actions().await.len(), 1);
}
