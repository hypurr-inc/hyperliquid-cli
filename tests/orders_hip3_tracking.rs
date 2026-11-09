//! Orders that are placed on a HIP-3 market and then polled or listed must read
//! the market's own dex: its mids and its open orders are not in the default
//! dex's lists. An order on a dex the account holds nothing on moves collateral
//! there first, unless the account mode does it.

mod support;

use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex, order_reply};
use support::orders::{answer_info, assert_nothing_sent, error_text, words};
use support::signing::test_signer;
use support::wire::{self, MarginRule};
use support::{IsolatedHome, json_stdout, run_json};

const OID: u64 = 7;
const CHASE: &str = "orders chase --coin xyz:TOKEN --side buy --size 20 --timeout 1s";
const BRACKET: &str = "orders bracket --coin xyz:TOKEN --side buy --entry limit --price 1.2 --size 20 --take-profit +10% --stop-loss -5%";
const CREATE: &str = "orders create --coin xyz:TOKEN --side buy --price 1.2 --size 10";
const SCALE: &str = "orders scale --coin xyz:TOKEN --side buy --start-price 1.2 --end-price 1.1 --total-size 20 --orders 2";

fn open_order() -> Value {
    let mut order = wire::basic_order("xyz:TOKEN", OID);
    order["limitPx"] = json!("1.2");
    order["sz"] = json!("20");
    order["origSz"] = json!("20");
    order["tif"] = json!("Alo");
    order
}

/// The market `xyz:TOKEN` lives on the dex `xyz`. The default dex has no such
/// market and no open orders, so a read that loses the dex finds nothing. The
/// account is unified and holds nothing.
async fn hip3_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start_with(vec![Hip3Dex {
        name: "xyz",
        universe: vec![wire::perp("xyz:TOKEN", MarginRule::Either)],
        mids: json!({"xyz:TOKEN": "1.23"}),
    }])
    .await;
    let user = test_signer();
    server
        .mount_info(
            json!({"type": "frontendOpenOrders", "user": user}),
            json!([]),
        )
        .await;
    server
        .mount_info(
            json!({"type": "frontendOpenOrders", "user": user, "dex": "xyz"}),
            json!([open_order()]),
        )
        .await;
    server
        .mount_info(
            json!({"type": "userAbstraction", "user": user}),
            json!("unifiedAccount"),
        )
        .await;
    server
        .mount_info(
            json!({"type": "clearinghouseState", "user": user}),
            wire::perp_state("10000", vec![]),
        )
        .await;
    server
}

/// The exchange answers the one order each command sends with `status`.
async fn placing(status: Value) -> ExchangeFixture {
    let server = hip3_exchange().await;
    server.mount_order_statuses(vec![status]).await;
    server
}

async fn assert_only_dex_xyz_was_listed(server: &ExchangeFixture) {
    let open = server.info_requests("frontendOpenOrders").await;
    assert!(!open.is_empty());
    assert!(open.iter().all(|body| body["dex"] == "xyz"), "{open:?}");
}

#[tokio::test]
async fn chase_polls_its_quote_on_the_market_dex() {
    let env = IsolatedHome::new();
    let server = placing(wire::resting_status(OID)).await;

    let rows = run_json(
        env.live_json(&server)
            .args(words(&format!("{CHASE} --interval 1s"))),
    );

    // The quote rests on the dex's book. Read from the default dex it would
    // vanish and chase would report it filled.
    assert_eq!(rows["status"], "timeout");
    let mids = server.info_requests("allMids").await;
    assert!(mids.iter().any(|body| body["dex"] == "xyz"), "{mids:?}");
    assert_only_dex_xyz_was_listed(&server).await;
}

#[tokio::test]
async fn chase_dry_run_prices_from_the_market_dex_mid() {
    let env = IsolatedHome::new();
    let server = hip3_exchange().await;

    let preview = run_json(
        env.live_json(&server)
            .args(["--dry-run"])
            .args(words(CHASE)),
    );

    // 1.23 less 5 bps is 1.229385, which is finer than the dex market's tick
    // grid; live chase sends it truncated, so the preview shows it truncated.
    assert_eq!(preview["args"]["first_price"], "1.2293");
}

#[tokio::test]
async fn chase_dry_run_refuses_a_market_that_does_not_exist_or_a_size_that_truncates_to_zero() {
    let env = IsolatedHome::new();
    let server = hip3_exchange().await;

    env.live_json(&server)
        .args(["--dry-run"])
        .args(words(
            "orders chase --coin xyz:NOPE --side buy --size 20 --timeout 1s",
        ))
        .assert()
        .code(13);

    // The market's lot is 2 decimals: 0.001 truncates to zero.
    let assert = env
        .live_json(&server)
        .args(["--dry-run"])
        .args(words(
            "orders chase --coin xyz:TOKEN --side buy --size 0.001 --timeout 1s",
        ))
        .assert()
        .code(2);
    let document = json_stdout(&assert);
    assert!(
        error_text(&document).contains("size is too small"),
        "{document}"
    );
}

#[tokio::test]
async fn bracket_entry_polls_the_market_dex_and_cancels_an_unfilled_entry() {
    let env = IsolatedHome::new();
    let server = placing(wire::resting_status(OID)).await;

    let assert = env
        .live_json(&server)
        .args(words(&format!("{BRACKET} --entry-timeout 1s")))
        .assert()
        .code(15);

    // The entry stays on the dex's book until the timeout cancels it; read
    // from the default dex it would look filled and be "protected".
    let document = json_stdout(&assert);
    assert_eq!(document["status"], "entry_cancelled");
    assert_eq!(document["category"], "partial_results");
    assert_eq!(document["exit_code"], 15);
    assert_eq!(document["entry_order_id"], OID);
    assert_only_dex_xyz_was_listed(&server).await;
}

#[tokio::test]
async fn a_bracket_that_cannot_check_its_entry_reports_next_commands_under_next() {
    let env = IsolatedHome::new();
    let server = placing(wire::resting_status(OID)).await;
    answer_info(&server, json!({"type": "frontendOpenOrders"}), 500, "").await;

    let assert = env
        .live_json(&server)
        .args(words(&format!("{BRACKET} --entry-timeout 5s")))
        .assert()
        .code(15);

    let document = json_stdout(&assert);
    assert_eq!(document["status"], "entry_open_unprotected");
    assert_eq!(document["category"], "partial_results");
    assert_eq!(document["reason"], "partial_failure");
    assert_eq!(document["entry_order_id"], OID);
    assert_eq!(document["next"][0]["command"], "hyperliquid orders open");
    assert_eq!(
        document["next"][1]["command"],
        format!("hyperliquid orders cancel {OID}")
    );
    assert!(document.get("next_commands").is_none());
}

#[tokio::test]
async fn bracket_protects_the_whole_entry_when_it_fills_while_resting() {
    let env = IsolatedHome::new();
    let server = hip3_exchange().await;
    server
        .mount_exchange_once(order_reply(vec![wire::resting_status(OID)]))
        .await;
    server
        .mount_order_statuses(vec![json!("waitingForTrigger"), json!("waitingForTrigger")])
        .await;
    // The entry is on the dex's book when placed and gone at the first poll,
    // with no partial fill seen in between.
    server
        .override_info(
            json!({"type": "frontendOpenOrders", "user": test_signer(), "dex": "xyz"}),
            json!([]),
        )
        .await;
    let mut position = wire::position("xyz:TOKEN", "cross");
    position["position"]["szi"] = json!("20");
    position["position"]["entryPx"] = json!("1.2");
    position["position"]["positionValue"] = json!("24");
    server
        .override_info(
            json!({"type": "clearinghouseState", "user": test_signer(), "dex": "xyz"}),
            wire::perp_state("10000", vec![position]),
        )
        .await;

    let assert = env
        .live_json(&server)
        .args(words(&format!("{BRACKET} --entry-timeout 5s")))
        .assert()
        .success();

    let document = json_stdout(&assert);
    assert_eq!(document["status"], "armed");
    assert_eq!(document["protected_size"], "20");
    let actions = server.exchange_actions().await;
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert_eq!(actions[1]["grouping"], "positionTpsl");
    for leg in actions[1]["orders"].as_array().unwrap() {
        assert_eq!(leg["s"], "20");
    }
}

#[tokio::test]
async fn cancel_all_with_a_dex_lists_that_dexs_open_orders_whatever_the_dex_is_spelled() {
    let env = IsolatedHome::new();
    let server = hip3_exchange().await;
    server
        .mount_exchange(json!({
            "status": "ok",
            "response": {"type": "cancel", "data": {"statuses": ["success"]}}
        }))
        .await;

    for dex in ["xyz", "XYZ"] {
        let rows = run_json(env.live_json(&server).args(words(&format!(
            "orders cancel-all --coin TOKEN --dex {dex} --yes"
        ))));
        assert_eq!(rows[0]["cancelled_orders"], 1, "{dex}");
    }

    assert_only_dex_xyz_was_listed(&server).await;
}

#[tokio::test]
async fn cancel_all_for_a_coin_the_dex_does_not_list_fails_instead_of_cancelling_nothing() {
    let env = IsolatedHome::new();
    let server = hip3_exchange().await;

    let assert = env
        .live_json(&server)
        .args(words("orders cancel-all --coin NOPE --dex xyz --yes"))
        .assert()
        .code(13);

    let document = json_stdout(&assert);
    assert!(error_text(&document).contains("NOPE"), "{document}");
    assert_nothing_sent(&server).await;
}

/// An account with nothing on `xyz` and the given raw `userAbstraction`: unless
/// the mode has the exchange move collateral itself, every order there needs a
/// helper transfer first.
async fn with_unfunded_dex_account(server: &ExchangeFixture, raw_mode: &str) {
    server
        .override_info(
            json!({"type": "userAbstraction", "user": test_signer()}),
            json!(raw_mode),
        )
        .await;
    server
        .mount_info(
            json!({"type": "clearinghouseState", "user": test_signer(), "dex": "xyz"}),
            wire::perp_state_with("0", "0", "0", "0", vec![]),
        )
        .await;
}

#[tokio::test]
async fn a_create_dry_run_previews_the_hip3_helper_transfer() {
    let env = IsolatedHome::new();
    let server = hip3_exchange().await;
    with_unfunded_dex_account(&server, "default").await;

    let preview = run_json(
        env.live_json(&server)
            .args(["--dry-run"])
            .args(words(CREATE)),
    );

    // 10 x 1.2 = 12 of notional at 10x leverage with a 20% buffer is 1.44.
    assert_eq!(
        preview["args"]["side_effects"],
        json!([{
            "action": "send_asset",
            "source": "perp",
            "dest": "dex:xyz",
            "token": "USDC",
            "amount": "1.44",
        }])
    );
}

#[tokio::test]
async fn modes_where_the_exchange_moves_dex_collateral_need_no_helper_transfer() {
    // dexAbstraction is the discontinued mode in which the exchange moves
    // collateral to the dex an order trades on; the CLI must not also do it.
    for raw_mode in ["portfolioMargin", "dexAbstraction", "unifiedAccount"] {
        let env = IsolatedHome::new();
        let server = hip3_exchange().await;
        with_unfunded_dex_account(&server, raw_mode).await;

        let preview = run_json(
            env.live_json(&server)
                .args(["--dry-run"])
                .args(words(CREATE)),
        );

        assert!(preview["args"].get("side_effects").is_none(), "{raw_mode}");
    }
}

#[tokio::test]
async fn scale_on_a_hip3_market_never_moves_funds() {
    let env = IsolatedHome::new();
    let server = hip3_exchange().await;
    with_unfunded_dex_account(&server, "default").await;
    server
        .mount_order_statuses(vec![wire::resting_status(1), wire::resting_status(2)])
        .await;

    let preview = run_json(
        env.live_json(&server)
            .args(["--dry-run"])
            .args(words(SCALE)),
    );
    assert!(preview["args"].get("side_effects").is_none());

    run_json(env.live_json(&server).args(words(SCALE)).arg("--yes"));

    let actions = server.exchange_actions().await;
    let types: Vec<&Value> = actions.iter().map(|action| &action["type"]).collect();
    assert_eq!(types, ["order"]);
}
