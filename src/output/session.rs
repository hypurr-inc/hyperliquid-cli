//! The output session: how this run prints, decided once at startup.
//!
//! One value carries the resolved format, the JSON projection flags, the
//! terminal facts, and the start instant that the "Completed in" footer
//! measures from. Renderers called from tests or from a second session never
//! see each other's format, projection or terminal facts. Styling is the one
//! exception: [`colors`] resolves `--no-color`, `NO_COLOR` and `FORCE_COLOR`
//! once per process, so `colors::init` must run before the first render.

use std::time::{Duration, Instant};

use super::terminal::OutputEnv;
use super::{
    JsonRenderOptions, OutputFormat, Rendered, TableData, colors, format, json_text, render_outcome,
};

#[derive(Debug, Clone)]
pub struct Session {
    format: OutputFormat,
    json: JsonRenderOptions,
    env: OutputEnv,
    started: Instant,
}

impl Session {
    /// A session that starts timing now.
    #[must_use]
    pub fn new(format: OutputFormat, json: JsonRenderOptions, env: OutputEnv) -> Self {
        Self {
            format,
            json,
            env,
            started: Instant::now(),
        }
    }

    /// A non-terminal session with no projection, for rendering assertions.
    #[cfg(test)]
    pub(crate) fn for_test(format: OutputFormat) -> Self {
        Self::new(format, JsonRenderOptions::default(), OutputEnv::default())
    }

    #[must_use]
    pub fn format(&self) -> OutputFormat {
        self.format
    }

    #[must_use]
    pub fn json_options(&self) -> &JsonRenderOptions {
        &self.json
    }

    #[must_use]
    pub fn env(&self) -> OutputEnv {
        self.env
    }

    /// `data` as it would print to stdout in this session's format.
    #[must_use]
    pub fn render(&self, data: &dyn TableData) -> String {
        self.render_as(data, self.format)
    }

    /// `data` as it would print to stdout in `format`, with this session's projection.
    #[must_use]
    pub fn render_as(&self, data: &dyn TableData, format: OutputFormat) -> String {
        render_outcome(data, format, &self.json, self.env.stdout_tty).text
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
            self.env.stdout_tty,
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

    #[test]
    fn format_timing_is_compact() {
        assert_eq!(
            format_timing(Duration::from_millis(1234)),
            "Completed in 1.2s"
        );
        assert_eq!(
            format_timing(Duration::from_millis(42)),
            "Completed in 42ms"
        );
    }

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

    #[test]
    fn json_line_is_compact_and_projected() {
        let session = Session::new(
            OutputFormat::Json,
            JsonRenderOptions::from_cli(Some("name"), false, None),
            OutputEnv::default(),
        );
        assert_eq!(session.render_json_line(&Rows), r#"{"name":"BTC"}"#);
    }

    #[test]
    fn sessions_do_not_share_projection() {
        let selecting = Session::new(
            OutputFormat::Json,
            JsonRenderOptions::from_cli(Some("name"), false, None),
            OutputEnv::default(),
        );
        let plain = Session::new(
            OutputFormat::Json,
            JsonRenderOptions::default(),
            OutputEnv::default(),
        );
        assert_eq!(selecting.render(&Rows), r#"{"name":"BTC"}"#);
        assert_eq!(plain.render(&Rows), r#"{"name":"BTC","price":"1"}"#);
    }
}
