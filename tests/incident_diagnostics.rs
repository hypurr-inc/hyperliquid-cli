mod support;

use serde_json::{Value, json};
use support::IsolatedHome;

#[test]
fn incident_context_excludes_remote_data_and_preserves_explicit_context() {
    let home = IsolatedHome::new();
    let dir = home.config.join("hyperliquid");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("last_error.json"),
        json!({
            "category": "api", "reason": "unavailable", "exit_code": 12, "retryable": true,
            "message": "secret top-level message",
            "data": {"command": "mids", "http_status": 503,
                "remote_message": "secret remote message", "unknown": "secret unknown"}
        })
        .to_string(),
    )
    .unwrap();
    let output = home
        .command()
        .args(["--json", "report-incident"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    let body = report["body"].as_str().unwrap();
    assert!(!body.contains("secret"));
    let context = body
        .split("```json\n")
        .nth(1)
        .unwrap()
        .split("\n```")
        .next()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(context).unwrap(),
        json!({
            "category": "api", "reason": "unavailable", "exit_code": 12, "retryable": true,
            "command": "mids", "http_status": 503
        })
    );
    let output = home
        .command()
        .args([
            "--json",
            "report-incident",
            "--context",
            "explicit remote_message",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        report["body"]
            .as_str()
            .unwrap()
            .contains("```json\nexplicit remote_message\n```")
    );
}

#[test]
fn recorded_command_uses_parsed_path_with_global_option_values() {
    let home = IsolatedHome::new();
    home.command()
        .args([
            "--format",
            "json",
            "--ows-signer",
            "unused",
            "orders",
            "create",
            "--coin",
            "BTC",
            "--side",
            "buy",
        ])
        .assert()
        .failure();
    let recorded: Value = serde_json::from_slice(
        &std::fs::read(home.config.join("hyperliquid/last_error.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(recorded["data"]["command"], "orders create");
}
