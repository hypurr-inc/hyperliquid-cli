//! Stdio Model Context Protocol (MCP) server for hyperliquid.
//!
//! Defaults to commands with read-only lifecycle and no risk. Set
//! HYPERLIQUID_MCP_ALLOW_UNSAFE=1 to expose other commands (except mcp).
//! Confirmation bypass arguments remain forbidden. HYPERLIQUID_MCP_TIMEOUT_MS
//! sets a positive execution deadline in milliseconds ([`DEFAULT_DEADLINE`] when unset). A value
//! of either variable the server cannot read is a configuration error (exit
//! 2), never a silent default.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::time::Duration;

use crate::clock;
use crate::command_registry::{ArgType, CommandContract, CommandRegistry};
use crate::command_spec::YES_ARG;
use crate::env::{self, EnvFlag, EnvSource};
use crate::errors::CliError;
use crate::output::sink;

pub(crate) const DEFAULT_DEADLINE: Duration = clock::SECOND.of(30);

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

impl JsonRpcResponse {
    fn result(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// A tool call's outcome: text content, flagged when the call failed.
    fn tool_text(id: Option<Value>, text: String, is_error: bool) -> Self {
        Self::result(
            id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error
            }),
        )
    }
}

/// What the operator configured for this server, read once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct McpSettings {
    pub(crate) allow_unsafe: bool,
    pub(crate) deadline: Duration,
}

impl McpSettings {
    fn from_env(source: &EnvSource) -> Result<Self, CliError> {
        Ok(Self {
            // An unreadable value never gets here: `Mode::resolve` refuses it at startup.
            allow_unsafe: env::flag(source, env::MCP_ALLOW_UNSAFE) == EnvFlag::On,
            deadline: match source.get(env::MCP_TIMEOUT_MS) {
                None => DEFAULT_DEADLINE,
                Some(raw) => parse_deadline(&raw)?,
            },
        })
    }
}

fn parse_deadline(raw: &str) -> Result<Duration, CliError> {
    let invalid = |why: String| {
        CliError::Configuration(format!(
            "{} must be a positive number of milliseconds: {why}",
            env::MCP_TIMEOUT_MS
        ))
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => Err(invalid("0 would end every call at once".to_string())),
        Ok(millis) => Ok(Duration::from_millis(millis)),
        Err(err) => Err(invalid(err.to_string())),
    }
}

/// Write one response line. `Ok(false)` when the client closed its end, which
/// ends the server; any other write failure is an error.
fn write_response(out: &mut impl Write, response: &JsonRpcResponse) -> Result<bool, anyhow::Error> {
    let line = serde_json::to_string(response)?;
    match sink::write_line(out, &line) {
        Ok(()) => Ok(true),
        Err(err) if sink::is_stdout_closed(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

pub fn run_stdio_server(
    source: &EnvSource,
    registry: &CommandRegistry,
) -> Result<(), anyhow::Error> {
    let settings = McpSettings::from_env(source)?;
    let exe = std::env::current_exe()?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => handle(&request, registry, &settings, &exe),
            Err(err) => Some(JsonRpcResponse::error(
                None,
                -32700,
                format!("Parse error: {err}"),
            )),
        };
        if let Some(response) = response
            && !write_response(&mut stdout, &response)?
        {
            break;
        }
    }

    Ok(())
}

/// The reply to one request, `None` for a notification.
fn handle(
    request: &JsonRpcRequest,
    registry: &CommandRegistry,
    settings: &McpSettings,
    exe: &std::path::Path,
) -> Option<JsonRpcResponse> {
    request.id.as_ref()?;
    let id = request.id.clone();
    Some(match request.method.as_str() {
        "initialize" => JsonRpcResponse::result(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "hyperliquid-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "tools": { "listChanged": false }
                }
            }),
        ),
        "notifications/initialized" | "initialized" => return None,
        "ping" => JsonRpcResponse::result(id, json!({})),
        "tools/list" => {
            let tools = exposed(registry, settings)
                .map(|cmd| {
                    json!({
                        "name": cmd.mcp_tool_name(),
                        "description": cmd.description,
                        "inputSchema": cmd.json_schema()
                    })
                })
                .collect::<Vec<_>>();
            JsonRpcResponse::result(id, json!({ "tools": tools }))
        }
        "tools/call" => call_tool(id, &request.params, registry, settings, exe),
        other => JsonRpcResponse::error(id, -32601, format!("Method not found: {other}")),
    })
}

fn call_tool(
    id: Option<Value>,
    params: &Value,
    registry: &CommandRegistry,
    settings: &McpSettings,
    exe: &std::path::Path,
) -> JsonRpcResponse {
    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args_obj = params.get("arguments").cloned().unwrap_or(json!({}));

    let Some(cmd) = exposed(registry, settings).find(|cmd| cmd.mcp_tool_name() == tool_name) else {
        return JsonRpcResponse::error(id, -32602, format!("Unknown tool: {tool_name}"));
    };
    let cli_args = match command_args(cmd, &args_obj) {
        Ok(args) => args,
        Err(message) => return JsonRpcResponse::error(id, -32602, message),
    };
    match execute(exe, &cli_args, settings.deadline) {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let text = if stdout.trim().is_empty() {
                String::from_utf8_lossy(&out.stderr).to_string()
            } else {
                stdout
            };
            JsonRpcResponse::tool_text(id, text, !out.status.success())
        }
        Err(err) => JsonRpcResponse::tool_text(id, format!("Execution error: {err}"), true),
    }
}

/// The commands this server offers. Discovery and invocation both read this,
/// so they cannot disagree.
pub(crate) fn exposed<'a>(
    registry: &'a CommandRegistry,
    settings: &McpSettings,
) -> impl Iterator<Item = &'a CommandContract> {
    let allow_unsafe = settings.allow_unsafe;
    registry
        .commands()
        .iter()
        .filter(move |cmd| cmd.spec.exposed_to_mcp(allow_unsafe))
}

pub(crate) fn command_args(cmd: &CommandContract, args: &Value) -> Result<Vec<String>, String> {
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

    fn settings_from(pairs: &[(&'static str, &'static str)]) -> Result<McpSettings, CliError> {
        McpSettings::from_env(&EnvSource::fixed(pairs.iter().copied()))
    }

    #[test]
    fn unset_environment_means_safe_defaults() {
        assert_eq!(
            settings_from(&[]).unwrap(),
            McpSettings {
                allow_unsafe: false,
                deadline: DEFAULT_DEADLINE,
            }
        );
    }

    #[test]
    fn the_env_help_states_the_default_deadline() {
        let help = env::TABLE
            .iter()
            .find(|var| var.name == env::MCP_TIMEOUT_MS)
            .unwrap()
            .help;
        assert!(help.contains(&format!("default {})", DEFAULT_DEADLINE.as_millis())));
    }

    #[test]
    fn valid_environment_is_applied() {
        let settings = settings_from(&[
            ("HYPERLIQUID_MCP_ALLOW_UNSAFE", "yes"),
            ("HYPERLIQUID_MCP_TIMEOUT_MS", " 250 "),
        ])
        .unwrap();
        assert!(settings.allow_unsafe);
        assert_eq!(settings.deadline, Duration::from_millis(250));
    }

    #[test]
    fn an_unreadable_environment_is_a_configuration_error_not_a_default() {
        for (name, value) in [
            ("HYPERLIQUID_MCP_TIMEOUT_MS", "soon"),
            ("HYPERLIQUID_MCP_TIMEOUT_MS", "0"),
            ("HYPERLIQUID_MCP_TIMEOUT_MS", "-5"),
            ("HYPERLIQUID_MCP_TIMEOUT_MS", ""),
        ] {
            let err = settings_from(&[(name, value)]).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{name}={value:?}");
            assert!(err.to_string().contains(name), "{err}");
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
