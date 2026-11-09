mod support;

use predicates::prelude::*;
use serde_json::{Value, json};

use support::exchange::ExchangeFixture;
use support::{
    API_OVERRIDE_ENV, FORMAT_ENV, IsolatedHome, TESTNET_API_OVERRIDE_ENV, hermetic_command,
    json_command, run_json, run_json_failure, wire,
};

/// The `--dry-run` preview of `args`, for a command that reads nothing. The
/// pipeline reads the config file, so the command gets a home.
fn preview(args: &[&str]) -> Value {
    run_json(IsolatedHome::new().json_command().arg("--dry-run").args(args))
}

/// The `--dry-run` preview of `args`, for a command that resolves a market
/// against `server`.
fn preview_on(server: &ExchangeFixture, args: &[&str]) -> Value {
    run_json(
        IsolatedHome::new()
            .json_command()
            .env(API_OVERRIDE_ENV, server.uri())
            .arg("--dry-run")
            .args(args),
    )
}

#[test]
fn version_is_one_json_document_and_plain_text_when_pretty() {
    let json = run_json(json_command().args(["orders", "create", "--version"]));
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));

    hermetic_command()
        .args(["--format", "pretty", "-v"])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "hyperliquid {}",
            env!("CARGO_PKG_VERSION")
        )));
}

#[test]
fn a_group_without_an_action_prints_its_help_on_stdout_and_exits_zero() {
    for format in ["json", "pretty"] {
        hermetic_command()
            .args(["--format", format, "orders"])
            .assert()
            .success()
            .stdout(predicates::str::contains("Usage: hyperliquid orders"))
            .stdout(predicates::str::contains("create"))
            .stderr(predicates::str::is_empty());
    }
}

#[test]
fn short_and_long_help_are_the_same() {
    let help = |flag| {
        hermetic_command()
            .args(["orders", "create", flag])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    assert_eq!(help("-h"), help("--help"));
}

#[test]
fn orders_create_does_not_accept_the_position_tpsl_grouping() {
    let help = hermetic_command()
        .args(["orders", "create", "--help"])
        .assert()
        .success();
    help.stdout(predicate::str::contains("normal-tpsl"))
        .stdout(predicate::str::contains("position-tpsl").not());

    hermetic_command()
        .env(FORMAT_ENV, "pretty")
        .args([
            "--dry-run",
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
            "--take-profit",
            "55000",
            "--grouping",
            "position-tpsl",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("position-tpsl"))
        .stderr(predicate::str::contains("normal-tpsl"));
}

#[test]
fn schema_orders_status_requires_oid_or_cloid() {
    let json = run_json(json_command().args(["schema", "orders", "status"]));

    assert_eq!(json["command"], "hyperliquid orders status");
    assert_eq!(json["json_schema"]["required"], json!(["user"]));
    assert_eq!(
        json["json_schema"]["oneOf"],
        json!([{"required": ["oid"]}, {"required": ["cloid"]}])
    );
}

#[test]
fn a_missing_required_argument_is_a_usage_error() {
    for args in [["perps", "get"], ["spot", "get"]] {
        hermetic_command()
            .env(FORMAT_ENV, "pretty")
            .args(args)
            .assert()
            .code(2)
            .stderr(predicates::str::contains("required"));
    }
}

#[test]
fn an_unknown_candle_interval_lists_the_valid_ones() {
    hermetic_command()
        .env(FORMAT_ENV, "pretty")
        .args(["candles", "BTC", "--interval", "999x"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("Valid intervals"));
}

#[tokio::test]
async fn output_format_follows_flag_then_environment_then_the_pipe() {
    let server = ExchangeFixture::start().await;
    let home = IsolatedHome::new();
    let mids = || {
        let mut command = home.command_with_server(&server);
        command.env_remove(FORMAT_ENV);
        command
    };

    let piped = run_json(mids().args(["--select", "coin,price", "mids"]));
    assert_eq!(piped[0]["coin"], "BTC");
    assert_eq!(piped[0]["price"], "50000");

    let pretty = |command: &mut assert_cmd::Command| {
        command
            .assert()
            .success()
            .stdout(predicates::str::contains("BTC"))
            .stdout(predicates::str::contains("\"BTC\"").not());
    };
    pretty(mids().args(["--format", "pretty", "mids"]));
    pretty(mids().env(FORMAT_ENV, "pretty").arg("mids"));
    pretty(
        mids()
            .env(FORMAT_ENV, "json")
            .args(["--format", "pretty", "mids"]),
    );
}

#[test]
fn an_invalid_format_variable_is_a_json_configuration_error() {
    let error = run_json_failure(
        hermetic_command()
            .env(FORMAT_ENV, "xml")
            .arg("schema"),
        2,
    );

    assert!(
        error["error"].as_str().unwrap().contains(FORMAT_ENV),
        "{error}"
    );
}

#[tokio::test]
async fn orders_create_dry_run_reports_reduce_only_for_limit_and_market() {
    let server = ExchangeFixture::start().await;
    let cases: [&[&str]; 2] = [
        &["--type", "limit", "--price", "90000", "--size", "0.001"],
        &["--type", "market", "--amount", "50"],
    ];
    for sizing in cases {
        let json = preview_on(
            &server,
            &[
                &["orders", "create", "--coin", "BTC", "--side", "sell"][..],
                &["--reduce-only"],
                sizing,
            ]
            .concat(),
        );
        assert_eq!(json["args"]["reduce_only"], true, "{sizing:?}");
    }
}

#[tokio::test]
async fn orders_create_dry_run_shows_normal_tpsl_children() {
    let server = ExchangeFixture::start().await;
    let json = preview_on(
        &server,
        &[
            "orders",
            "create",
            "--coin",
            "ETH",
            "--side",
            "buy",
            "--type",
            "limit",
            "--price",
            "2000",
            "--size",
            "0.1",
            "--take-profit",
            "2200",
            "--stop-loss",
            "1900",
            "--grouping",
            "normal-tpsl",
        ],
    );
    let args = &json["args"];

    assert_eq!(args["grouping"], "normal-tpsl");
    assert_eq!(args["batch_order"]["grouping"], "normalTpsl");
    assert_eq!(args["batch_order"]["orders"].as_array().unwrap().len(), 3);
    assert_eq!(args["legs"][1]["leg"], "take_profit");
    assert_eq!(args["legs"][1]["side"], "sell");
    assert_eq!(args["legs"][1]["reduce_only"], true);
    assert_eq!(args["batch_order"]["orders"][1]["b"], false);
    assert_eq!(
        args["batch_order"]["orders"][1]["t"]["trigger"]["tpsl"],
        "tp"
    );
    assert_eq!(
        args["batch_order"]["orders"][2]["t"]["trigger"]["triggerPx"],
        "1900"
    );
}

#[tokio::test]
async fn orders_create_dry_run_surfaces_margin_mode_intent() {
    let server = ExchangeFixture::start().await;
    let json = preview_on(
        &server,
        &[
            "orders",
            "create",
            "--coin",
            "ETH",
            "--side",
            "buy",
            "--price",
            "2000",
            "--size",
            "0.1",
            "--margin-mode",
            "isolated",
        ],
    );

    assert_eq!(json["args"]["margin_mode"], "isolated");
}

#[tokio::test]
async fn orders_tpsl_dry_run_shows_position_tpsl_children_without_auth() {
    let server = ExchangeFixture::start().await;
    let json = preview_on(
        &server,
        &[
            "orders",
            "tpsl",
            "--coin",
            "ETH",
            "--take-profit",
            "2200",
            "--stop-loss",
            "1900",
            "--grouping",
            "position-tpsl",
        ],
    );
    let args = &json["args"];
    let batch = &args["batch_order_preview"];

    assert_eq!(json["command"], "orders tpsl");
    assert_eq!(json["would_execute"], "submit_position_tpsl");
    assert_eq!(args["grouping_wire"], "positionTpsl");
    assert_eq!(args["size_mode"], "current_position");
    assert_eq!(args["side"], "sell");
    assert_eq!(args["margin_mode"], "cross");
    assert_eq!(batch["grouping"], "positionTpsl");
    assert_eq!(batch["size_source"], "current_position");
    assert_eq!(batch["orders"].as_array().unwrap().len(), 2);
    assert_eq!(batch["orders"][0]["b"], false);
    assert_eq!(batch["orders"][0]["s"], "current_position");
    assert_eq!(args["legs"][1]["leg"], "stop_loss");
}

#[tokio::test]
async fn orders_twap_dry_runs_show_the_validated_intent() {
    let server = ExchangeFixture::start().await;

    let create = preview_on(
        &server,
        &[
            "orders",
            "twap-create",
            "--coin",
            "ETH",
            "--side",
            "buy",
            "--size",
            "0.25",
            "--duration",
            "600",
        ],
    );
    assert_eq!(create["command"], "orders twap-create");
    assert_eq!(create["would_execute"], "create_twap_order");
    assert_eq!(create["args"]["coin"], "ETH");
    assert_eq!(create["args"]["side"], "buy");
    assert_eq!(create["args"]["size"], "0.25");
    assert_eq!(create["args"]["duration"], 600);
    assert_eq!(create["args"]["resolved_asset"], "ETH");
    assert_eq!(create["args"]["margin_mode"], "cross");

    let cancel = preview_on(
        &server,
        &["orders", "twap-cancel", "42", "--coin", "ETH"],
    );
    assert_eq!(cancel["command"], "orders twap-cancel");
    assert_eq!(cancel["would_execute"], "cancel_twap_order");
    assert_eq!(cancel["args"]["twap_id"], 42);
    assert_eq!(cancel["args"]["coin"], "ETH");
    assert_eq!(cancel["args"]["resolved_asset"], "ETH");
}

#[test]
fn orders_schedule_cancel_dry_run_shows_the_validated_intent() {
    let set = preview(&["orders", "schedule-cancel", "--in", "5m"]);
    assert_eq!(set["command"], "orders schedule-cancel");
    assert_eq!(set["would_execute"], "schedule_dead_mans_switch");
    assert_eq!(set["args"]["mode"], "set");
    assert_eq!(set["args"]["in_duration_ms"], 300_000);
    assert!(set["vault_address"].is_null());

    let clear = preview(&["orders", "schedule-cancel", "--clear"]);
    assert_eq!(clear["would_execute"], "schedule_dead_mans_switch");
    assert_eq!(clear["args"]["mode"], "clear");

    let subaccount = "0x0000000000000000000000000000000000000123";
    let on_behalf = preview(&[
        "orders",
        "schedule-cancel",
        "--in",
        "5s",
        "--on-behalf-of",
        subaccount,
    ]);
    assert_eq!(on_behalf["args"]["on_behalf_of"], subaccount);
    assert_eq!(on_behalf["acting_as"], subaccount);
    assert_eq!(on_behalf["vault_address"], subaccount);
}

#[test]
fn orders_schedule_cancel_rejects_in_below_the_exchange_minimum() {
    IsolatedHome::new()
        .json_command()
        .args(["--dry-run", "orders", "schedule-cancel", "--in", "4s"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "orders schedule-cancel --in must be at least 5s",
        ));
}

#[test]
fn orders_cancel_dry_run_shows_the_normalized_identifier() {
    let json = preview(&["orders", "cancel", "--cloid", "0xabcd"]);

    assert_eq!(json["command"], "orders cancel");
    assert_eq!(json["would_execute"], "cancel_order");
    assert_eq!(json["args"]["cloid"], "0xabcd");
    assert_eq!(json["args"]["identifier"], "0xabcd");
}

#[tokio::test]
async fn orders_cancel_all_dry_run_shows_the_resolved_coin_filter() {
    let server = ExchangeFixture::start().await;
    let json = preview_on(&server, &["orders", "cancel-all", "--coin", "ETH"]);

    assert_eq!(json["command"], "orders cancel-all");
    assert_eq!(json["would_execute"], "cancel_open_orders");
    assert_eq!(json["args"]["coin"], "ETH");
    assert_eq!(json["args"]["resolved_coin_filter"], "ETH");
}

#[tokio::test]
async fn dry_run_payload_redacts_secret_like_fields_at_every_depth() {
    let server = ExchangeFixture::start().await;
    let secrets = [
        "private_key",
        "api_key",
        "access_token",
        "authorization",
        "password",
        "mnemonic",
        "seed_phrase",
        "passphrase",
        "credential",
        "signature",
        "bearer_value",
    ];
    let payload = json!({
        "private_key": "0xabc",
        "api_key": "api-secret",
        "access_token": "token-secret",
        "authorization": "Bearer header-secret",
        "nested": {
            "password": "password-secret",
            "mnemonic": "word list",
            "seed_phrase": "seed-secret",
            "passphrase": "pass-secret",
            "credential": "credential-secret",
            "signature": "0xsig",
            "bearer_value": "Bearer value-secret"
        }
    });

    let json = preview_on(
        &server,
        &[
            "--payload-json",
            &payload.to_string(),
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
            "--price",
            "50000",
            "--size",
            "0.001",
        ],
    );

    let shown = &json["payload"];
    for name in secrets {
        let value = shown.get(name).unwrap_or_else(|| &shown["nested"][name]);
        assert_eq!(value, "[redacted]", "{name}");
    }
}

#[test]
fn resource_ids_that_look_like_path_traversal_are_rejected() {
    json_command()
        .args([
            "--dry-run",
            "orders",
            "create",
            "--coin",
            "../BTC",
            "--side",
            "buy",
            "--price",
            "50000",
            "--size",
            "0.001",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("path traversal"));
}

/// Testnet spot metadata whose second pair references a missing token.
async fn malformed_spot_meta_server() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), wire::malformed_spot_meta())
        .await;
    server
        .override_info(json!({"type": "meta"}), wire::perp_meta_btc_only())
        .await;
    server
}

#[tokio::test]
async fn testnet_reads_skip_malformed_spot_meta_entries() {
    let server = malformed_spot_meta_server().await;
    let home = IsolatedHome::new();
    let read = |args: &[&str]| {
        run_json(
            home.json_command()
                .env(TESTNET_API_OVERRIDE_ENV, server.uri())
                .arg("--testnet")
                .args(args),
        )
    };

    let markets = read(&["spot", "list"]);
    assert_eq!(markets.as_array().unwrap().len(), 1);
    assert_eq!(markets[0]["symbol"], "PURR/USDC");

    let perp = read(&["perps", "get", "BTC"]);
    assert_eq!(perp["name"], "BTC");
    assert_eq!(perp["max_leverage"], 50);
}
