//! Stdio Model Context Protocol (MCP) server for hyperliquid.
//!
//! Provides standard JSON-RPC 2.0 stdio transport exposing all CLI commands as tools.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

use crate::command_registry::CommandRegistry;
use crate::commands::schema::schema_query;
use crate::output::TableData;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

pub fn run_stdio_server() -> Result<(), anyhow::Error> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {e}"),
                        data: None,
                    }),
                };
                let _ = writeln!(stdout, "{}", serde_json::to_string(&resp)?);
                let _ = stdout.flush();
                continue;
            }
        };

        let id = req.id.clone();
        let resp = match req.method.as_str() {
            "initialize" => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {
                        "name": "hyperliquid-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "tools": { "listChanged": false }
                    }
                })),
                error: None,
            },
            "notifications/initialized" | "initialized" => {
                // Notifications have no response
                continue;
            }
            "ping" => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(json!({})),
                error: None,
            },
            "tools/list" => {
                let registry = CommandRegistry::get();
                let tools = registry
                    .commands()
                    .iter()
                    .map(|cmd| {
                        let tool_name = cmd.command_path.join("_").replace('-', "_");
                        let schema = schema_query(&cmd.command_path)
                            .ok()
                            .map(|s| s.to_json_value())
                            .unwrap_or(Value::Null);

                        let input_schema = schema
                            .get("json_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));

                        json!({
                            "name": tool_name,
                            "description": cmd.description,
                            "inputSchema": input_schema
                        })
                    })
                    .collect::<Vec<_>>();

                JsonRpcResponse {
                    jsonrpc: "2.0",
                    id,
                    result: Some(json!({ "tools": tools })),
                    error: None,
                }
            }
            "tools/call" => {
                let tool_name = req.params.get("name").and_then(Value::as_str).unwrap_or("");
                let args_obj = req.params.get("arguments").cloned().unwrap_or(json!({}));

                let registry = CommandRegistry::get();
                let matched = registry.commands().iter().find(|cmd| {
                    let name = cmd.command_path.join("_").replace('-', "_");
                    name == tool_name
                });

                match matched {
                    Some(cmd) => {
                        let mut cli_args = vec!["--format".to_string(), "json".to_string()];
                        cli_args.extend(cmd.command_path.iter().cloned());

                        if let Value::Object(map) = args_obj {
                            for (k, v) in map {
                                let flag = format!("--{}", k.replace('_', "-"));
                                match v {
                                    Value::Bool(true) => {
                                        cli_args.push(flag);
                                    }
                                    Value::Bool(false) => {}
                                    Value::String(s) => {
                                        cli_args.push(flag);
                                        cli_args.push(s);
                                    }
                                    Value::Number(n) => {
                                        cli_args.push(flag);
                                        cli_args.push(n.to_string());
                                    }
                                    Value::Array(_) | Value::Object(_) => {
                                        cli_args.push(flag);
                                        cli_args.push(v.to_string());
                                    }
                                    Value::Null => {}
                                }
                            }
                        }

                        let exe = std::env::current_exe().unwrap_or_else(|_| "hyperliquid".into());
                        let output = std::process::Command::new(exe).args(&cli_args).output();

                        match output {
                            Ok(out) => {
                                let stdout_str = String::from_utf8_lossy(&out.stdout).to_string();
                                let stderr_str = String::from_utf8_lossy(&out.stderr).to_string();
                                let is_error = !out.status.success();
                                let content_text = if stdout_str.trim().is_empty() {
                                    stderr_str
                                } else {
                                    stdout_str
                                };

                                JsonRpcResponse {
                                    jsonrpc: "2.0",
                                    id,
                                    result: Some(json!({
                                        "content": [
                                            {
                                                "type": "text",
                                                "text": content_text
                                            }
                                        ],
                                        "isError": is_error
                                    })),
                                    error: None,
                                }
                            }
                            Err(e) => JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: Some(json!({
                                    "content": [{ "type": "text", "text": format!("Execution error: {e}") }],
                                    "isError": true
                                })),
                                error: None,
                            },
                        }
                    }
                    None => JsonRpcResponse {
                        jsonrpc: "2.0",
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32601,
                            message: format!("Unknown tool: {tool_name}"),
                            data: None,
                        }),
                    },
                }
            }
            other => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {other}"),
                    data: None,
                }),
            },
        };

        let _ = writeln!(stdout, "{}", serde_json::to_string(&resp)?);
        let _ = stdout.flush();
    }

    Ok(())
}
