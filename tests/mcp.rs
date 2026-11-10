mod support;

use hyperliquid_cli::env;
use serde_json::{Value, json};
use support::{IsolatedHome, json_stdout};

#[test]
fn stdio_initialize_list_and_call() {
    let home = IsolatedHome::new();
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
    let output = home
        .command()
        .env(env::NO_UPDATE_CHECK, "1")
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
    // The notifications get no reply; the unparseable line gets an error with a null id.
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

#[test]
fn an_unreadable_mcp_environment_exits_2_before_the_server_starts() {
    let home = IsolatedHome::new();
    for (name, value) in [
        (env::MCP_TIMEOUT_MS, "soon"),
        (env::MCP_TIMEOUT_MS, "0"),
        (env::MCP_ALLOW_UNSAFE, "maybe"),
    ] {
        let assert = home
            .command()
            .env(env::NO_UPDATE_CHECK, "1")
            .env(name, value)
            .args(["--format", "json", "mcp"])
            .write_stdin(json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string() + "\n")
            .assert()
            .code(2);
        let envelope = json_stdout(&assert);
        assert_eq!(envelope["reason"], "invalid_config", "{name}={value}");
        assert!(
            envelope["error"].as_str().unwrap().contains(name),
            "{envelope}"
        );
    }
}
