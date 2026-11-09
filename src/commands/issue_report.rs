//! The pre-filled GitHub issue that `feedback` and `report-incident` print and,
//! on request, open in the browser.

use serde::Serialize;
use serde_json::Value;

use super::support::{REPORT_REPO, open_in_browser, urlencoding};
use crate::output::{TableData, serialize_row};

#[derive(Debug, Clone, Serialize)]
pub struct IssueReport {
    pub url: String,
    pub title: String,
    pub body: String,
    pub opened_in_browser: bool,
}

impl IssueReport {
    /// A new-issue URL carrying `title`, `body` and the comma-separated
    /// `labels`; opened in the browser when `open` is set.
    #[must_use]
    pub fn new(title: String, body: String, labels: &str, open: bool) -> Self {
        let url = format!(
            "https://github.com/{REPORT_REPO}/issues/new?title={}&body={}&labels={}",
            urlencoding(&title),
            urlencoding(&body),
            urlencoding(labels),
        );
        let opened_in_browser = open && open_in_browser(&url);
        Self {
            url,
            title,
            body,
            opened_in_browser,
        }
    }
}

/// The CLI version and platform lines every report body carries.
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

    #[test]
    fn the_url_encodes_title_body_and_labels() {
        let report = IssueReport::new(
            "Feedback [ux]: a&b".into(),
            "line one\nline two".into(),
            "feedback,ux",
            false,
        );
        assert_eq!(
            report.url,
            "https://github.com/hypurr-inc/hyperliquid-cli/issues/new?title=Feedback+%5Bux%5D%3A+a%26b&body=line+one%0Aline+two&labels=feedback%2Cux"
        );
        assert!(!report.opened_in_browser);
    }

    #[test]
    fn environment_lines_name_version_and_platform() {
        let lines = environment_lines();
        assert!(lines.starts_with("- **CLI Version**: `"));
        assert!(lines.contains("- **Platform**: `"));
    }
}
