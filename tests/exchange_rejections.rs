//! What the exchange's refusals exit with and say: the documented strings each
//! have their own reason and hint, and a rejection inside an order status reads
//! the same as a top-level `status: err`.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, order_reply};
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

const TPSL: &[&str] = &[
    "orders",
    "tpsl",
    "--coin",
    "BTC",
    "--side",
    "sell",
    "--size",
    "0.1",
    "--take-profit",
    "6100",
    "--stop-loss",
    "4900",
];

async fn tpsl_with(statuses: Vec<Value>) -> (i32, Value) {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server.mount_exchange(order_reply(statuses)).await;
    let assert = env.live_json(&server).args(TPSL).assert();
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
