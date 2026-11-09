//! `outcomeTemplates`, and the `{keyword}` text outcomes deployed from them carry.
//!
//! An outcome or question deployed from a template is named `template:<id>`,
//! its `description` is `|`-separated `key:value` pairs, and the template's own
//! `name` holds `{key}` placeholders those pairs fill. Side names of template
//! outcomes are `template:` plus a side name that may hold placeholders too
//! (`template:{shortNameA}`, `template:No`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::output::warn;
use crate::wire::InfoRequest;

use super::outcome_description::OutcomeDescription;

/// What marks a name or side name as template-derived.
pub const TEMPLATE_PREFIX: &str = "template:";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TemplateKeyword {
    pub keyword: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRole {
    StandaloneOutcome,
    Question,
    QuestionOutcome,
    /// Any other shape the exchange sends, kept as its raw JSON text and
    /// reported in the template's `warnings`.
    Unrecognized(String),
}

impl TemplateRole {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::StandaloneOutcome => "standalone_outcome",
            Self::Question => "question",
            Self::QuestionOutcome => "question_outcome",
            Self::Unrecognized(raw) => raw,
        }
    }
}

impl Serialize for TemplateRole {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OutcomeTemplate {
    pub id: String,
    pub role: TemplateRole,
    /// The question template a `question_outcome` template belongs to.
    pub parent: Option<String>,
    /// Side names of a `standalone_outcome` template (placeholders unresolved).
    pub side_names: Vec<String>,
    pub name: String,
    pub description: String,
    pub keywords: Vec<TemplateKeyword>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawTemplate {
    id: String,
    role: Value,
    name: String,
    description: String,
    #[serde(default)]
    keywords: Vec<(String, String)>,
}

/// The templates, and one line per entry that could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateList {
    pub templates: Vec<OutcomeTemplate>,
    pub skipped: Vec<String>,
}

impl TemplateList {
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&OutcomeTemplate> {
        self.templates.iter().find(|template| template.id == id)
    }
}

/// The templates `outcomeTemplates` lists. Entries that could not be read are
/// named on stderr, not dropped silently and not fatal.
pub async fn load_outcome_templates(api: &ApiClient) -> Result<TemplateList, CliError> {
    let entries = api
        .info::<Vec<Value>>(
            &InfoRequest::new("outcomeTemplates"),
            "loading outcome templates",
        )
        .await?;
    let list = templates_from_entries(entries);
    report_skipped(&list.skipped);
    Ok(list)
}

/// One stderr warning per entry a loader could not turn into rows.
pub(super) fn report_skipped(skipped: &[String]) {
    for line in skipped {
        warn(line);
    }
}

pub(super) fn templates_from_entries(entries: Vec<Value>) -> TemplateList {
    let mut list = TemplateList::default();
    for entry in entries {
        match serde_json::from_value::<RawTemplate>(entry.clone()) {
            Ok(raw) => list.templates.push(template_from_raw(raw)),
            Err(err) => list
                .skipped
                .push(format!("outcome template {entry} was not read: {err}")),
        }
    }
    list
}

fn template_from_raw(raw: RawTemplate) -> OutcomeTemplate {
    let mut warnings = Vec::new();
    let mut parent = None;
    let mut side_names = Vec::new();
    let role = match &raw.role {
        Value::String(role) if role == "question" => TemplateRole::Question,
        Value::Object(map) if map.len() == 1 => match map.iter().next() {
            Some((key, body)) if key == "standaloneOutcome" => {
                side_names = body
                    .get("sideNames")
                    .and_then(Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(|name| name.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                TemplateRole::StandaloneOutcome
            }
            Some((key, body)) if key == "questionOutcome" => {
                parent = body
                    .get("parent")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                TemplateRole::QuestionOutcome
            }
            _ => unknown_role(&raw.role, &mut warnings),
        },
        _ => unknown_role(&raw.role, &mut warnings),
    };
    OutcomeTemplate {
        id: raw.id,
        role,
        parent,
        side_names,
        name: raw.name,
        description: raw.description,
        keywords: raw
            .keywords
            .into_iter()
            .map(|(keyword, kind)| TemplateKeyword { keyword, kind })
            .collect(),
        warnings,
    }
}

fn unknown_role(role: &Value, warnings: &mut Vec<String>) -> TemplateRole {
    warnings.push(format!("unrecognized template role {role}"));
    TemplateRole::Unrecognized(role.to_string())
}

/// `text` with every `{key}` that `values` has replaced, and the keys it
/// lacked (left in the text as written).
#[must_use]
pub fn interpolate(text: &str, values: &OutcomeDescription) -> (String, Vec<String>) {
    let mut out = String::with_capacity(text.len());
    let mut missing = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) if is_keyword(&after[..close]) => {
                let key = &after[..close];
                match values.get(key) {
                    Some(value) => out.push_str(value),
                    None => {
                        out.push_str(&rest[open..open + close + 2]);
                        if !missing.iter().any(|seen| seen == key) {
                            missing.push(key.to_string());
                        }
                    }
                }
                rest = &after[close + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    (out, missing)
}

fn is_keyword(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// A side name with its `template:` prefix removed and placeholders filled
/// from the outcome's description. `None` in the second slot when nothing was
/// missing.
#[must_use]
pub fn resolve_side_name(
    side_name: &str,
    description: &OutcomeDescription,
) -> (String, Option<String>) {
    let Some(pattern) = side_name.strip_prefix(TEMPLATE_PREFIX) else {
        return (side_name.to_string(), None);
    };
    let (text, missing) = interpolate(pattern, description);
    let warning = (!missing.is_empty()).then(|| {
        format!(
            "side name '{side_name}' has no value for {} in the description",
            missing.join(", ")
        )
    });
    (text, warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(description: &str) -> OutcomeDescription {
        OutcomeDescription::parse(description)
    }

    #[test]
    fn placeholders_are_filled_from_description_pairs_and_missing_ones_stay_visible() {
        let (text, missing) = interpolate(
            "{a} v {b}: {c} {a}",
            &values("a:PHX|b:SAS|note without colon|a:ignored"),
        );

        assert_eq!(text, "PHX v SAS: {c} PHX");
        assert_eq!(missing, ["c"]);
    }

    #[test]
    fn values_keep_colons_and_text_without_keywords_is_untouched() {
        let pairs = values("time:20261110-0200|label:a:b");
        assert_eq!(pairs.get("time"), Some("20261110-0200"));
        assert_eq!(pairs.get("label"), Some("a:b"));
        let (text, missing) = interpolate("price {not a keyword} {} {", &pairs);
        assert_eq!(text, "price {not a keyword} {} {");
        assert!(missing.is_empty());
    }

    #[test]
    fn side_names_drop_the_template_prefix_and_report_missing_keywords() {
        assert_eq!(
            resolve_side_name("template:{shortNameA}", &values("shortNameA:PHX")),
            ("PHX".to_string(), None)
        );
        assert_eq!(
            resolve_side_name("template:No", &values("")),
            ("No".to_string(), None)
        );
        assert_eq!(
            resolve_side_name("Yes", &values("")),
            ("Yes".to_string(), None)
        );
        let (text, warning) = resolve_side_name("template:{shortNameB}", &values("shortNameA:PHX"));
        assert_eq!(text, "{shortNameB}");
        assert!(warning.unwrap().contains("shortNameB"));
    }

    #[test]
    fn roles_decode_in_all_three_documented_shapes_and_unknown_ones_are_reported() {
        let list = templates_from_entries(vec![
            serde_json::json!({"id": "s", "name": "n", "description": "d", "keywords": [["perp", "hlPerp"]],
                "role": {"standaloneOutcome": {"sideNames": ["Yes", "No"]}}}),
            serde_json::json!({"id": "q", "name": "n", "description": "d", "keywords": [], "role": "question"}),
            serde_json::json!({"id": "c", "name": "n", "description": "d", "keywords": [],
                "role": {"questionOutcome": {"parent": "q"}}}),
            serde_json::json!({"id": "x", "name": "n", "description": "d", "keywords": [], "role": {"scalar": 1}}),
            serde_json::json!({"id": "broken"}),
        ]);

        let roles: Vec<(&str, &str, Option<&str>)> = list
            .templates
            .iter()
            .map(|t| (t.id.as_str(), t.role.as_str(), t.parent.as_deref()))
            .collect();
        assert_eq!(
            roles,
            [
                ("s", "standalone_outcome", None),
                ("q", "question", None),
                ("c", "question_outcome", Some("q")),
                ("x", r#"{"scalar":1}"#, None),
            ]
        );
        assert_eq!(list.templates[0].role, TemplateRole::StandaloneOutcome);
        assert_eq!(list.templates[0].side_names, ["Yes", "No"]);
        assert_eq!(list.templates[0].keywords[0].kind, "hlPerp");
        assert!(list.templates[3].warnings[0].contains("unrecognized"));
        assert_eq!(list.skipped.len(), 1);
        assert!(list.skipped[0].contains("broken"), "{:?}", list.skipped);
    }

    #[test]
    fn a_template_role_serializes_as_its_name() {
        let json = serde_json::to_value([
            TemplateRole::StandaloneOutcome,
            TemplateRole::Question,
            TemplateRole::QuestionOutcome,
            TemplateRole::Unrecognized("{\"scalar\":1}".to_string()),
        ])
        .unwrap();

        assert_eq!(
            json,
            serde_json::json!([
                "standalone_outcome",
                "question",
                "question_outcome",
                "{\"scalar\":1}"
            ])
        );
    }
}
