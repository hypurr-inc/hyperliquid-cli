//! The pre-filled GitHub issue that `feedback` and `report-incident` print and,
//! on request, open in the browser.

use serde::Serialize;
use serde_json::Value;

use crate::mode::Mode;
use crate::output::{TableData, serialize_row};
use crate::update::REPO;

#[derive(Debug, Clone, Serialize)]
pub struct IssueReport {
    pub url: String,
    pub title: String,
    pub body: String,
    pub opened_in_browser: bool,
}

impl IssueReport {
    /// `--open` (`open_requested`) opens the browser only for a person at a terminal: an
    /// agent, CI or a JSON run only prints the URL.
    #[must_use]
    pub fn new(
        title: String,
        body: String,
        labels: &str,
        open_requested: bool,
        mode: &Mode,
    ) -> Self {
        Self::build(title, body, labels, open_requested, mode, open_in_browser)
    }

    fn build(
        title: String,
        body: String,
        labels: &str,
        open_requested: bool,
        mode: &Mode,
        open: impl FnOnce(&str) -> bool,
    ) -> Self {
        let url = format!(
            "https://github.com/{REPO}/issues/new?title={}&body={}&labels={}",
            urlencoding(&title),
            urlencoding(&body),
            urlencoding(labels),
        );
        let opened_in_browser = open_requested && mode.person_at_terminal() && open(&url);
        Self {
            url,
            title,
            body,
            opened_in_browser,
        }
    }
}

fn urlencoding(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn browser_command(url: &str) -> Option<std::process::Command> {
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        Some(command)
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        Some(command)
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url]);
        Some(command)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        None
    }
}

fn succeeds(mut command: std::process::Command) -> bool {
    command.status().is_ok_and(|status| status.success())
}

fn open_in_browser(url: &str) -> bool {
    browser_command(url).is_some_and(succeeds)
}

#[must_use]
pub fn environment_lines() -> String {
    format!(
        "- **CLI Version**: `{}`\n- **Platform**: `{}/{}`\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

impl TableData for IssueReport {
    fn headers(&self) -> Vec<&str> {
        vec!["Title", "URL"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.title.clone(), self.url.clone()]]
    }

    fn to_json_value(&self) -> Value {
        serialize_row(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::OutputFormat;
    use crate::terminal::Terminal;

    fn person_at_terminal() -> Mode {
        Mode {
            terminal: Terminal {
                stdin: true,
                stdout: true,
                stderr: true,
            },
            ..Mode::default()
        }
    }

    fn report(open_requested: bool, mode: &Mode, opener: impl FnOnce(&str) -> bool) -> IssueReport {
        IssueReport::build("t".into(), "b".into(), "l", open_requested, mode, opener)
    }

    #[test]
    fn the_url_encodes_title_body_and_labels() {
        let report = IssueReport::new(
            "Feedback [ux]: a&b".into(),
            "line one\nline two".into(),
            "feedback,ux",
            false,
            &Mode::default(),
        );
        assert_eq!(
            report.url,
            "https://github.com/hypurr-inc/hyperliquid-cli/issues/new?title=Feedback+%5Bux%5D%3A+a%26b&body=line+one%0Aline+two&labels=feedback%2Cux"
        );
        assert!(!report.opened_in_browser);
    }

    #[test]
    fn open_launches_the_browser_for_a_person_at_a_terminal_only() {
        let mut opened = Vec::new();
        let shown = report(true, &person_at_terminal(), |url| {
            opened.push(url.to_string());
            true
        });
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0], shown.url);
        assert!(shown.opened_in_browser);

        let at = person_at_terminal();
        for (name, mode) in [
            ("agent", Mode { agent: true, ..at }),
            ("ci", Mode { ci: true, ..at }),
            (
                "json",
                Mode {
                    format: OutputFormat::Json,
                    ..at
                },
            ),
            (
                "stdout piped",
                Mode {
                    terminal: Terminal {
                        stdout: false,
                        ..at.terminal
                    },
                    ..at
                },
            ),
        ] {
            let skipped = report(true, &mode, |_| {
                panic!("{name}: the browser must stay closed")
            });
            assert!(!skipped.opened_in_browser, "{name}");
        }
        let not_asked = report(false, &at, |_| panic!("--open was not passed"));
        assert!(!not_asked.opened_in_browser);
    }

    #[test]
    fn a_browser_that_did_not_open_is_not_reported_as_opened() {
        let failed = report(true, &person_at_terminal(), |_| false);
        assert!(!failed.opened_in_browser);
    }

    #[cfg(unix)]
    #[test]
    fn only_a_zero_exit_counts_as_success() {
        use std::process::Command;
        assert!(succeeds(Command::new("true")));
        assert!(!succeeds(Command::new("false")));
        assert!(!succeeds(Command::new("/nonexistent/opener")));
    }
}
