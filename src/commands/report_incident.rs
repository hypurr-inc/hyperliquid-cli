//! `report-incident`: report an error or issue directly to GitHub.

use clap::Args;
use serde::Serialize;
use serde_json::Value;

use super::support::{REPORT_REPO, open_in_browser, urlencoding};
use crate::command_spec::{CliCommand, CommandSpec, DryRunPolicy, InputKind, Lifecycle, Risk, arg};
use crate::invocation::Invocation;
use crate::output::TableData;

#[derive(Args, Debug, Clone)]
pub struct ReportIncidentArgs {
    /// Command that failed or encountered an issue.
    #[arg(long, value_name = "CMD")]
    pub command: Option<String>,

    /// Error message, code, or description.
    #[arg(long, value_name = "ERROR")]
    pub error: Option<String>,

    /// Short title for the issue.
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// Additional details or reproduction context.
    #[arg(long, value_name = "TEXT")]
    pub context: Option<String>,

    /// Open issue in web browser if running in an interactive terminal.
    #[arg(long)]
    pub open: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportIncidentOutput {
    pub url: String,
    pub title: String,
    pub body: String,
    pub opened_in_browser: bool,
}

impl TableData for ReportIncidentOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Title", "URL"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.title.clone(), self.url.clone()]]
    }

    fn to_json_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

pub struct ReportIncident;

impl CliCommand for ReportIncident {
    type Args = ReportIncidentArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["report-incident"],
        "system",
        "Generate or submit an incident bug report to GitHub",
        "Generates a pre-filled GitHub issue URL with CLI version, environment details, and error diagnostics. If not provided, inspects the last error recorded by the CLI. Agents can inspect the issue URL and markdown body directly from JSON.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::None)
    .dry_run(DryRunPolicy::NotSupported)
    .local()
    .args(&[
        arg("command").kind(InputKind::Identifier),
        arg("error").kind(InputKind::Identifier),
        arg("title").kind(InputKind::Identifier),
        arg("context").kind(InputKind::Identifier),
    ]);

    async fn run(inv: &Invocation, args: &ReportIncidentArgs) -> Result<(), anyhow::Error> {
        let version = env!("CARGO_PKG_VERSION");
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;

        let last_error_json: Option<Value> = crate::paths::last_error_file()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|content| serde_json::from_str::<Value>(&content).ok());

        let cmd_str = args
            .command
            .clone()
            .or_else(|| {
                last_error_json
                    .as_ref()
                    .and_then(|j| j.get("data"))
                    .and_then(|d| d.get("command"))
                    .and_then(|c| c.as_str())
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| "unknown command".to_string());

        let error_str = args
            .error
            .clone()
            .or_else(|| {
                last_error_json.as_ref().map(|j| {
                    let category = j
                        .get("category")
                        .and_then(|c| c.as_str())
                        .unwrap_or("unknown");
                    let reason = j
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unspecified");
                    let exit_code = j.get("exit_code").and_then(|e| e.as_i64()).unwrap_or(1);
                    format!("{category} / {reason} (exit {exit_code})")
                })
            })
            .unwrap_or_else(|| "unspecified error".to_string());

        let default_title = format!("Incident report: {cmd_str} failed");
        let title = args.title.clone().unwrap_or(default_title);

        let context_str = args.context.clone().unwrap_or_else(|| {
            if let Some(j) = &last_error_json {
                let mut sanitized = serde_json::Map::new();
                if let Some(v) = j.get("category") {
                    sanitized.insert("category".into(), v.clone());
                }
                if let Some(v) = j.get("reason") {
                    sanitized.insert("reason".into(), v.clone());
                }
                if let Some(v) = j.get("exit_code") {
                    sanitized.insert("exit_code".into(), v.clone());
                }
                if let Some(v) = j.get("retryable") {
                    sanitized.insert("retryable".into(), v.clone());
                }
                if let Some(v) = j.get("data") {
                    sanitized.insert("data".into(), v.clone());
                }
                serde_json::to_string_pretty(&Value::Object(sanitized))
                    .unwrap_or_else(|_| "N/A".to_string())
            } else {
                "N/A".to_string()
            }
        });

        let body = format!(
            "### Incident Report\n\n\
            - **CLI Version**: `{version}`\n\
            - **Platform**: `{os}/{arch}`\n\
            - **Failing Command**: `{cmd_str}`\n\
            - **Error**: `{error_str}`\n\n\
            ### Context & Diagnostics\n\
            ```json\n{context_str}\n```\n\n\
            *Generated automatically by hyperliquid report-incident*"
        );

        let encoded_title = urlencoding(&title);
        let encoded_body = urlencoding(&body);
        let url = format!(
            "https://github.com/{REPORT_REPO}/issues/new?title={encoded_title}&body={encoded_body}&labels=bug,agent-incident"
        );

        let opened = if args.open {
            open_in_browser(&url)
        } else {
            false
        };

        let output = ReportIncidentOutput {
            url,
            title,
            body,
            opened_in_browser: opened,
        };

        inv.print(&output);
        Ok(())
    }
}
