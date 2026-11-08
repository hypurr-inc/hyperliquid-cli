use assert_cmd::Command;
use serde_json::{Value, json};

#[test]
fn cached_diagnostics_are_allowlisted_and_explicit_inputs_preserved() {
    let home = tempfile::tempdir().unwrap();
    // The config directory is `$XDG_CONFIG_HOME` on Linux and
    // `~/Library/Application Support` on macOS; cover whichever this platform reads.
    for dir in [
        home.path().join("hyperliquid"),
        home.path()
            .join("Library")
            .join("Application Support")
            .join("hyperliquid"),
    ] {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("last_error.json"),
            json!({
                "category":"api", "reason":"unavailable", "exit_code":12,
                "error":"secret raw error", "retryable":true,
                "data":{"command":"mids", "error":"secret nested error"}
            })
            .to_string(),
        )
        .unwrap();
    }
    let run = |args: &[&str]| -> Value {
        let output = Command::cargo_bin("hyperliquid")
            .unwrap()
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("XDG_DATA_HOME", home.path())
            .args(["--format", "json", "report-incident"])
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    };
    let default = run(&[]);
    let body = default["body"].as_str().unwrap();
    assert!(body.contains("api / unavailable (exit 12)"));
    assert!(!body.contains("secret"));
    assert!(!body.contains("retryable"));
    assert!(!body.contains("\"data\""));
    let explicit = run(&["--context", "explicit context", "--error", "explicit error"]);
    let body = explicit["body"].as_str().unwrap();
    assert!(body.contains("explicit context"));
    assert!(body.contains("explicit error"));
}
