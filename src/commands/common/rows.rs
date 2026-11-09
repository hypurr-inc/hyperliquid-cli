//! Output types that are one serializable row (or a list of them) and nothing
//! else: the headers, the cells and the JSON document each come from the row
//! type, so the three cannot drift apart.

use serde::Serialize;

use crate::output::{self, TableData};

pub const MISSING: &str = "n/a";

/// `Self`'s `Serialize` output is its JSON document.
pub trait Row: Serialize {
    const HEADERS: &'static [&'static str];

    fn cells(&self) -> Vec<String>;

    /// Style a column through [`style_column`], which finds it by header.
    fn pretty_cells(&self) -> Vec<String> {
        self.cells()
    }
}

/// A [`Row`] shown in a list, with the sentence an empty list prints.
pub trait ListRow: Row {
    const EMPTY: &'static str;
}

/// # Panics
///
/// When `R` has no such header: that is a bug in the row type, found by any
/// render of it.
pub fn style_column<R: Row>(
    cells: &mut [String],
    header: &str,
    style: impl FnOnce(&str) -> String,
) {
    let column = R::HEADERS
        .iter()
        .position(|candidate| *candidate == header)
        .unwrap_or_else(|| panic!("no column named {header:?}"));
    cells[column] = style(&cells[column]);
}

#[must_use]
pub fn or_missing(value: Option<impl ToString>) -> String {
    value.map_or_else(|| MISSING.to_string(), |value| value.to_string())
}

/// A table without the column leaves the rows alone.
pub fn paint_column(
    headers: &[&str],
    mut rows: Vec<Vec<String>>,
    header: &str,
    applies: fn(&str) -> bool,
    paint: fn(&str) -> String,
) -> Vec<Vec<String>> {
    if let Some(column) = headers.iter().position(|name| *name == header) {
        for row in &mut rows {
            if let Some(cell) = row.get_mut(column)
                && applies(cell)
            {
                *cell = paint(cell);
            }
        }
    }
    rows
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rows<T> {
    rows: Vec<T>,
}

impl<T> Rows<T> {
    #[must_use]
    pub fn new(rows: Vec<T>) -> Self {
        Self { rows }
    }

    /// A list of exactly `row`: a JSON array of one object, unlike [`One`].
    #[must_use]
    pub fn one(row: T) -> Self {
        Self { rows: vec![row] }
    }
}

impl<T: ListRow> TableData for Rows<T> {
    fn headers(&self) -> Vec<&str> {
        T::HEADERS.to_vec()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows.iter().map(Row::cells).collect()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.rows.iter().map(Row::pretty_cells).collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some(T::EMPTY)
    }

    fn to_json_value(&self) -> serde_json::Value {
        output::serialize_row(&self.rows)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct One<T> {
    row: T,
}

impl<T> One<T> {
    #[must_use]
    pub fn new(row: T) -> Self {
        Self { row }
    }
}

impl<T: Row> TableData for One<T> {
    fn headers(&self) -> Vec<&str> {
        T::HEADERS.to_vec()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![self.row.cells()]
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        vec![self.row.pretty_cells()]
    }

    fn to_json_value(&self) -> serde_json::Value {
        output::serialize_row(&self.row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Pair {
        name: &'static str,
        value: u32,
    }

    impl Row for Pair {
        const HEADERS: &'static [&'static str] = &["Name", "Value"];

        fn cells(&self) -> Vec<String> {
            vec![self.name.to_string(), self.value.to_string()]
        }

        fn pretty_cells(&self) -> Vec<String> {
            let mut cells = self.cells();
            style_column::<Self>(&mut cells, "Value", |value| format!("<{value}>"));
            cells
        }
    }

    impl ListRow for Pair {
        const EMPTY: &'static str = "No pairs.";
    }

    #[test]
    fn rows_render_headers_cells_json_and_the_empty_sentence() {
        let list = Rows::new(vec![Pair {
            name: "a",
            value: 1,
        }]);

        assert_eq!(list.headers(), ["Name", "Value"]);
        assert_eq!(list.rows(), [["a", "1"]]);
        assert_eq!(list.pretty_rows(), [["a", "<1>"]]);
        assert_eq!(
            list.to_json_value(),
            serde_json::json!([{"name": "a", "value": 1}])
        );
        assert_eq!(list.empty_message(), Some("No pairs."));
    }

    #[test]
    fn one_renders_a_single_object() {
        let one = One::new(Pair {
            name: "a",
            value: 1,
        });

        assert_eq!(one.rows(), [["a", "1"]]);
        assert_eq!(
            one.to_json_value(),
            serde_json::json!({"name": "a", "value": 1})
        );
        assert_eq!(one.empty_message(), None);
    }

    #[test]
    #[should_panic(expected = "no column named")]
    fn styling_an_unknown_column_is_a_bug_not_a_silent_skip() {
        let mut cells = vec!["a".to_string(), "1".to_string()];
        style_column::<Pair>(&mut cells, "Nope", |cell| cell.to_string());
    }
}
