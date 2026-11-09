mod support;

use predicates::prelude::*;

#[test]
fn every_format_is_accepted_by_its_long_and_short_flag() {
    for flag in ["--format", "-f"] {
        for format in ["pretty", "table", "json"] {
            support::hermetic_command()
                .args([flag, format, "--help"])
                .assert()
                .success();
        }
    }
}

#[test]
fn test_json_flag_enables_json_mode() {
    let output = support::hermetic_command()
        .arg("--json")
        .arg("--version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn top_level_help_explains_dynamic_agent_json_default() {
    support::hermetic_command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "json when stdout is not a terminal",
        ))
        .stdout(predicate::str::contains("HYPERLIQUID_AGENT"))
        .stdout(predicate::str::contains(
            "Unknown fields are omitted when the output shape is dynamic",
        ));
}

#[test]
fn schema_help_includes_agent_examples() {
    support::hermetic_command()
        .args(["schema", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hyperliquid --format json schema"))
        .stdout(predicate::str::contains("schema orders create"));
}

#[test]
fn orders_create_help_surfaces_risk_and_dry_run() {
    support::hermetic_command()
        .args(["orders", "create", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Risk: moves funds"))
        .stdout(predicate::str::contains("Dry-run: supported"))
        .stdout(predicate::str::contains("Confirmation:"));
}

#[test]
fn json_payload_parse_errors_do_not_echo_secret_content() {
    support::hermetic_command()
        .args([
            "--format",
            "json",
            "--dry-run",
            "--payload-json",
            r#"{"api_key":"should-not-render","#,
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
        .failure()
        .code(2)
        .stdout(predicate::str::contains("invalid payload JSON"))
        .stdout(predicate::str::contains("should-not-render").not())
        .stdout(predicate::str::contains("api_key").not());
}
