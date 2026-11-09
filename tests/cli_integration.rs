mod support;

use support::exchange::ExchangeFixture;

use predicates::prelude::*;
use serde_json::{Value, json};

use support::{
    API_OVERRIDE_ENV, FORMAT_ENV, IsolatedHome, TEST_ACCOUNT_PASSPHRASE, TESTNET_API_OVERRIDE_ENV,
    wire,
};

async fn mock_malformed_spot_meta_server() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .override_info(json!({"type": "spotMeta"}), wire::malformed_spot_meta())
        .await;
    server
        .override_info(json!({"type": "meta"}), wire::perp_meta_btc_only())
        .await;
    server
}

#[test]
fn test_version_json_is_one_document_and_pretty_is_plain() {
    let json = support::hermetic_command()
        .args(["--format", "json", "orders", "create", "--version"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));

    support::hermetic_command()
        .args(["--format", "pretty", "-v"])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "hyperliquid {}",
            env!("CARGO_PKG_VERSION")
        )));
}

#[test]
fn test_group_without_action_prints_help_on_stdout_and_exits_zero() {
    for format in ["json", "pretty"] {
        support::hermetic_command()
            .args(["--format", format, "orders"])
            .assert()
            .success()
            .stdout(predicates::str::contains("Usage: hyperliquid orders"))
            .stdout(predicates::str::contains("create"))
            .stderr(predicates::str::is_empty());
    }
}

#[test]
fn test_short_and_long_help_match() {
    let short = support::hermetic_command()
        .args(["orders", "create", "-h"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let long = support::hermetic_command()
        .args(["orders", "create", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(short, long);
    assert!(String::from_utf8_lossy(&long).contains("Notes:"));
}

#[test]
fn test_no_args_shows_info() {
    support::hermetic_command()
        .assert()
        .success()
        .stdout(predicates::str::contains("Usage: hyperliquid"));
}

#[test]
fn orders_create_help_only_lists_accepted_grouping_values() {
    let output = support::hermetic_command()
        .args(["orders", "create", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output).unwrap();

    assert!(help.contains("normal-tpsl"));
    assert!(!help.contains("position-tpsl"), "{help}");
}

#[test]
fn orders_create_rejects_position_tpsl_grouping() {
    support::hermetic_command()
        .env("HYPERLIQUID_FORMAT", "pretty")
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
        .failure()
        .code(2)
        .stderr(predicate::str::contains("position-tpsl"))
        .stderr(predicate::str::contains("normal-tpsl"));
}

#[test]
fn schema_orders_status_requires_oid_or_cloid() {
    let output = support::hermetic_command()
        .args(["--format", "json", "schema", "orders", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["command"], "hyperliquid orders status");
    assert_eq!(json["json_schema"]["required"], serde_json::json!(["user"]));
    assert_eq!(
        json["json_schema"]["oneOf"],
        serde_json::json!([
            {"required": ["oid"]},
            {"required": ["cloid"]}
        ])
    );
}

#[test]
fn missing_required_argument_exits_2() {
    for args in [["perps", "get"], ["spot", "get"]] {
        support::hermetic_command()
            .env("HYPERLIQUID_FORMAT", "pretty")
            .args(args)
            .assert()
            .code(2)
            .stderr(predicates::str::contains("required"));
    }
}

#[test]
fn test_candles_invalid_interval_exits_2() {
    support::hermetic_command()
        .env("HYPERLIQUID_FORMAT", "pretty")
        .args(["candles", "BTC", "--interval", "999x"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("Valid intervals"));
}

#[test]
fn mistyped_subcommand_suggests_the_fix_and_points_at_its_group_help() {
    let output = support::hermetic_command()
        .args(["--format", "json", "orders", "cancl"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["reason"], "unknown_command");
    assert_eq!(json["data"]["suggestion"], "cancel");
    assert_eq!(json["next"][0]["command"], "hyperliquid orders --help");
}

#[tokio::test]
async fn test_select_filters_perps_json_through_command_context() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);

    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--select",
            "name,max_leverage",
            "perps",
            "list",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let first = json
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(
        first.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["max_leverage", "name"]
    );
    assert_eq!(first["name"], "BTC");
    assert_eq!(first["max_leverage"], 50);
}

#[tokio::test]
async fn non_tty_defaults_to_json_for_read_commands() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);

    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .env_remove(FORMAT_ENV)
        .args(["--select", "coin,price", "mids"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let first = json.as_array().unwrap().first().unwrap();
    assert_eq!(first["coin"], "BTC");
    assert_eq!(first["price"], "50000");
}

#[tokio::test]
async fn explicit_pretty_overrides_non_tty_json_default() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);

    command
        .env(API_OVERRIDE_ENV, server.uri())
        .args(["--format", "pretty", "mids"])
        .assert()
        .success()
        .stdout(predicates::str::contains("BTC"))
        .stdout(predicates::str::contains("\"BTC\"").not());
}

#[tokio::test]
async fn hyperliquid_format_env_overrides_non_tty_default() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);

    command
        .env(API_OVERRIDE_ENV, server.uri())
        .env("HYPERLIQUID_FORMAT", "pretty")
        .args(["mids"])
        .assert()
        .success()
        .stdout(predicates::str::contains("BTC"))
        .stdout(predicates::str::contains("\"BTC\"").not());
}

#[test]
fn invalid_hyperliquid_format_env_returns_json_configuration_error() {
    let env = IsolatedHome::new();
    let mut command = env.command();

    let output = command
        .env("HYPERLIQUID_FORMAT", "xml")
        .args(["schema"])
        .assert()
        .failure()
        .code(2)
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("HYPERLIQUID_FORMAT")
    );
}

#[tokio::test]
async fn test_max_results_limits_arrays_before_select() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--max-results",
            "1",
            "--select",
            "name",
            "perps",
            "list",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let rows = json.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "BTC");
    assert!(rows[0].get("max_leverage").is_none());
}

#[tokio::test]
async fn orders_create_dry_run_validates_and_does_not_require_auth() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
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
            "0.001",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["command"], "orders create");
    assert_eq!(json["would_execute"], "submit_order");
    assert_eq!(json["args"]["coin"], "BTC");
    assert_eq!(json["args"]["reduce_only"], false);
    assert_eq!(json["args"]["margin_mode"], "cross");
}

#[tokio::test]
async fn orders_create_dry_run_reports_reduce_only_for_limit_and_market() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let cases: [&[&str]; 2] = [
        &["--type", "limit", "--price", "90000", "--size", "0.001"],
        &["--type", "market", "--amount", "50"],
    ];
    for sizing in cases {
        let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
        let output = command
            .env(API_OVERRIDE_ENV, server.uri())
            .args(["--format", "json", "--dry-run", "orders", "create"])
            .args(["--coin", "BTC", "--side", "sell", "--reduce-only"])
            .args(sizing)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let json: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(json["args"]["reduce_only"], true, "{sizing:?}");
    }
}

#[tokio::test]
async fn orders_create_dry_run_shows_normal_tpsl_children() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
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
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let args = &json["args"];
    assert_eq!(json["command"], "orders create");
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
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
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
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["args"]["margin_mode"], "isolated");
}

#[tokio::test]
async fn orders_tpsl_dry_run_shows_position_tpsl_children_without_auth() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
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
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let args = &json["args"];
    assert_eq!(json["command"], "orders tpsl");
    assert_eq!(json["would_execute"], "submit_position_tpsl");
    assert_eq!(args["grouping_wire"], "positionTpsl");
    assert_eq!(args["size_mode"], "current_position");
    assert_eq!(args["side"], "sell");
    assert_eq!(args["margin_mode"], "cross");
    assert_eq!(args["batch_order_preview"]["grouping"], "positionTpsl");
    assert_eq!(
        args["batch_order_preview"]["size_source"],
        "current_position"
    );
    assert_eq!(
        args["batch_order_preview"]["orders"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(args["batch_order_preview"]["orders"][0]["b"], false);
    assert_eq!(
        args["batch_order_preview"]["orders"][0]["s"],
        "current_position"
    );
    assert_eq!(args["legs"][1]["leg"], "stop_loss");
}

#[tokio::test]
async fn orders_twap_create_dry_run_shows_validated_intent() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
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
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let args = &json["args"];
    assert_eq!(json["command"], "orders twap-create");
    assert_eq!(json["would_execute"], "create_twap_order");
    assert_eq!(args["coin"], "ETH");
    assert_eq!(args["side"], "buy");
    assert_eq!(args["size"], "0.25");
    assert_eq!(args["duration"], 600);
    assert_eq!(args["resolved_asset"], "ETH");
    assert_eq!(args["margin_mode"], "cross");
}

#[tokio::test]
async fn orders_twap_cancel_dry_run_shows_validated_intent() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "twap-cancel",
            "42",
            "--coin",
            "ETH",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let args = &json["args"];
    assert_eq!(json["command"], "orders twap-cancel");
    assert_eq!(json["would_execute"], "cancel_twap_order");
    assert_eq!(args["twap_id"], 42);
    assert_eq!(args["coin"], "ETH");
    assert_eq!(args["resolved_asset"], "ETH");
}

#[tokio::test]
async fn orders_schedule_cancel_dry_run_shows_validated_intent() {
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "schedule-cancel",
            "--in",
            "5m",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["command"], "orders schedule-cancel");
    assert_eq!(json["would_execute"], "schedule_dead_mans_switch");
    assert_eq!(json["args"]["mode"], "set");
    assert_eq!(json["args"]["in_duration_ms"], 300000);
    assert!(json["vault_address"].is_null());
}

#[tokio::test]
async fn orders_schedule_cancel_rejects_in_below_exchange_minimum() {
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    command
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "schedule-cancel",
            "--in",
            "4s",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "orders schedule-cancel --in must be at least 5s",
        ));
}

#[tokio::test]
async fn orders_schedule_cancel_clear_dry_run_shows_validated_intent() {
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "schedule-cancel",
            "--clear",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["command"], "orders schedule-cancel");
    assert_eq!(json["would_execute"], "schedule_dead_mans_switch");
    assert_eq!(json["args"]["mode"], "clear");
}

#[tokio::test]
async fn orders_schedule_cancel_dry_run_preserves_on_behalf_of_context() {
    let env = IsolatedHome::new();
    let subaccount = "0x0000000000000000000000000000000000000123";
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "schedule-cancel",
            "--in",
            "5s",
            "--on-behalf-of",
            subaccount,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["args"]["on_behalf_of"], subaccount);
    assert_eq!(json["acting_as"], subaccount);
    assert_eq!(json["vault_address"], subaccount);
}

#[tokio::test]
async fn orders_cancel_dry_run_shows_normalized_identifier() {
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "cancel",
            "--cloid",
            "0xabcd",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["command"], "orders cancel");
    assert_eq!(json["would_execute"], "cancel_order");
    assert_eq!(json["args"]["cloid"], "0xabcd");
    assert_eq!(json["args"]["identifier"], "0xabcd");
}

#[tokio::test]
async fn orders_cancel_all_dry_run_shows_resolved_coin_filter() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "orders",
            "cancel-all",
            "--coin",
            "ETH",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["command"], "orders cancel-all");
    assert_eq!(json["would_execute"], "cancel_open_orders");
    assert_eq!(json["args"]["coin"], "ETH");
    assert_eq!(json["args"]["resolved_coin_filter"], "ETH");
}

#[tokio::test]
async fn dry_run_payload_redacts_private_key_like_fields() {
    let server = ExchangeFixture::start().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);
    let output = command
        .env(API_OVERRIDE_ENV, server.uri())
        .args([
            "--format",
            "json",
            "--dry-run",
            "--payload-json",
            r#"{
                "private_key":"0xabc",
                "api_key":"api-secret",
                "access_token":"token-secret",
                "authorization":"Bearer header-secret",
                "nested":{
                    "password":"password-secret",
                    "mnemonic":"word list",
                    "seed_phrase":"seed-secret",
                    "passphrase":"pass-secret",
                    "credential":"credential-secret",
                    "signature":"0xsig",
                    "bearer_value":"Bearer value-secret"
                }
            }"#,
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
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["payload"]["private_key"], "[redacted]");
    assert_eq!(json["payload"]["api_key"], "[redacted]");
    assert_eq!(json["payload"]["access_token"], "[redacted]");
    assert_eq!(json["payload"]["authorization"], "[redacted]");
    assert_eq!(json["payload"]["nested"]["password"], "[redacted]");
    assert_eq!(json["payload"]["nested"]["mnemonic"], "[redacted]");
    assert_eq!(json["payload"]["nested"]["seed_phrase"], "[redacted]");
    assert_eq!(json["payload"]["nested"]["passphrase"], "[redacted]");
    assert_eq!(json["payload"]["nested"]["credential"], "[redacted]");
    assert_eq!(json["payload"]["nested"]["signature"], "[redacted]");
    assert_eq!(json["payload"]["nested"]["bearer_value"], "[redacted]");
}

#[test]
fn hardening_rejects_traversal_like_resource_ids() {
    support::hermetic_command()
        .args([
            "--format",
            "json",
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
        .failure()
        .code(2)
        .stdout(predicate::str::contains("path traversal"));
}

#[tokio::test]
async fn testnet_spot_list_skips_malformed_spot_meta_entries() {
    let server = mock_malformed_spot_meta_server().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);

    let output = command
        .env(TESTNET_API_OVERRIDE_ENV, server.uri())
        .args(["--testnet", "--format", "json", "spot", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let markets = json.as_array().unwrap();
    assert_eq!(markets.len(), 1);
    assert_eq!(markets[0]["symbol"], "PURR/USDC");
}

#[tokio::test]
async fn testnet_resolver_backed_perps_get_tolerates_malformed_spot_meta() {
    let server = mock_malformed_spot_meta_server().await;
    let env = IsolatedHome::new();
    let mut command = env.account_command(TEST_ACCOUNT_PASSPHRASE);

    let output = command
        .env(TESTNET_API_OVERRIDE_ENV, server.uri())
        .args(["--testnet", "--format", "json", "perps", "get", "BTC"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["name"], "BTC");
    assert_eq!(json["max_leverage"], 50);
}
