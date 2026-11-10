mod support;

use serde_json::{Value, json};
use support::{IsolatedHome, run_json};

#[test]
fn incident_context_excludes_remote_data_and_preserves_explicit_context() {
    let home = IsolatedHome::new();
    home.write_last_error(&json!({
        "category": "api", "reason": "unavailable", "exit_code": 12, "retryable": true,
        "message": "secret top-level message",
        "data": {"command": "mids", "http_status": 503,
            "remote_message": "secret remote message", "unknown": "secret unknown"}
    }));
    let report = run_json(home.command().args(["--json", "report-incident"]));
    let body = report["body"].as_str().unwrap();
    assert!(!body.contains("secret"));
    assert!(body.contains("api / unavailable (exit 12)"), "{body}");
    assert!(!body.contains("retryable"), "{body}");
    assert!(!body.contains("\"data\""), "{body}");
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
            "category": "api", "reason": "unavailable", "exit_code": 12
        })
    );

    let report = run_json(home.command().args([
        "--json",
        "report-incident",
        "--context",
        "explicit remote_message",
        "--error",
        "explicit error",
    ]));
    let body = report["body"].as_str().unwrap();
    assert!(
        body.contains("```json\nexplicit remote_message\n```"),
        "{body}"
    );
    assert!(body.contains("explicit error"), "{body}");
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
    let recorded = home.read_last_error();
    assert_eq!(recorded["data"]["command"], "orders create");
}
