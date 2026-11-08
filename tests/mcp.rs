use assert_cmd::Command;
use serde_json::{Value, json};

#[test]
fn stdio_initialize_list_and_call() {
    let home = tempfile::tempdir().unwrap();
    let messages = [
        "{".to_string(),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled"}).to_string(),
        json!({"jsonrpc":"2.0","method":"ping"}).to_string(),
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string(),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string(),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"schema","arguments":{"command_path":["feedback"]}}}).to_string(),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"missing"}}).to_string(),
        json!({"jsonrpc":"2.0","id":5,"method":"missing"}).to_string(),
    ];
    let output = Command::cargo_bin("hyperliquid")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_DATA_HOME", home.path())
        .env("HYPERLIQUID_NO_UPDATE_CHECK", "1")
        .env_remove("HYPERLIQUID_MCP_ALLOW_UNSAFE")
        .arg("mcp")
        .write_stdin(messages.join("\n") + "\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let responses: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 6);
    assert_eq!(responses[0]["id"], Value::Null);
    assert!(responses[0].get("id").is_some());
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(
        responses[1]["result"]["serverInfo"]["name"],
        "hyperliquid-mcp"
    );
    let tools = responses[2]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "schema"));
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"] == "mcp" || tool["name"] == "orders_create")
    );
    assert_eq!(responses[3]["result"]["isError"], false);
    assert!(
        responses[3]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("feedback")
    );
    assert_eq!(responses[4]["error"]["code"], -32602);
    assert_eq!(responses[5]["error"]["code"], -32601);
}
