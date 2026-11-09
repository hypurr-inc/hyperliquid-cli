//! Stdio Model Context Protocol (MCP) server for hyperliquid.
//!
//! Defaults to commands with read-only lifecycle and no risk. Set
//! HYPERLIQUID_MCP_ALLOW_UNSAFE=1 to expose other commands (except mcp).
//! Confirmation bypass arguments remain forbidden. HYPERLIQUID_MCP_TIMEOUT_MS
//! sets a positive execution deadline in milliseconds (default 30000).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

use crate::command_registry::{ArgType, CommandContract, CommandRegistry};
use crate::command_spec::YES_ARG;
use crate::commands::schema::schema_query;
use crate::output::TableData;
use crate::settings::{EnvSource, env};

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

pub fn run_stdio_server(source: &EnvSource) -> Result<(), anyhow::Error> {
    let allow_unsafe = crate::output::terminal::env_flag(source, env::MCP_ALLOW_UNSAFE)
        == crate::output::terminal::EnvFlag::On;
    let exe = std::env::current_exe()?;
    let deadline = source
        .get(env::MCP_TIMEOUT_MS)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(30));
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

        if req.id.is_none() {
            continue;
        }
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
                    .filter(|cmd| exposed(cmd, allow_unsafe))
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
                    name == tool_name && exposed(cmd, allow_unsafe)
                });

                match matched {
                    Some(cmd) => {
                        let cli_args = match command_args(cmd, &args_obj) {
                            Ok(args) => args,
                            Err(message) => {
                                let resp = JsonRpcResponse {
                                    jsonrpc: "2.0",
                                    id,
                                    result: None,
                                    error: Some(JsonRpcError {
                                        code: -32602,
                                        message,
                                        data: None,
                                    }),
                                };
                                writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
                                stdout.flush()?;
                                continue;
                            }
                        };
                        let output = execute(&exe, &cli_args, deadline);

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
                            code: -32602,
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

// Keep discovery and invocation behind the same policy.
fn exposed(cmd: &CommandContract, allow_unsafe: bool) -> bool {
    cmd.command_path != ["mcp"] && (allow_unsafe || cmd.spec.is_public_read())
}

fn command_args(cmd: &CommandContract, args: &Value) -> Result<Vec<String>, String> {
    let map = args.as_object().ok_or("arguments must be an object")?;
    for key in map.keys() {
        if key == YES_ARG {
            return Err("confirmation bypass arguments are not allowed".into());
        }
        if !cmd.inputs.iter().any(|input| input.id == *key) {
            return Err(format!("Unknown argument: {key}"));
        }
    }
    let mut result = vec!["--format".into(), "json".into()];
    result.extend(cmd.command_path.iter().cloned());
    let mut inputs = cmd.inputs.iter().collect::<Vec<_>>();
    inputs.sort_by_key(|input| input.positional_index.unwrap_or(usize::MAX));
    for input in inputs {
        let Some(value) = map.get(&input.id).filter(|v| !v.is_null()) else {
            continue;
        };
        if input.arg_type == ArgType::Boolean {
            let enabled = value.as_bool().ok_or("flag must be boolean")?;
            if enabled {
                result.push(format!(
                    "--{}",
                    input.long.as_deref().ok_or("flag has no long name")?
                ));
            }
            continue;
        }
        let values = if input.multiple {
            value
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(value))
        } else {
            std::slice::from_ref(value)
        };
        for value in values {
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            if input.positional_index.is_some() {
                // Prevent positional values from being interpreted as CLI options.
                if text.starts_with('-') {
                    return Err("positional values cannot start with '-'".into());
                }
            } else {
                // Equals form keeps even values beginning with '-' attached to their option.
                result.push(format!(
                    "--{}={text}",
                    input.long.as_deref().ok_or("option has no long name")?
                ));
                continue;
            }
            result.push(text);
        }
    }
    Ok(result)
}

fn execute(
    exe: &std::path::Path,
    args: &[String],
    deadline: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut b = Vec::new();
        stdout.read_to_end(&mut b).map(|_| b)
    });
    let err = std::thread::spawn(move || {
        let mut b = Vec::new();
        stderr.read_to_end(&mut b).map(|_| b)
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                break match outcome {
                    Err(e) => Err(e),
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "MCP execution deadline exceeded",
                    )),
                };
            }
        }
    };
    let stdout = out
        .join()
        .map_err(|_| std::io::Error::other("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| std::io::Error::other("stderr reader panicked"))??;
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(path: &[&str]) -> &'static CommandContract {
        CommandRegistry::get().find_path(path).unwrap()
    }

    #[test]
    fn positional_and_repeated_arguments_follow_contract() {
        let args = command_args(
            contract(&["schema"]),
            &json!({"command_path":["orders","create"]}),
        )
        .unwrap();
        assert_eq!(args, ["--format", "json", "schema", "orders", "create"]);
        let cmd = CommandRegistry::get()
            .commands()
            .iter()
            .find(|cmd| {
                cmd.inputs
                    .iter()
                    .any(|input| input.id == "coin" && input.positional_index.is_some())
            })
            .unwrap();
        let args = command_args(cmd, &json!({"coin":"BTC"})).unwrap();
        assert_eq!(args.last().unwrap(), "BTC");
        assert!(!args.iter().any(|s| s == "--coin"));
    }

    #[test]
    fn json_options_and_confirmation_boundary() {
        let cmd = contract(&["report-incident"]);
        let args = command_args(cmd, &json!({"context":{"a":true}})).unwrap();
        assert!(args.iter().any(|s| s == "--context={\"a\":true}"));
        let args = command_args(cmd, &json!({"context":false})).unwrap();
        assert!(args.iter().any(|s| s == "--context=false"));
        assert!(command_args(cmd, &json!({"yes":true})).is_err());
        assert!(command_args(cmd, &json!({"unknown":true})).is_err());
        assert!(command_args(cmd, &json!([])).is_err());
    }

    #[test]
    fn default_exposure_is_public_reads_only() {
        for cmd in CommandRegistry::get().commands() {
            assert_eq!(exposed(cmd, true), cmd.command_path != ["mcp"]);
            assert_eq!(
                exposed(cmd, false),
                cmd.command_path != ["mcp"] && cmd.spec.is_public_read()
            );
            if exposed(cmd, false) {
                assert!(!cmd.spec.changes_state(), "{}", cmd.command_key());
                assert!(!cmd.dangerous, "{}", cmd.command_key());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn execution_drains_pipes_and_reaps_at_deadline() {
        let args = vec![
            "-c".into(),
            "while :; do echo output; echo error >&2; done".into(),
        ];
        let start = std::time::Instant::now();
        let error = execute(
            std::path::Path::new("/bin/sh"),
            &args,
            std::time::Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }
}
