//! What the exchange's refusals exit with and say: the documented strings each
//! have their own reason and hint, and a rejection inside an order status reads
//! the same as a top-level `status: err`.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, order_reply};
use support::orders::{words, write_orders_file};
use support::{IsolatedHome, json_stdout, wire};

const CUMULATIVE_LIMIT: &str = "Too many cumulative requests sent (37986 > 10436) for cumulative volume traded $437.92. Place taker orders to free up 1 request per USDC traded.";
const UNREGISTERED_SIGNER: &str =
    "L1 error: User or API Wallet 0x1111111111111111111111111111111111111111 does not exist.";

/// The JSON error document and exit code of `orders cancel 12345` whose
/// `/exchange` answer is `reply`.
async fn cancel_with(reply: Value) -> (i32, Value) {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(
            json!({"type": "orderStatus"}),
            wire::order_status_reply(wire::basic_order("BTC", 12345)),
        )
        .await;
    server.mount_exchange(reply).await;

    let assert = env
        .live_json(&server)
        .args(["orders", "cancel", "12345"])
        .assert();
    let code = assert.get_output().status.code().expect("exit code");
    assert_eq!(
        server.exchange_actions().await.len(),
        1,
        "a rejection is never resent"
    );
    (code, json_stdout(&assert))
}

fn in_order_status(text: &str) -> Value {
    json!({"status": "ok", "response": {"type": "cancel", "data": {"statuses": [{"error": text}]}}})
}

fn top_level(text: &str) -> Value {
    json!({"status": "err", "response": text})
}

/// (text, exit code, category, reason, a fragment of `data.hint`)
const CASES: &[(&str, i32, &str, &str, &str)] = &[
    (
        "Order must have minimum value of $10.",
        13,
        "unsupported",
        "order_below_minimum_value",
        "at least $10",
    ),
    (
        "Reduce only order would increase position.",
        13,
        "unsupported",
        "reduce_only_violation",
        "positions list",
    ),
    (
        "Post only order would have immediately matched, bbo was 50000@50001.",
        13,
        "unsupported",
        "post_only_would_cross",
        "--tif gtc",
    ),
    (
        "Price must be divisible by tick size.",
        13,
        "unsupported",
        "bad_price",
        "5 significant figures",
    ),
    (
        "Order price too far from oracle",
        13,
        "unsupported",
        "price_out_of_band",
        "oracle or reference",
    ),
    (
        "Order would increase open interest while open interest is capped",
        13,
        "unsupported",
        "open_interest_cap",
        "open-interest cap",
    ),
    (
        CUMULATIVE_LIMIT,
        11,
        "rate_limited",
        "rate_limited",
        "reserveRequestWeight",
    ),
    (
        UNREGISTERED_SIGNER,
        10,
        "auth",
        "signer_not_registered",
        "hyperliquid api-wallet list",
    ),
];

#[tokio::test]
async fn a_rejection_in_an_order_status_and_a_top_level_error_exit_and_report_the_same() {
    for (text, exit_code, category, reason, hint) in CASES {
        for (path, reply) in [
            ("order status", in_order_status(text)),
            ("top-level", top_level(text)),
        ] {
            let (code, document) = cancel_with(reply).await;
            assert_eq!(code, *exit_code, "{path}: {text}");
            assert_eq!(document["exit_code"], *exit_code, "{path}: {text}");
            assert_eq!(document["category"], *category, "{path}: {text}");
            assert_eq!(document["reason"], *reason, "{path}: {text}");
            let data_hint = document["data"]["hint"].as_str().unwrap_or_default();
            assert!(data_hint.contains(hint), "{path}: {text}: {document}");
            assert!(
                document["error"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("[untrusted remote data] {text}")),
                "{path}: {text}: {document}"
            );
        }
    }
}

#[tokio::test]
async fn an_address_rate_limit_says_how_long_to_wait_and_never_claims_a_resend_is_safe() {
    for reply in [
        in_order_status(CUMULATIVE_LIMIT),
        top_level(CUMULATIVE_LIMIT),
    ] {
        let (code, document) = cancel_with(reply).await;
        assert_eq!(code, 11);
        assert_eq!(document["retry_after"], 10);
        assert!(document.get("retryable").is_none(), "{document}");
    }
}

#[tokio::test]
async fn text_nothing_recognises_keeps_each_paths_established_exit_code() {
    // In an order status it is "unsupported input" (13); at the top level it
    // is the generic `exchange_rejected` (1). Recognised text never differs.
    let (code, document) = cancel_with(in_order_status("Something the exchange invented.")).await;
    assert_eq!(code, 13);
    assert_eq!(document["reason"], "unsupported_input");
    assert!(document["data"].get("hint").is_none(), "{document}");

    let (code, document) = cancel_with(top_level("Something the exchange invented.")).await;
    assert_eq!(code, 1);
    assert_eq!(document["reason"], "exchange_rejected");
    assert!(document.get("data").is_none(), "{document}");
}

const TPSL: &str =
    "orders tpsl --coin BTC --side sell --size 0.1 --take-profit 6100 --stop-loss 4900";

async fn tpsl_with(statuses: Vec<Value>) -> (i32, Value) {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server.mount_exchange(order_reply(statuses)).await;
    let assert = env.live_json(&server).args(words(TPSL)).assert();
    (
        assert.get_output().status.code().expect("exit code"),
        json_stdout(&assert),
    )
}

#[tokio::test]
async fn a_batch_rejected_whole_for_one_reason_is_reported_once() {
    let text = "Order must have minimum value of $10.";
    let (code, document) = tpsl_with(vec![json!({"error": text}), json!({"error": text})]).await;

    assert_eq!(code, 13);
    assert_eq!(document["reason"], "order_below_minimum_value");
    assert_eq!(
        document["error"],
        "Unsupported input: Couldn't place TP/SL orders. Exchange said: [untrusted remote data] Order must have minimum value of $10."
    );
}

#[tokio::test]
async fn a_batch_whose_legs_differ_lists_each_leg_with_its_number() {
    let (code, document) = tpsl_with(vec![
        json!({"error": "Order has zero size."}),
        json!({"error": "Invalid TP/SL price."}),
    ])
    .await;

    assert_eq!(code, 13);
    assert_eq!(document["reason"], "unsupported_input");
    assert_eq!(
        document["error"],
        "Unsupported input: Couldn't place TP/SL orders. Exchange said:\n  1. [untrusted remote data] Order has zero size.\n  2. [untrusted remote data] Invalid TP/SL price."
    );
}

const RATE_LIMIT: &str = "Too many requests: rate limit exceeded";
const CREDENTIALS: &str = "Unauthorized: invalid key";

/// Every command that reads an exchange status gives a rate limit exit 11 and
/// a refused credential exit 10, and any other rejection neither. `prepare`
/// mounts what the command reads before it sends; `reply` is the `/exchange`
/// answer carrying the rejection `text`.
async fn assert_rejection_exit_codes(
    line: &str,
    prepare: impl AsyncFn(&IsolatedHome, &ExchangeFixture),
    reply: impl Fn(&str) -> Value,
) {
    for (text, expected, category) in [
        (RATE_LIMIT, Some(11), "rate_limited"),
        (CREDENTIALS, Some(10), "auth"),
        ("Order has invalid size.", None, ""),
    ] {
        let env = IsolatedHome::new();
        let server = ExchangeFixture::start().await;
        prepare(&env, &server).await;
        server.mount_exchange(reply(text)).await;

        let assert = env
            .live_json(&server)
            .args(words(line))
            .current_dir(env.tmp_path())
            .assert();
        let code = assert.get_output().status.code().unwrap();
        match expected {
            Some(expected) => {
                assert_eq!(code, expected, "{line} on {text:?}");
                assert_eq!(json_stdout(&assert)["category"], category, "{line}");
            }
            None => assert!(
                code != 10 && code != 11 && code != 0,
                "{line} on {text:?} exited {code}"
            ),
        }
        assert_eq!(
            server.exchange_actions().await.len(),
            1,
            "{line} sends one action and does not retry a rejection"
        );
    }
}

async fn nothing_to_prepare(_: &IsolatedHome, _: &ExchangeFixture) {}

async fn with_open_order(_: &IsolatedHome, server: &ExchangeFixture) {
    server
        .mount_info_containing(
            json!({"type": "orderStatus"}),
            wire::order_status_reply(wire::basic_order("BTC", 12345)),
        )
        .await;
}

fn one_rejected_order(text: &str) -> Value {
    order_reply(vec![json!({"error": text})])
}

#[tokio::test]
async fn create_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        "orders create --coin BTC --side buy --price 50000 --size 0.01",
        nothing_to_prepare,
        one_rejected_order,
    )
    .await;
}

#[tokio::test]
async fn batch_create_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        "orders batch-create --orders-file orders.json",
        async |env, _| {
            write_orders_file(
                env,
                "orders.json",
                &json!({"orders": [
                    {"coin": "BTC", "side": "buy", "price": "50000", "size": "0.001"},
                    {"coin": "ETH", "side": "buy", "price": "3000", "size": "0.01"},
                ]}),
            );
        },
        |text| order_reply(vec![json!({"error": text}), json!({"error": text})]),
    )
    .await;
}

#[tokio::test]
async fn cancel_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes("orders cancel 12345", with_open_order, in_order_status).await;
}

#[tokio::test]
async fn modify_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        "orders modify 12345 --price 51000 --size 0.05",
        with_open_order,
        one_rejected_order,
    )
    .await;
}

#[tokio::test]
async fn twap_create_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        "orders twap-create --coin BTC --side buy --size 1.0 --duration 3600",
        nothing_to_prepare,
        |text| json!({"status": "ok", "response": {"type": "twapOrder", "data": {"status": {"error": text}}}}),
    )
    .await;
}

#[tokio::test]
async fn tpsl_status_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(TPSL, nothing_to_prepare, |text| {
        order_reply(vec![json!({"error": text}), json!({"error": text})])
    })
    .await;
}

#[tokio::test]
async fn trailing_stop_rejections_keep_their_exit_codes() {
    assert_rejection_exit_codes(
        "orders trailing-stop --coin BTC --side sell --size 0.01 --retracement-pct 5",
        nothing_to_prepare,
        top_level,
    )
    .await;
}

#[tokio::test]
async fn a_chase_quote_rejection_keeps_its_exit_code_and_its_wording() {
    let line = "orders chase --coin BTC --side buy --size 0.5 --timeout 10s";
    assert_rejection_exit_codes(line, nothing_to_prepare, one_rejected_order).await;

    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_exchange(one_rejected_order("Order has invalid size."))
        .await;
    let assert = env.live_json(&server).args(words(line)).assert().code(13);
    assert_eq!(
        json_stdout(&assert)["error"],
        "Unsupported input: chase quote rejected: [untrusted remote data] Order has invalid size."
    );
}

#[tokio::test]
async fn a_bracket_entry_rejection_keeps_its_exit_code() {
    assert_rejection_exit_codes(
        "orders bracket --coin BTC --side buy --entry limit --price 50000 --size 0.01 --take-profit +10% --stop-loss -5%",
        nothing_to_prepare,
        one_rejected_order,
    )
    .await;
}
