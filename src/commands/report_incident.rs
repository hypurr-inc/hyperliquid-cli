use clap::Args;

use super::issue_report::{IssueReport, environment_lines};
use crate::command_spec::{CliCommand, CommandSpec, InputKind, arg};
use crate::invocation::Invocation;
use crate::report::LastError;

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

pub struct ReportIncident;

impl CliCommand for ReportIncident {
    type Args = ReportIncidentArgs;

    const SPEC: CommandSpec = CommandSpec::local_tool(
        &["report-incident"],
        "system",
        "Generate or submit an incident bug report to GitHub",
        "Generates a pre-filled GitHub issue URL with CLI version, environment details, and error diagnostics. If not provided, inspects the last error recorded by the CLI. Agents can inspect the issue URL and markdown body directly from JSON.",
    )
    .args(&[
        arg("command").kind(InputKind::Identifier),
        arg("error").kind(InputKind::Identifier),
        arg("title").kind(InputKind::Identifier),
        arg("context").kind(InputKind::Identifier),
    ]);

    async fn run(inv: &Invocation, args: &ReportIncidentArgs) -> Result<(), anyhow::Error> {
        let last_error = LastError::load(inv.settings().env());

        let cmd_str = args
            .command
            .clone()
            .or_else(|| last_error.as_ref().and_then(|last| last.command.clone()))
            .unwrap_or_else(|| "unknown command".to_string());

        let error_str = args
            .error
            .clone()
            .or_else(|| last_error.as_ref().map(LastError::summary))
            .unwrap_or_else(|| "unspecified error".to_string());

        let default_title = format!("Incident report: {cmd_str} failed");
        let title = args.title.clone().unwrap_or(default_title);

        let context_str = args.context.clone().unwrap_or_else(|| {
            last_error.as_ref().map_or_else(
                || "N/A".to_string(),
                |last| {
                    serde_json::to_string_pretty(&last.classification())
                        .unwrap_or_else(|_| "N/A".to_string())
                },
            )
        });

        let body = format!(
            "### Incident Report\n\n\
            {}\
            - **Failing Command**: `{cmd_str}`\n\
            - **Error**: `{error_str}`\n\n\
            ### Context & Diagnostics\n\
            ```json\n{context_str}\n```\n\n\
            *Generated automatically by hyperliquid report-incident*",
            environment_lines()
        );

        inv.print(&IssueReport::new(
            title,
            body,
            "bug,agent-incident",
            args.open,
            inv.mode(),
        ));
        Ok(())
    }
}
