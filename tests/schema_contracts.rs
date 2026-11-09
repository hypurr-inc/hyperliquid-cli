mod support;

use serde_json::json;

use support::{json_command, run_json};

#[test]
fn schema_group_lookup_returns_child_schemas() {
    let schemas = run_json(json_command().args([
        "--select",
        "command,description",
        "--max-results",
        "2",
        "schema",
        "subscribe",
    ]));
    let rows = schemas.as_array().unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["command"], "hyperliquid subscribe trades");
    assert!(rows[0].get("description").is_some());
}

#[test]
fn schema_leaf_lookup_still_returns_single_schema_object() {
    let schema = run_json(json_command().args(["schema", "account", "portfolio"]));

    assert_eq!(schema["command"], "hyperliquid account portfolio <ADDRESS>");
    assert_eq!(schema["command_path"], json!(["account", "portfolio"]));
}
