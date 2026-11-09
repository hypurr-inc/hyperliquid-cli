//! The three output formats:
//! - `Pretty`: human-readable, tabwriter-aligned, styled only when stdout styling is enabled (default)
//! - `Table`: bordered tables on a terminal, tab-separated header and rows on a pipe
//! - `Json`: stable JSON output with snake_case keys, indented on a terminal and compact on a pipe
//!
//! Color theme (see [`colors`] for when styling applies):
//! - Cyan: headers
//! - Green: positive values (profit, gains)
//! - Red: negative values (loss, declines)
//! - Gray: muted/secondary text
//! - Yellow: warnings
//!
//! Streams:
//! - stdout carries the answer. Empty-state sentences and `--max-results`
//!   truncation notices go to stderr in pretty and table modes, as [`Notice`]s.
//!
//! Timing feedback:
//! - "Completed in 1.2s" is printed to stderr only for human formats on a terminal

use serde_json::Value;
use std::io::Write;
use std::sync::OnceLock;
use tabwriter::TabWriter;

use crate::response_sanitization::is_format_char;
use crate::terminal::OutputFormat;

mod session;

pub use notice::{Notice, warn};
pub use session::Session;

pub mod colors;
pub mod format;
pub mod notice;

/// `--select`, `--results-only` and `--max-results`, applied to JSON output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsonRenderOptions {
    selected_fields: Option<Vec<String>>,
    results_only: bool,
    max_results: Option<usize>,
}

impl JsonRenderOptions {
    #[must_use]
    pub fn from_cli(select: Option<&str>, results_only: bool, max_results: Option<usize>) -> Self {
        Self {
            selected_fields: select.map(parse_selected_fields),
            results_only,
            max_results,
        }
    }

    #[must_use]
    pub fn max_results(&self) -> Option<usize> {
        self.max_results
    }

    /// The `--select` field names, lowercased, when `--select` was given.
    #[must_use]
    pub fn selected_fields(&self) -> Option<&[String]> {
        self.selected_fields.as_deref()
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.selected_fields.is_some() || self.results_only || self.max_results.is_some()
    }

    #[must_use]
    pub fn apply(&self, value: serde_json::Value) -> serde_json::Value {
        self.project(value, limit_json_results, project_selected_fields)
    }

    /// Project one JSONL stream envelope.
    ///
    /// Stream envelopes keep their top-level `type`, `subscription`, and `data`
    /// keys unless the caller explicitly selects top-level fields. `--max-results`
    /// is therefore applied recursively to nested payloads instead of truncating
    /// the envelope itself.
    #[must_use]
    pub fn apply_stream(&self, value: serde_json::Value) -> serde_json::Value {
        self.project(
            value,
            limit_json_stream_results,
            project_stream_selected_fields,
        )
    }

    /// The shared pipeline: `--results-only`, then `--max-results`, then
    /// `--select`. A document and a stream envelope differ only in how the
    /// last two steps treat nesting.
    fn project(
        &self,
        value: serde_json::Value,
        limit: fn(serde_json::Value, usize) -> serde_json::Value,
        select: fn(serde_json::Value, &[String]) -> serde_json::Value,
    ) -> serde_json::Value {
        let value = if self.results_only {
            strip_results_envelope(value)
        } else {
            value
        };
        let value = match self.max_results {
            Some(max_results) => limit(value, max_results),
            None => value,
        };
        match self.selected_fields.as_deref() {
            Some(fields) => select(value, fields),
            None => value,
        }
    }
}

fn parse_selected_fields(select: &str) -> Vec<String> {
    select
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(|field| field.to_ascii_lowercase())
        .collect()
}

fn limit_json_results(value: serde_json::Value, max_results: usize) -> serde_json::Value {
    match value {
        serde_json::Value::Array(rows) => {
            serde_json::Value::Array(rows.into_iter().take(max_results).collect())
        }
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .take(max_results)
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        ),
        scalar => scalar,
    }
}

fn limit_json_stream_results(value: serde_json::Value, max_results: usize) -> serde_json::Value {
    match value {
        serde_json::Value::Array(rows) => {
            serde_json::Value::Array(rows.into_iter().take(max_results).collect())
        }
        serde_json::Value::Object(object) if is_stream_envelope(&object) => {
            serde_json::Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| (key, limit_json_stream_results(value, max_results)))
                    .collect(),
            )
        }
        serde_json::Value::Object(object) => {
            let is_leaf_object = object.values().all(|value| {
                !matches!(
                    value,
                    serde_json::Value::Array(_) | serde_json::Value::Object(_)
                )
            });
            let entries = object
                .into_iter()
                .map(|(key, value)| (key, limit_json_stream_results(value, max_results)));

            if is_leaf_object {
                serde_json::Value::Object(
                    entries
                        .take(max_results)
                        .collect::<serde_json::Map<String, serde_json::Value>>(),
                )
            } else {
                serde_json::Value::Object(entries.collect())
            }
        }
        scalar => scalar,
    }
}

fn is_stream_envelope(object: &serde_json::Map<String, serde_json::Value>) -> bool {
    object.contains_key("type")
        && (object.contains_key("subscription") || object.contains_key("data"))
}

fn strip_results_envelope(value: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut object) = value else {
        return value;
    };

    for key in ["data", "results", "result"] {
        if let Some(result) = object.remove(key) {
            return result;
        }
    }

    serde_json::Value::Object(object)
}

fn project_selected_fields(value: serde_json::Value, fields: &[String]) -> serde_json::Value {
    match value {
        serde_json::Value::Array(rows) => serde_json::Value::Array(
            rows.into_iter()
                .map(|row| project_selected_fields(row, fields))
                .collect(),
        ),
        serde_json::Value::Object(object) => {
            if object.values().all(serde_json::Value::is_object) {
                serde_json::Value::Object(
                    object
                        .into_iter()
                        .map(|(key, value)| (key, project_selected_fields(value, fields)))
                        .collect(),
                )
            } else {
                serde_json::Value::Object(project_object_fields(&object, fields))
            }
        }
        scalar => scalar,
    }
}

fn project_stream_selected_fields(
    value: serde_json::Value,
    fields: &[String],
) -> serde_json::Value {
    let serde_json::Value::Object(mut object) = value else {
        return project_selected_fields(value, fields);
    };

    let top_level_projection = project_object_fields(&object, fields);
    let nested_projection = object
        .remove("data")
        .map(|data| project_selected_fields(data, fields));

    if !top_level_projection.is_empty() {
        let mut projection = top_level_projection;
        if !projection.contains_key("data")
            && let Some(data) = nested_projection.filter(|data| !data.is_null())
        {
            let include_data = match &data {
                serde_json::Value::Object(map) => !map.is_empty(),
                serde_json::Value::Array(items) => !items.is_empty(),
                _ => true,
            };
            if include_data {
                projection.insert("data".to_string(), data);
            }
        }
        return serde_json::Value::Object(projection);
    }

    if let Some(data) = nested_projection {
        object.insert("data".to_string(), data);
    }
    serde_json::Value::Object(object)
}

fn project_object_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    fields: &[String],
) -> serde_json::Map<String, serde_json::Value> {
    let mut projected = serde_json::Map::new();

    for field in fields {
        if let Some(value) = object.get(field) {
            projected.insert(field.clone(), value.clone());
            continue;
        }

        if let Some((alias, value)) = alias_value_for_field(object, field) {
            projected.insert(alias.to_string(), value.clone());
        }
    }

    projected
}

/// `--select` shorthands: each canonical field name, then the keys that can
/// supply it, most specific first. The first key present answers.
const FIELD_ALIASES: &[(&str, &[&str])] = &[
    ("coin", &["coin", "name", "asset"]),
    (
        "price",
        &[
            "price",
            "limit_price",
            "limit_px",
            "limitPx",
            "mid",
            "mark_price",
            "oracle_price",
        ],
    ),
    ("size", &["size", "sz", "amount"]),
];

fn alias_value_for_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<(&'static str, &'a serde_json::Value)> {
    let (canonical, keys) = FIELD_ALIASES.iter().find(|(name, _)| *name == field)?;
    keys.iter()
        .find_map(|key| object.get(*key))
        .map(|value| (*canonical, value))
}

pub trait TableData {
    fn headers(&self) -> Vec<&str>;

    fn rows(&self) -> Vec<Vec<String>>;

    /// Row data for pretty output.
    ///
    /// Implementors can override this to apply human-facing ANSI styling to
    /// selected cells while keeping [`Self::rows`] plain for table output.
    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.rows()
    }

    fn to_json_value(&self) -> serde_json::Value;

    /// Sentence for stderr when [`Self::rows`] is empty, for example `No orders.`.
    ///
    /// Pretty and table print only the header (on a pipe) or nothing (on a
    /// terminal) to stdout and this message to stderr. Implementors with an
    /// empty state should return no rows and set this instead of a placeholder
    /// row. Defaults to `No results.`.
    fn empty_message(&self) -> Option<&str> {
        None
    }
}

/// The JSON document of a type that derives `Serialize`, for
/// [`TableData::to_json_value`] implementations whose JSON is the type itself.
///
/// Serializing a derived row type fails only when its shape can't be JSON, such
/// as a map with non-string keys. That is a bug in the type, the same for every
/// run, so it panics (the panic hook reports it as a bug) instead of printing
/// `{}` as though the command had returned nothing.
#[must_use]
pub fn serialize_row<T: serde::Serialize + ?Sized>(row: &T) -> Value {
    serde_json::to_value(row).expect("a TableData type must serialize to JSON")
}

/// Generic output wrapper for raw JSON info endpoints.
///
/// Command modules should prefer typed rows when they have a stable domain
/// model. This wrapper is for documented API endpoints whose nested response
/// shape is valuable for agents after normalizing object keys to snake_case.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonValueOutput {
    value: Value,
    /// Headers and rows, built on the first table render: JSON output never needs them.
    table: OnceLock<(Vec<String>, Vec<Vec<String>>)>,
    empty_message: &'static str,
}

impl JsonValueOutput {
    /// `empty_message` is the sentence printed to stderr in pretty and table
    /// modes when the response has no rows, for example `No orders.`.
    #[must_use]
    pub fn new(value: Value, empty_message: &'static str) -> Self {
        Self {
            value: snake_case_json_keys(value),
            table: OnceLock::new(),
            empty_message,
        }
    }

    fn table(&self) -> &(Vec<String>, Vec<Vec<String>>) {
        self.table
            .get_or_init(|| json_value_table_rows(&self.value))
    }
}

impl TableData for JsonValueOutput {
    fn headers(&self) -> Vec<&str> {
        self.table().0.iter().map(String::as_str).collect()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.table().1.clone()
    }

    fn empty_message(&self) -> Option<&str> {
        Some(self.empty_message)
    }

    fn to_json_value(&self) -> serde_json::Value {
        self.value.clone()
    }
}

fn snake_case_json_keys(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (to_snake_case(&key), snake_case_json_keys(value)))
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.into_iter().map(snake_case_json_keys).collect())
        }
        scalar => scalar,
    }
}

fn json_value_table_rows(value: &Value) -> (Vec<String>, Vec<Vec<String>>) {
    match value {
        Value::Array(values) if values.is_empty() => (vec!["Value".to_string()], Vec::new()),
        Value::Array(values) => array_table_rows(values),
        Value::Object(object) if object.is_empty() => {
            (vec!["Field".to_string(), "Value".to_string()], Vec::new())
        }
        Value::Object(object) => (
            vec!["Field".to_string(), "Value".to_string()],
            object
                .iter()
                .map(|(key, value)| vec![key.clone(), json_cell(value)])
                .collect(),
        ),
        scalar => (vec!["Value".to_string()], vec![vec![json_cell(scalar)]]),
    }
}

fn array_table_rows(values: &[Value]) -> (Vec<String>, Vec<Vec<String>>) {
    if values.iter().any(|value| !value.is_object()) {
        return (
            vec!["Value".to_string()],
            values.iter().map(|value| vec![json_cell(value)]).collect(),
        );
    }

    let headers = values
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|object| object.keys().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let rows = values
        .iter()
        .filter_map(Value::as_object)
        .map(|object| {
            headers
                .iter()
                .map(|header| object.get(header).map(json_cell).unwrap_or_default())
                .collect()
        })
        .collect();

    (headers, rows)
}

pub(crate) fn json_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| String::new())
        }
    }
}

fn to_snake_case(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut previous_was_underscore = false;
    let mut previous_was_lower_or_digit = false;

    for ch in input.chars() {
        if matches!(ch, '-' | ' ' | '.') {
            if !output.is_empty() && !previous_was_underscore {
                output.push('_');
                previous_was_underscore = true;
            }
            previous_was_lower_or_digit = false;
            continue;
        }

        if ch == '_' {
            if !output.is_empty() && !previous_was_underscore {
                output.push('_');
            }
            previous_was_underscore = true;
            previous_was_lower_or_digit = false;
            continue;
        }

        if ch.is_ascii_uppercase() {
            if previous_was_lower_or_digit && !previous_was_underscore {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
            previous_was_underscore = false;
            previous_was_lower_or_digit = false;
            continue;
        }

        output.push(ch);
        previous_was_underscore = false;
        previous_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }

    output.trim_matches('_').to_string()
}

/// Characters that render as nothing or reorder text without being C0/C1
/// controls: every Unicode format character plus the line and paragraph
/// separators.
fn is_invisible_format_char(ch: char) -> bool {
    is_format_char(ch) || matches!(ch, '\u{2028}' | '\u{2029}')
}

/// Length of a style sequence this module's color helpers emit at the start of `text`.
fn known_style_len(text: &str) -> Option<usize> {
    colors::ALL_STYLES
        .iter()
        .find(|code| text.starts_with(**code))
        .map(|code| code.len())
}

/// Make a table cell safe to print: control, bidi, zero-width and other
/// invisible format characters become visible escapes (`\n`, `\t`, `\x1b`,
/// `\u{202e}`). With `keep_styles`, the exact color sequences from [`colors`]
/// pass through so styled cells keep their theme.
fn sanitize_cell(cell: &str, keep_styles: bool) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut rest = cell;
    while let Some(ch) = rest.chars().next() {
        if keep_styles
            && ch == '\x1b'
            && let Some(len) = known_style_len(rest)
        {
            out.push_str(&rest[..len]);
            rest = &rest[len..];
            continue;
        }
        match ch {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            ch if ch.is_control() => out.push_str(&format!("\\x{:02x}", u32::from(ch))),
            ch if is_invisible_format_char(ch) => {
                out.push_str(&format!("\\u{{{:x}}}", u32::from(ch)));
            }
            ch => out.push(ch),
        }
        rest = &rest[ch.len_utf8()..];
    }
    out
}

struct Rendered {
    text: String,
    notices: Vec<Notice>,
}

#[cfg(test)]
impl Rendered {
    fn notice_texts(&self) -> Vec<String> {
        self.notices.iter().map(Notice::text).collect()
    }
}

/// What both human formats share before they draw anything: sanitized headers,
/// rows clipped to `--max-results`, and the stderr notices.
struct Prepared {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    notices: Vec<Notice>,
}

impl Prepared {
    fn new(data: &dyn TableData, rows: Vec<Vec<String>>, max_results: Option<usize>) -> Self {
        let headers = data
            .headers()
            .into_iter()
            .map(|header| sanitize_cell(header, false))
            .collect();
        let total = rows.len();
        let rows: Vec<Vec<String>> = match max_results {
            Some(max) => rows.into_iter().take(max).collect(),
            None => rows,
        };
        let notices = row_notices(data, total, rows.len());
        Self {
            headers,
            rows,
            notices,
        }
    }

    /// Draw with `draw(headers, rows)`. An empty result prints nothing on a
    /// terminal (its sentence goes to stderr) and only the real header on a pipe.
    fn render(self, tty: bool, draw: impl FnOnce(&[String], &[Vec<String>]) -> String) -> Rendered {
        let text = if tty && self.rows.is_empty() {
            String::new()
        } else {
            draw(&self.headers, &self.rows)
        };
        Rendered {
            text,
            notices: self.notices,
        }
    }
}

/// Notices for human formats: the empty-state sentence, or how many rows
/// `--max-results` clipped.
fn row_notices(data: &dyn TableData, total: usize, shown: usize) -> Vec<Notice> {
    if total == 0 {
        return vec![Notice::Empty(
            data.empty_message().unwrap_or("No results.").to_string(),
        )];
    }
    if shown < total {
        return vec![Notice::Clipped { shown, total }];
    }
    Vec::new()
}

fn render_pretty_inner(data: &dyn TableData, max_results: Option<usize>, tty: bool) -> Rendered {
    let keep_styles = colors::stdout_enabled();
    Prepared::new(data, data.pretty_rows(), max_results).render(tty, |headers, rows| {
        let mut tw = TabWriter::new(Vec::new());
        let header_line = headers
            .iter()
            .map(|header| colors::cyan(header))
            .collect::<Vec<_>>()
            .join("\t");
        writeln!(tw, "{header_line}").unwrap();

        if !rows.is_empty() {
            let sep_line = headers
                .iter()
                .map(|_| "─".repeat(12))
                .collect::<Vec<_>>()
                .join("\t");
            writeln!(tw, "{sep_line}").unwrap();
        }

        for row in rows {
            let line = row
                .iter()
                .map(|cell| sanitize_cell(cell, keep_styles))
                .collect::<Vec<_>>()
                .join("\t");
            writeln!(tw, "{line}").unwrap();
        }

        tw.flush().unwrap();
        let output = String::from_utf8_lossy(&tw.into_inner().unwrap()).to_string();
        output.trim_end().to_string()
    })
}

fn render_table_inner(data: &dyn TableData, max_results: Option<usize>, tty: bool) -> Rendered {
    Prepared::new(data, data.rows(), max_results).render(tty, |headers, rows| {
        let rows = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| sanitize_cell(cell, false))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        if !tty {
            let mut lines = Vec::with_capacity(rows.len() + 1);
            lines.push(headers.join("\t"));
            lines.extend(rows.iter().map(|row| row.join("\t")));
            return lines.join("\n");
        }

        use tabled::builder::Builder;
        use tabled::settings::Style;

        let mut builder = Builder::default();
        builder.push_record(headers.iter().cloned());
        for row in rows {
            builder.push_record(row);
        }
        builder
            .build()
            .with(Style::rounded())
            .to_string()
            .trim_end()
            .to_string()
    })
}

/// The one JSON serializer for stdout: indented with 2 spaces when `pretty`
/// (a terminal), compact single-line otherwise (a pipe or NDJSON stream).
/// A [`Value`] always serializes, so there is no failure to handle.
fn json_text(value: &Value, pretty: bool) -> String {
    if pretty {
        format!("{value:#}")
    } else {
        value.to_string()
    }
}

fn render_outcome(
    data: &dyn TableData,
    format: OutputFormat,
    options: &JsonRenderOptions,
    tty: bool,
) -> Rendered {
    match format {
        OutputFormat::Pretty => render_pretty_inner(data, options.max_results(), tty),
        OutputFormat::Table => render_table_inner(data, options.max_results(), tty),
        OutputFormat::Json => Rendered {
            text: json_text(&options.apply(data.to_json_value()), tty),
            notices: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;

    fn plain_env() -> Mode {
        Mode::default()
    }

    fn tty_env() -> Mode {
        Mode {
            terminal: crate::terminal::Terminal {
                stdout: true,
                ..crate::terminal::Terminal::default()
            },
            ..Mode::default()
        }
    }

    fn render_with(data: &dyn TableData, format: OutputFormat, mode: Mode) -> String {
        Session::new(JsonRenderOptions::default(), &Mode { format, ..mode }).render(data)
    }

    fn render(data: &dyn TableData, format: OutputFormat) -> String {
        render_with(data, format, plain_env())
    }

    fn render_pretty(data: &dyn TableData) -> String {
        render(data, OutputFormat::Pretty)
    }

    fn render_table(data: &dyn TableData) -> String {
        render(data, OutputFormat::Table)
    }

    struct TestData {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        json: serde_json::Value,
    }

    impl TableData for TestData {
        fn headers(&self) -> Vec<&str> {
            self.headers.iter().map(|s| s.as_str()).collect()
        }

        fn rows(&self) -> Vec<Vec<String>> {
            self.rows.clone()
        }

        fn to_json_value(&self) -> serde_json::Value {
            self.json.clone()
        }
    }

    fn sample_data() -> TestData {
        TestData {
            headers: vec![
                "Name".to_string(),
                "Price".to_string(),
                "Change".to_string(),
            ],
            rows: vec![
                vec![
                    "BTC".to_string(),
                    "50000.00".to_string(),
                    "+2.5%".to_string(),
                ],
                vec![
                    "ETH".to_string(),
                    "3000.00".to_string(),
                    "-1.2%".to_string(),
                ],
            ],
            json: serde_json::json!([
                {"name": "BTC", "price": "50000.00", "change": "+2.5%"},
                {"name": "ETH", "price": "3000.00", "change": "-1.2%"}
            ]),
        }
    }

    #[test]
    fn json_value_output_arrays_use_union_headers_for_objects() {
        let output = JsonValueOutput::new(
            serde_json::json!([
                {"firstKey": "a"},
                {"secondKey": "b", "firstKey": "c"}
            ]),
            "empty",
        );

        assert_eq!(output.headers(), vec!["first_key", "second_key"]);
        assert_eq!(
            output.rows(),
            vec![
                vec!["a".to_string(), String::new()],
                vec!["c".to_string(), "b".to_string()],
            ]
        );
    }

    #[test]
    fn json_value_output_mixed_arrays_use_value_column() {
        let output =
            JsonValueOutput::new(serde_json::json!([{"firstKey": "a"}, "raw", 7]), "empty");

        assert_eq!(output.headers(), vec!["Value"]);
        assert_eq!(
            output.rows(),
            vec![
                vec![r#"{"first_key":"a"}"#.to_string()],
                vec!["raw".to_string()],
                vec!["7".to_string()],
            ]
        );
    }

    #[test]
    fn pretty_aligns_columns_under_a_separator_row() {
        assert_eq!(
            render_pretty(&sample_data()),
            "Name          Price         Change\n\
             ────────────  ────────────  ────────────\n\
             BTC           50000.00      +2.5%\n\
             ETH           3000.00       -1.2%"
        );
    }

    #[test]
    fn pretty_headers_are_cyan_only_when_stdout_styling_is_on() {
        colors::with_streams_for_test(true, false, || {
            let output = render_pretty(&sample_data());
            let header = output.lines().next().unwrap();
            for name in ["Name", "Price", "Change"] {
                assert!(
                    header.contains(&colors::paint(colors::CYAN, name)),
                    "{header}"
                );
            }
        });
        assert!(!render_pretty(&sample_data()).contains('\x1b'));
    }

    #[test]
    fn table_on_a_terminal_is_a_rounded_grid_and_never_colored() {
        colors::with_streams_for_test(true, true, || {
            let output = render_with(&sample_data(), OutputFormat::Table, tty_env());
            assert_eq!(
                output,
                "╭──────┬──────────┬────────╮\n\
                 │ Name │ Price    │ Change │\n\
                 ├──────┼──────────┼────────┤\n\
                 │ BTC  │ 50000.00 │ +2.5%  │\n\
                 │ ETH  │ 3000.00  │ -1.2%  │\n\
                 ╰──────┴──────────┴────────╯"
            );
            assert!(!render_table(&sample_data()).contains('\x1b'));
        });
    }

    #[test]
    fn test_project_selected_fields_filters_array_objects() {
        let value = serde_json::json!([
            {"coin": "BTC", "price": "50000", "size": "0.1", "side": "buy"},
            {"coin": "ETH", "price": "3000", "size": "1.5", "side": "sell"}
        ]);

        let projected = project_selected_fields(
            value,
            &["coin".to_string(), "price".to_string(), "size".to_string()],
        );

        let rows = projected.as_array().unwrap();
        for row in rows {
            let object = row.as_object().unwrap();
            assert_eq!(object.len(), 3);
            assert!(object.contains_key("coin"));
            assert!(object.contains_key("price"));
            assert!(object.contains_key("size"));
        }
    }

    #[test]
    fn test_project_selected_fields_supports_agent_aliases() {
        let value = serde_json::json!([
            {"name": "BTC", "mid": "50000", "max_leverage": 50}
        ]);

        let projected = project_selected_fields(value, &["coin".to_string(), "price".to_string()]);

        assert_eq!(projected[0]["coin"], "BTC");
        assert_eq!(projected[0]["price"], "50000");
        assert_eq!(projected[0].as_object().unwrap().len(), 2);
    }

    #[test]
    fn test_project_selected_fields_supports_order_price_aliases() {
        let snake_case_value = serde_json::json!([
            {"coin": "BTC", "limit_price": "50000", "size": "0.1", "oid": 12345}
        ]);
        let sdk_value = serde_json::json!([
            {"coin": "ETH", "limit_px": "3000", "sz": "1.5", "oid": 67890}
        ]);
        let api_value = serde_json::json!([
            {"coin": "SOL", "limitPx": "100", "sz": "2", "oid": 24680}
        ]);

        let fields = &["coin".to_string(), "price".to_string(), "size".to_string()];

        let snake_case_projected = project_selected_fields(snake_case_value, fields);
        assert_eq!(snake_case_projected[0]["coin"], "BTC");
        assert_eq!(snake_case_projected[0]["price"], "50000");
        assert_eq!(snake_case_projected[0]["size"], "0.1");
        assert_eq!(snake_case_projected[0].as_object().unwrap().len(), 3);

        let sdk_projected = project_selected_fields(sdk_value, fields);
        assert_eq!(sdk_projected[0]["coin"], "ETH");
        assert_eq!(sdk_projected[0]["price"], "3000");
        assert_eq!(sdk_projected[0]["size"], "1.5");
        assert_eq!(sdk_projected[0].as_object().unwrap().len(), 3);

        let api_projected = project_selected_fields(api_value, fields);
        assert_eq!(api_projected[0]["coin"], "SOL");
        assert_eq!(api_projected[0]["price"], "100");
        assert_eq!(api_projected[0]["size"], "2");
        assert_eq!(api_projected[0].as_object().unwrap().len(), 3);
    }

    #[test]
    fn test_project_selected_fields_filters_top_level_object() {
        let value = serde_json::json!({
            "name": "BTC",
            "max_leverage": 50,
            "sz_decimals": 5
        });

        let projected =
            project_selected_fields(value, &["name".to_string(), "max_leverage".to_string()]);

        assert_eq!(projected["name"], "BTC");
        assert_eq!(projected["max_leverage"], 50);
        assert_eq!(projected.as_object().unwrap().len(), 2);
    }

    #[test]
    fn test_stream_options_limit_nested_payload_without_truncating_envelope() {
        let options = JsonRenderOptions::from_cli(None, false, Some(2));
        let value = serde_json::json!({
            "type": "event",
            "subscription": "allMids(None)",
            "data": {
                "channel": "allMids",
                "data": {
                    "mids": {
                        "BTC": "50000",
                        "ETH": "3000",
                        "SOL": "100"
                    }
                }
            }
        });

        let limited = options.apply_stream(value);

        assert_eq!(limited["type"], "event");
        assert_eq!(limited["subscription"], "allMids(None)");
        let mids = limited["data"]["data"]["mids"].as_object().unwrap();
        assert_eq!(mids.len(), 2);
        assert!(mids.contains_key("BTC"));
        assert!(mids.contains_key("ETH"));
    }

    #[test]
    fn test_stream_options_limit_one_preserves_intermediate_wrappers() {
        let options = JsonRenderOptions::from_cli(None, false, Some(1));
        let value = serde_json::json!({
            "type": "event",
            "subscription": "allMids(None)",
            "data": {
                "channel": "allMids",
                "data": {
                    "mids": {
                        "BTC": "50000",
                        "ETH": "3000"
                    }
                }
            }
        });

        let limited = options.apply_stream(value);

        assert_eq!(limited["data"]["channel"], "allMids");
        let payload = limited["data"]["data"]["mids"].as_object().unwrap();
        assert_eq!(payload.len(), 1);
        assert!(payload.contains_key("BTC"));
    }

    #[test]
    fn test_stream_options_project_top_level_fields() {
        let options = JsonRenderOptions::from_cli(Some("type,subscription"), false, None);
        let value = serde_json::json!({
            "type": "subscribed",
            "subscription": "allMids(None)",
            "data": {"ignored": true}
        });

        let projected = options.apply_stream(value);

        assert_eq!(
            projected,
            serde_json::json!({
                "type": "subscribed",
                "subscription": "allMids(None)"
            })
        );
    }

    #[test]
    fn test_stream_options_project_top_level_and_nested_fields() {
        let options = JsonRenderOptions::from_cli(Some("type,coin"), false, None);
        let value = serde_json::json!({
            "type": "event",
            "subscription": "allMids(None)",
            "data": {
                "coin": "BTC",
                "price": "50000"
            }
        });

        let projected = options.apply_stream(value);

        assert_eq!(
            projected,
            serde_json::json!({
                "type": "event",
                "data": {
                    "coin": "BTC"
                }
            })
        );
    }

    #[test]
    fn test_stream_options_explicit_data_selection_keeps_full_payload() {
        let options = JsonRenderOptions::from_cli(Some("data"), false, None);
        let value = serde_json::json!({
            "type": "event",
            "subscription": "trades(BTC)",
            "data": {
                "channel": "trades",
                "data": [{ "coin": "BTC", "price": "50000" }]
            }
        });

        let projected = options.apply_stream(value);

        assert_eq!(projected["data"]["channel"], "trades");
        assert!(projected["data"]["data"].is_array());
    }

    #[test]
    fn test_results_only_strips_common_envelope_keys() {
        let value = serde_json::json!({
            "data": [{"coin": "BTC"}],
            "metadata": {"duration_ms": 12}
        });

        let stripped = strip_results_envelope(value);

        assert_eq!(stripped, serde_json::json!([{"coin": "BTC"}]));
    }

    #[test]
    fn table_on_a_pipe_is_tab_separated() {
        let output = render_table(&sample_data());
        assert_eq!(
            output,
            "Name\tPrice\tChange\nBTC\t50000.00\t+2.5%\nETH\t3000.00\t-1.2%"
        );
    }

    #[test]
    fn test_json_is_compact_on_pipe_and_indented_on_terminal() {
        let data = sample_data();
        let compact = render(&data, OutputFormat::Json);
        assert!(!compact.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&compact).unwrap();

        let pretty = render_with(&data, OutputFormat::Json, tty_env());
        assert!(pretty.contains("\n  "));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pretty).unwrap(),
            value
        );
    }

    fn empty_data() -> TestData {
        TestData {
            headers: vec!["Name".to_string(), "Price".to_string()],
            rows: vec![],
            json: serde_json::json!([]),
        }
    }

    #[test]
    fn test_empty_data_notice_goes_to_stderr_stream() {
        let options = JsonRenderOptions::default();
        let outcome = render_outcome(&empty_data(), OutputFormat::Table, &options, false);
        assert_eq!(outcome.notice_texts(), ["No results."]);

        let output = JsonValueOutput::new(serde_json::json!([]), "No orders.");
        let outcome = render_outcome(&output, OutputFormat::Table, &options, false);
        assert_eq!(outcome.text, "Value");
        assert_eq!(outcome.notice_texts(), ["No orders."]);

        let outcome = render_outcome(&output, OutputFormat::Json, &options, false);
        assert_eq!(outcome.text, "[]");
        assert!(outcome.notices.is_empty());
    }

    #[test]
    fn test_max_results_truncation_notice_for_human_formats_only() {
        let options = JsonRenderOptions::from_cli(None, false, Some(1));
        let data = sample_data();

        let table = render_outcome(&data, OutputFormat::Table, &options, false);
        assert_eq!(table.text, "Name\tPrice\tChange\nBTC\t50000.00\t+2.5%");
        assert_eq!(
            table.notice_texts(),
            ["Showing 1 of 2. Remove --max-results to see all."]
        );
        let pretty = render_outcome(&data, OutputFormat::Pretty, &options, false);
        assert_eq!(pretty.notices, table.notices);
        let json = render_outcome(&data, OutputFormat::Json, &options, false);
        assert!(json.notices.is_empty());

        let unclipped = JsonRenderOptions::from_cli(None, false, Some(2));
        assert!(
            render_outcome(&data, OutputFormat::Table, &unclipped, false)
                .notices
                .is_empty()
        );
    }

    #[test]
    fn test_cells_escape_control_and_format_characters() {
        let data = TestData {
            headers: vec!["Name".to_string()],
            rows: vec![vec![
                "evil\x1b[31m\nFORGED\tROW\u{202e}REV\u{200b}".to_string(),
            ]],
            json: serde_json::json!([{"name": "x"}]),
        };
        let expected = "evil\\x1b[31m\\nFORGED\\tROW\\u{202e}REV\\u{200b}";

        let table = render_table(&data);
        assert_eq!(table.lines().count(), 2);
        assert!(table.contains(expected));

        let pretty = render_pretty(&data);
        assert!(pretty.contains(expected));
        assert_eq!(pretty.lines().count(), 3);

        assert!(render_with(&data, OutputFormat::Table, tty_env()).contains(expected));
    }

    #[test]
    fn every_declared_style_passes_table_sanitising() {
        assert!(colors::ALL_STYLES.contains(&colors::BOLD));
        for code in colors::ALL_STYLES {
            let cell = format!("{code}x");
            assert_eq!(sanitize_cell(&cell, true), cell, "{code:?}");
            assert_ne!(sanitize_cell(&cell, false), cell, "{code:?}");
        }
    }

    #[test]
    fn test_pretty_keeps_theme_styles_but_escapes_other_escapes() {
        colors::with_streams_for_test(true, false, || {
            let cell = format!("{}\x1b[2J", colors::green("10"));
            assert_eq!(
                sanitize_cell(&cell, true),
                format!("{}\\x1b[2J", colors::paint(colors::GREEN, "10"))
            );
            assert_eq!(
                sanitize_cell(&colors::green("10"), false),
                "\\x1b[32m10\\x1b[0m"
            );
        });
    }

    #[test]
    fn cells_hide_every_format_character_the_sanitizer_hides() {
        // Arabic number sign and a tag character are `Cf` but were outside the
        // table cells used before it was shared with the error sanitizer.
        assert_eq!(
            sanitize_cell("a\u{0600}b\u{E0041}c", false),
            "a\\u{600}b\\u{e0041}c"
        );
        assert_eq!(
            sanitize_cell("a\u{2028}b\u{2029}c", false),
            "a\\u{2028}b\\u{2029}c"
        );
        assert_eq!(sanitize_cell("a\u{202E}b", false), "a\\u{202e}b");
    }

    #[test]
    fn json_text_is_pretty_on_a_terminal_and_compact_otherwise() {
        let value = serde_json::json!({"a": [1, 2], "b": "x"});
        assert_eq!(json_text(&value, false), r#"{"a":[1,2],"b":"x"}"#);
        assert_eq!(
            json_text(&value, true),
            serde_json::to_string_pretty(&value).unwrap()
        );
    }

    #[test]
    #[should_panic(expected = "must serialize to JSON")]
    fn serialize_row_fails_loudly_instead_of_printing_an_empty_document() {
        let unserializable =
            std::collections::HashMap::from([((1, 2), "tuple keys are not JSON object keys")]);
        let _ = serialize_row(&unserializable);
    }

    #[test]
    fn empty_result_is_silent_on_a_terminal_and_header_only_on_a_pipe() {
        let empty = TestData {
            headers: vec!["Coin".to_string()],
            rows: Vec::new(),
            json: serde_json::json!([]),
        };
        for format in [OutputFormat::Pretty, OutputFormat::Table] {
            let on_terminal = render_outcome(&empty, format, &JsonRenderOptions::default(), true);
            assert_eq!(on_terminal.text, "", "{format}");
            assert_eq!(on_terminal.notice_texts(), ["No results."], "{format}");

            let on_pipe = render_outcome(&empty, format, &JsonRenderOptions::default(), false);
            assert_eq!(on_pipe.text, "Coin", "{format}");
            assert_eq!(on_pipe.notice_texts(), ["No results."], "{format}");
        }
    }
}
