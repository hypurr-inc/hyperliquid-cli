//! Reporting a failed run: print the error in the run's format, record it for
//! `report-incident`, and exit with its code.
//!
//! [`crate::errors`] is the error data; this module is what reaches for the
//! terminal's styling, the config directory and private file writes, so it
//! sits above them.

mod last_error;

use crate::env::EnvSource;
use crate::errors::{CliError, ErrorEnvelope, StdoutDocument};
use crate::output::{colors, sink};
use crate::terminal::OutputFormat;

pub use last_error::LastError;

/// Render a human-readable error block (no trailing newline).
///
/// Rich output starts with a red `✗`; plain output starts with `Error:`.
/// Detail lines are indented two spaces. A recovery block follows after a blank
/// line: the instruction, each command indented four spaces, then any note.
fn render_human_error(err: &CliError, rich: bool) -> String {
    let headline = err.to_string();
    let mut out = if rich {
        format!("{} {headline}", colors::paint(colors::RED, "✗"))
    } else {
        format!("Error: {headline}")
    };
    if let Some(detail) = err.detail() {
        out.push_str(&format!("\n  {detail}"));
    }
    if let Some(recovery) = err.recovery() {
        out.push_str(&format!("\n\n{}", recovery.instruction));
        for command in recovery.commands {
            if let Some(when) = command.when {
                out.push_str(&format!("\n{when}:"));
            }
            out.push_str(&format!("\n    {}", command.command));
        }
        if let Some(note) = recovery.note {
            out.push_str(&format!("\n{note}"));
        }
    }
    out
}

/// Print the error envelope to **stdout**, unless the command owns that
/// document ([`StdoutDocument::CommandResult`]): it already wrote its own
/// single JSON document carrying the envelope keys.
fn print_json_error(err: &CliError) {
    match err.stdout_document() {
        StdoutDocument::Envelope => sink::out_line(&ErrorEnvelope::from_cli_error(err).to_json()),
        StdoutDocument::CommandResult => {}
    }
}

/// Print the human error block to **stderr** (`✗ ...` when stderr supports
/// styling, `Error: ...` otherwise).
fn print_human_error(err: &CliError) {
    sink::err_line(&render_human_error(err, colors::stderr_enabled()));
}

/// Record the error for `report-incident` (with the failing `command` path,
/// such as `orders create`, when one was selected), print it in `format` (the
/// JSON envelope on stdout, or a human block on stderr), and terminate the
/// process with its exit code.
pub fn exit_with_error(
    err: CliError,
    format: OutputFormat,
    command: Option<&str>,
    env: &EnvSource,
) -> ! {
    last_error::record(&err, command, env);
    match format {
        OutputFormat::Json => print_json_error(&err),
        OutputFormat::Pretty | OutputFormat::Table => print_human_error(&err),
    }
    std::process::exit(err.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_uses_error_prefix() {
        let text = render_human_error(&CliError::rate_limited(), false);
        assert_eq!(
            text,
            "Error: Too many requests to the Hyperliquid API. Try again shortly."
        );
    }

    #[test]
    fn rich_uses_red_glyph() {
        let text = render_human_error(&CliError::rate_limited(), true);
        assert!(
            text.starts_with("\u{1b}[31m✗\u{1b}[0m Too many requests"),
            "{text}"
        );
        assert!(!text.contains("Error:"));
    }

    #[test]
    fn recovery_block_for_confirmation_required() {
        let err = CliError::ConfirmationRequired {
            headline: "Can't run `orders create` without confirmation.".into(),
            preview: Some("hyperliquid orders create --dry-run".into()),
            approve: Some("hyperliquid orders create --yes".into()),
        };
        assert_eq!(
            render_human_error(&err, false),
            "Error: Can't run `orders create` without confirmation.\n\nPreview it:\n    hyperliquid orders create --dry-run\nOnly after a human approves:\n    hyperliquid orders create --yes"
        );
    }

    #[test]
    fn status_goes_in_a_detail_line() {
        let err = CliError::api_status(reqwest::StatusCode::SERVICE_UNAVAILABLE, "");
        assert_eq!(
            render_human_error(&err, false),
            "Error: The Hyperliquid API failed to process the request.\n  HTTP 503 Service Unavailable"
        );
    }
}
