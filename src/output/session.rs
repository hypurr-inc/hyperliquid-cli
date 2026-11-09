//! The output session: how this run prints, decided once at startup.
//!
//! One value carries the resolved format, the JSON projection flags, whether
//! stdout is a terminal (the layout), and the start instant that the
//! "Completed in" footer measures from. The format and the terminal come from
//! the run's [`Mode`], so the two cannot disagree. Renderers called from tests
//! or from a second session never see each other's format or projection.
//! Styling is the one exception: [`colors`] is process-wide.

use std::time::{Duration, Instant};

use super::{
    JsonRenderOptions, OutputFormat, Rendered, TableData, colors, format, json_text, render_outcome,
};
use crate::mode::Mode;

#[derive(Debug, Clone)]
pub struct Session {
    format: OutputFormat,
    json: JsonRenderOptions,
    stdout_tty: bool,
    started: Instant,
}

impl Session {
    #[must_use]
    pub fn new(json: JsonRenderOptions, mode: &Mode) -> Self {
        Self {
            format: mode.format,
            json,
            stdout_tty: mode.terminal.stdout,
            started: Instant::now(),
        }
    }

    /// A non-terminal session with no projection, for rendering assertions.
    #[cfg(test)]
    pub(crate) fn for_test(format: OutputFormat) -> Self {
        Self::new(
            JsonRenderOptions::default(),
            &Mode {
                format,
                ..Mode::default()
            },
        )
    }

    #[must_use]
    pub fn format(&self) -> OutputFormat {
        self.format
    }

    #[must_use]
    pub fn json_options(&self) -> &JsonRenderOptions {
        &self.json
    }

    /// `data` as it would print to stdout in this session's format.
    #[must_use]
    pub fn render(&self, data: &dyn TableData) -> String {
        self.render_as(data, self.format)
    }

    /// `data` as it would print to stdout in `format`, with this session's projection.
    #[must_use]
    pub fn render_as(&self, data: &dyn TableData, format: OutputFormat) -> String {
        render_outcome(data, format, &self.json, self.stdout_tty).text
    }

    /// `data` as one compact JSON line with this session's projection, for NDJSON streams.
    #[must_use]
    pub fn render_json_line(&self, data: &dyn TableData) -> String {
        json_text(&self.json.apply(data.to_json_value()), false)
    }

    /// The gray "Completed in" line for the time since the session started.
    #[must_use]
    pub fn timing_footer(&self) -> String {
        colors::stderr::gray(&format_timing(self.started.elapsed()))
    }

    /// Print the answer to stdout; empty-state and truncation notices go to stderr.
    pub fn print(&self, data: &dyn TableData) {
        print_rendered(&render_outcome(
            data,
            self.format,
            &self.json,
            self.stdout_tty,
        ));
    }
}

/// Write rendered output: the answer to stdout, notices to stderr.
fn print_rendered(rendered: &Rendered) {
    if !rendered.text.is_empty() {
        println!("{}", rendered.text);
    }
    for notice in &rendered.notices {
        eprintln!("{notice}");
    }
}

fn format_timing(duration: Duration) -> String {
    format!("Completed in {}", format::duration(duration))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rows;
    impl TableData for Rows {
        fn headers(&self) -> Vec<&str> {
            vec!["Name"]
        }
        fn rows(&self) -> Vec<Vec<String>> {
            vec![vec!["BTC".to_string()]]
        }
        fn to_json_value(&self) -> serde_json::Value {
            serde_json::json!({"name": "BTC", "price": "1"})
        }
    }

    fn json_mode() -> Mode {
        Mode {
            format: OutputFormat::Json,
            ..Mode::default()
        }
    }

    #[test]
    fn the_session_takes_its_format_and_layout_from_the_mode() {
        let mode = Mode {
            format: OutputFormat::Table,
            terminal: crate::output::terminal::Terminal {
                stdout: true,
                ..Default::default()
            },
            ..Mode::default()
        };
        let session = Session::new(JsonRenderOptions::default(), &mode);
        assert_eq!(session.format(), OutputFormat::Table);
        assert!(
            session.render(&Rows).contains('╭'),
            "a terminal stdout draws the bordered table"
        );
        let piped = Session::new(
            JsonRenderOptions::default(),
            &Mode {
                terminal: crate::output::terminal::Terminal::default(),
                ..mode
            },
        );
        assert_eq!(piped.render(&Rows), "Name\nBTC");
    }

    #[test]
    fn json_line_is_compact_and_projected() {
        let session = Session::new(
            JsonRenderOptions::from_cli(Some("name"), false, None),
            &json_mode(),
        );
        assert_eq!(session.render_json_line(&Rows), r#"{"name":"BTC"}"#);
    }

    #[test]
    fn sessions_do_not_share_projection() {
        let selecting = Session::new(
            JsonRenderOptions::from_cli(Some("name"), false, None),
            &json_mode(),
        );
        let plain = Session::new(JsonRenderOptions::default(), &json_mode());
        assert_eq!(selecting.render(&Rows), r#"{"name":"BTC"}"#);
        assert_eq!(plain.render(&Rows), r#"{"name":"BTC","price":"1"}"#);
    }
}
