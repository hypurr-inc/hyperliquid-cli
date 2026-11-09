//! Outcome markets as `outcomeMeta` lists them: one row per tradable side, one
//! row per question, and the `settledOutcome` read.

use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::output::warn;
use crate::wire::InfoRequest;

use super::asset_id::{outcome_asset_id, outcome_coin, outcome_encoding, outcome_token};
use super::outcome_templates::{
    TEMPLATE_PREFIX, TemplateList, description_values, interpolate, load_outcome_templates,
    resolve_side_name,
};

/// How a question lists an outcome.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuestionRole {
    /// One of the question's named outcomes.
    Named,
    /// The outcome the protocol created for "none of the named outcomes".
    Fallback,
}

/// One side of an outcome market.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OutcomeSideRow {
    pub outcome: u64,
    pub side: u64,
    pub encoding: u64,
    pub coin: String,
    pub token: String,
    pub asset_id: u64,
    /// With the catalog loaders that read `outcomeTemplates`, a template
    /// outcome's name is the template's text with the description's values
    /// filled in; [`load_outcome_sides`] leaves the raw `template:<id>`.
    pub outcome_name: String,
    pub side_name: String,
    pub description: String,
    pub side_token: Option<u64>,
    /// Token the outcome is quoted and settled in (`USDC`, `USDH`, ...).
    pub quote_token: Option<String>,
    /// Venue of the HIP-4 deployer; absent on protocol-recurring outcomes.
    pub venue: Option<String>,
    pub deployer_fee_scale: Option<Decimal>,
    /// The template id when the outcome was deployed from a template.
    pub template: Option<String>,
    /// The question this outcome belongs to, if any.
    pub question: Option<u64>,
    pub question_name: Option<String>,
    pub question_role: Option<QuestionRole>,
    /// The question lists this outcome among its settled named outcomes.
    pub settled: bool,
    /// What is odd about this row, never dropped silently.
    pub warnings: Vec<String>,
}

/// One question: a collection of outcomes where exactly one settles to Yes.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OutcomeQuestionRow {
    pub question: u64,
    pub name: String,
    pub template: Option<String>,
    pub description: String,
    pub fallback_outcome: Option<u64>,
    pub named_outcomes: Vec<u64>,
    pub settled_named_outcomes: Vec<u64>,
    pub warnings: Vec<String>,
}

/// Everything `outcomeMeta` lists, with the entries that could not be turned
/// into rows named in `skipped`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutcomeCatalog {
    /// Ordered by outcome, then side, ascending.
    pub sides: Vec<OutcomeSideRow>,
    /// Ordered by question, ascending.
    pub questions: Vec<OutcomeQuestionRow>,
    pub skipped: Vec<String>,
}

/// Every outcome side, ordered by outcome and side, with names as the exchange
/// sends them. An outcome that cannot become rows is reported on stderr, not
/// dropped silently and not fatal: one odd outcome must not hide the rest.
pub async fn load_outcome_sides(api: &ApiClient) -> Result<Vec<OutcomeSideRow>, CliError> {
    let catalog = load_outcome_listing(api).await?;
    report_skipped(&catalog.skipped);
    Ok(catalog.sides)
}

/// [`load_outcome_sides`] plus questions, with template names and side names
/// resolved. A failed `outcomeTemplates` read leaves the raw names and puts the
/// reason in the `warnings` of every row it affects.
pub async fn load_outcome_catalog(api: &ApiClient) -> Result<OutcomeCatalog, CliError> {
    let mut catalog = load_outcome_listing(api).await?;
    let templates = load_outcome_templates(api).await;
    let templates = match templates {
        Ok(list) => {
            report_skipped(&list.skipped);
            Ok(list)
        }
        Err(err) => Err(err.to_string()),
    };
    catalog.resolve_templates(&templates);
    report_skipped(&catalog.skipped);
    Ok(catalog)
}

/// One stderr warning per entry a loader could not turn into rows.
pub fn report_skipped(skipped: &[String]) {
    for line in skipped {
        warn(line);
    }
}

/// The `outcomeMeta` listing as is: template names unresolved and unreadable
/// entries left in `skipped` for the caller to report.
pub async fn load_outcome_listing(api: &ApiClient) -> Result<OutcomeCatalog, CliError> {
    let response = api
        .info::<OutcomeMetaResponse>(&InfoRequest::new("outcomeMeta"), "loading outcome metadata")
        .await?;
    Ok(OutcomeCatalog::from_meta(response))
}

impl OutcomeCatalog {
    fn from_meta(response: OutcomeMetaResponse) -> Self {
        let mut catalog = Self::default();

        for entry in response.questions {
            match serde_json::from_value::<RawQuestion>(entry.clone()) {
                Ok(raw) => catalog.questions.push(question_row(raw)),
                Err(err) => catalog
                    .skipped
                    .push(format!("outcomeMeta question {entry} was not read: {err}")),
            }
        }
        let membership = question_membership(&catalog.questions);

        for entry in response.outcomes {
            match serde_json::from_value::<RawOutcome>(entry.clone()) {
                Ok(raw) => catalog.push_outcome(raw, &membership),
                Err(err) => catalog
                    .skipped
                    .push(format!("outcomeMeta outcome {entry} was not read: {err}")),
            }
        }

        catalog.attach_unlisted_outcome_warnings();
        catalog.sides.sort_by_key(|row| (row.outcome, row.side));
        catalog.questions.sort_by_key(|row| row.question);
        catalog
    }

    fn push_outcome(&mut self, raw: RawOutcome, membership: &HashMap<u64, Vec<Membership>>) {
        let RawOutcome {
            outcome,
            name,
            description,
            side_specs,
            quote_token,
            venue,
            deployer_fee_scale,
        } = raw;
        if side_specs.is_empty() {
            self.skipped
                .push(format!("outcome {outcome} ('{name}') lists no sides"));
            return;
        }
        let mut shared_warnings = Vec::new();
        if side_specs.len() == 1 {
            shared_warnings.push(
                "outcomeMeta lists one side; outcomes are binary (sides 0 and 1)".to_string(),
            );
        }
        if side_specs.len() > 2 {
            let ignored: Vec<&str> = side_specs[2..]
                .iter()
                .map(|side| side.name.as_str())
                .collect();
            shared_warnings.push(format!(
                "outcomeMeta lists {} sides; outcomes are binary (sides 0 and 1), ignored: {}",
                side_specs.len(),
                ignored.join(", ")
            ));
        }
        let memberships = membership.get(&outcome).map(Vec::as_slice).unwrap_or(&[]);
        if memberships.len() > 1 {
            let questions: Vec<String> = memberships
                .iter()
                .map(|member| member.question.to_string())
                .collect();
            shared_warnings.push(format!(
                "listed by several questions ({}); showing the first",
                questions.join(", ")
            ));
        }
        let member = memberships.first();
        let template = name.strip_prefix(TEMPLATE_PREFIX).map(str::to_string);

        for (side, side_spec) in side_specs.into_iter().take(2).enumerate() {
            let side = side as u64;
            let encoded = outcome_encoding(outcome, side).and_then(|encoding| {
                Ok((
                    encoding,
                    outcome_asset_id(encoding)?,
                    outcome_coin(encoding),
                    outcome_token(encoding),
                ))
            });
            let (encoding, asset_id, coin, token) = match encoded {
                Ok(encoded) => encoded,
                Err(err) => {
                    self.skipped.push(format!(
                        "outcome {outcome} side {side} ('{name}') has no asset id: {err}"
                    ));
                    continue;
                }
            };
            let (side_name, side_warning) = resolve_side_name(&side_spec.name, &description);
            let mut warnings = shared_warnings.clone();
            warnings.extend(side_warning);
            self.sides.push(OutcomeSideRow {
                outcome,
                side,
                encoding,
                coin,
                token,
                asset_id,
                outcome_name: name.clone(),
                side_name,
                description: description.clone(),
                side_token: side_spec.token,
                quote_token: quote_token.clone(),
                venue: venue.clone(),
                deployer_fee_scale,
                template: template.clone(),
                question: member.map(|member| member.question),
                question_name: member.map(|member| member.name.clone()),
                question_role: member.map(|member| member.role),
                settled: member.is_some_and(|member| member.settled),
                warnings,
            });
        }
    }

    /// A question that names an outcome `outcomeMeta` does not list: said on
    /// the question, since no side row exists to carry it.
    fn attach_unlisted_outcome_warnings(&mut self) {
        let listed: std::collections::HashSet<u64> =
            self.sides.iter().map(|row| row.outcome).collect();
        for question in &mut self.questions {
            let unlisted: Vec<String> = question
                .named_outcomes
                .iter()
                .chain(&question.fallback_outcome)
                .chain(&question.settled_named_outcomes)
                .filter(|outcome| !listed.contains(outcome))
                .map(u64::to_string)
                .collect();
            if !unlisted.is_empty() {
                question.warnings.push(format!(
                    "outcomes not listed by outcomeMeta: {}",
                    unlisted.join(", ")
                ));
            }
        }
    }

    /// Replace template names with their text. `templates` is the
    /// `outcomeTemplates` list, or why it could not be read.
    pub fn resolve_templates(&mut self, templates: &Result<TemplateList, String>) {
        for question in &mut self.questions {
            let resolved = resolve_name(&question.name, &question.description, templates);
            question.name = resolved.text;
            question.warnings.extend(resolved.warnings);
        }
        let question_names: HashMap<u64, &str> = self
            .questions
            .iter()
            .map(|question| (question.question, question.name.as_str()))
            .collect();
        for row in &mut self.sides {
            let resolved = resolve_name(&row.outcome_name, &row.description, templates);
            row.outcome_name = resolved.text;
            row.warnings.extend(resolved.warnings);
            if let Some(name) = row.question.and_then(|id| question_names.get(&id)) {
                row.question_name = Some((*name).to_string());
            }
        }
    }
}

struct Membership {
    question: u64,
    name: String,
    role: QuestionRole,
    settled: bool,
}

fn question_membership(questions: &[OutcomeQuestionRow]) -> HashMap<u64, Vec<Membership>> {
    let mut membership: HashMap<u64, Vec<Membership>> = HashMap::new();
    for question in questions {
        let named = question.named_outcomes.iter().map(|outcome| {
            (
                *outcome,
                QuestionRole::Named,
                question.settled_named_outcomes.contains(outcome),
            )
        });
        let settled_only = question
            .settled_named_outcomes
            .iter()
            .filter(|outcome| !question.named_outcomes.contains(outcome))
            .map(|outcome| (*outcome, QuestionRole::Named, true));
        let fallback = question
            .fallback_outcome
            .map(|outcome| (outcome, QuestionRole::Fallback, false));
        for (outcome, role, settled) in named.chain(settled_only).chain(fallback) {
            membership.entry(outcome).or_default().push(Membership {
                question: question.question,
                name: question.name.clone(),
                role,
                settled,
            });
        }
    }
    membership
}

fn question_row(raw: RawQuestion) -> OutcomeQuestionRow {
    let template = raw.name.strip_prefix(TEMPLATE_PREFIX).map(str::to_string);
    OutcomeQuestionRow {
        question: raw.question,
        name: raw.name,
        template,
        description: raw.description,
        fallback_outcome: raw.fallback_outcome,
        named_outcomes: raw.named_outcomes,
        settled_named_outcomes: raw.settled_named_outcomes,
        warnings: Vec::new(),
    }
}

struct ResolvedName {
    text: String,
    warnings: Vec<String>,
}

/// A `template:<id>` name as the template's text with the description's values
/// filled in; any other name unchanged.
fn resolve_name(
    name: &str,
    description: &str,
    templates: &Result<TemplateList, String>,
) -> ResolvedName {
    let Some(id) = name.strip_prefix(TEMPLATE_PREFIX) else {
        return ResolvedName {
            text: name.to_string(),
            warnings: Vec::new(),
        };
    };
    let unresolved = |warning: String| ResolvedName {
        text: name.to_string(),
        warnings: vec![warning],
    };
    let list = match templates {
        Ok(list) => list,
        Err(reason) => {
            return unresolved(format!(
                "name '{name}' left as is: outcomeTemplates unavailable ({reason})"
            ));
        }
    };
    let Some(template) = list.get(id) else {
        return unresolved(format!(
            "name '{name}' left as is: template '{id}' is not in outcomeTemplates"
        ));
    };
    let (text, missing) = interpolate(&template.name, &description_values(description));
    let warnings = if missing.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "name from template '{id}' has no value for {} in the description",
            missing.join(", ")
        )]
    };
    ResolvedName { text, warnings }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeMetaResponse {
    /// Entries stay raw JSON so one malformed outcome is skipped with a
    /// message instead of failing the whole read.
    outcomes: Vec<Value>,
    #[serde(default)]
    questions: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOutcome {
    outcome: u64,
    name: String,
    description: String,
    #[serde(default)]
    side_specs: Vec<RawSideSpec>,
    quote_token: Option<String>,
    venue: Option<String>,
    deployer_fee_scale: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSideSpec {
    name: String,
    token: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawQuestion {
    question: u64,
    name: String,
    description: String,
    fallback_outcome: Option<u64>,
    #[serde(default)]
    named_outcomes: Vec<u64>,
    #[serde(default)]
    settled_named_outcomes: Vec<u64>,
}

/// How an outcome settled, from `settledOutcome`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SettledOutcomeRow {
    pub outcome: u64,
    pub name: String,
    pub template: Option<String>,
    pub description: String,
    pub side_names: Vec<String>,
    pub quote_token: Option<String>,
    pub deployer: Option<String>,
    /// Quote tokens paid per share of side 0.
    pub settle_fraction: Decimal,
    /// Quote tokens paid per share of side 0 and side 1: `settleFraction` and
    /// `1 - settleFraction`. Settlement pays these out automatically.
    pub side_payouts: Vec<Decimal>,
    /// Settlement details from the deployer, such as `price:76876.9`.
    pub details: String,
    pub question: Option<u64>,
    /// `active` or `settled`: outcomes of one question settle one at a time.
    pub question_state: Option<String>,
    pub question_name: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSettled {
    spec: RawSettledSpec,
    settle_fraction: Decimal,
    details: String,
    question: Option<RawSettledQuestion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSettledSpec {
    outcome: u64,
    name: String,
    description: String,
    #[serde(default)]
    side_specs: Vec<RawSideSpec>,
    quote_token: Option<String>,
    deployer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSettledQuestion {
    question: Value,
    name: String,
    description: String,
}

/// How `outcome` settled; `None` when it has not (the exchange answers `null`
/// for an outcome that is still trading and for an unknown id alike).
pub async fn load_settled_outcome(
    api: &ApiClient,
    outcome: u64,
) -> Result<Option<SettledOutcomeRow>, CliError> {
    let raw = api
        .info::<Option<RawSettled>>(
            &InfoRequest::new("settledOutcome").field("outcome", outcome),
            "loading settled outcome",
        )
        .await?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let needs_templates = raw.spec.name.starts_with(TEMPLATE_PREFIX)
        || raw
            .question
            .as_ref()
            .is_some_and(|question| question.name.starts_with(TEMPLATE_PREFIX));
    let templates = if needs_templates {
        match load_outcome_templates(api).await {
            Ok(list) => {
                report_skipped(&list.skipped);
                Ok(list)
            }
            Err(err) => Err(err.to_string()),
        }
    } else {
        Ok(TemplateList::default())
    };
    Ok(Some(settled_row(raw, &templates)))
}

fn settled_row(raw: RawSettled, templates: &Result<TemplateList, String>) -> SettledOutcomeRow {
    let mut warnings = Vec::new();
    let spec = raw.spec;
    let template = spec.name.strip_prefix(TEMPLATE_PREFIX).map(str::to_string);
    let name = resolve_name(&spec.name, &spec.description, templates);
    warnings.extend(name.warnings);
    let side_names = spec
        .side_specs
        .iter()
        .map(|side| {
            let (text, warning) = resolve_side_name(&side.name, &spec.description);
            warnings.extend(warning);
            text
        })
        .collect();
    let fraction = raw.settle_fraction;
    if fraction < Decimal::ZERO || fraction > Decimal::ONE {
        warnings.push(format!("settleFraction {fraction} is outside 0..=1"));
    }

    let (mut question, mut question_state, mut question_name) = (None, None, None);
    if let Some(raw_question) = raw.question {
        let resolved = resolve_name(&raw_question.name, &raw_question.description, templates);
        warnings.extend(resolved.warnings);
        question_name = Some(resolved.text);
        match question_state_of(&raw_question.question) {
            Some((state, id)) => {
                question = Some(id);
                question_state = Some(state.to_string());
            }
            None => warnings.push(format!(
                "unrecognized question state {}",
                raw_question.question
            )),
        }
    }

    SettledOutcomeRow {
        outcome: spec.outcome,
        name: name.text,
        template,
        description: spec.description,
        side_names,
        quote_token: spec.quote_token,
        deployer: spec.deployer,
        settle_fraction: fraction,
        side_payouts: vec![fraction.normalize(), (Decimal::ONE - fraction).normalize()],
        details: raw.details,
        question,
        question_state,
        question_name,
        warnings,
    }
}

/// `{"active": N}` or `{"settled": N}`.
fn question_state_of(value: &Value) -> Option<(&'static str, u64)> {
    let map = value.as_object().filter(|map| map.len() == 1)?;
    let (state, id) = map.iter().next()?;
    let state = match state.as_str() {
        "active" => "active",
        "settled" => "settled",
        _ => return None,
    };
    Some((state, id.as_u64()?))
}

#[cfg(test)]
pub(crate) fn catalog_for_tests(meta: Value) -> OutcomeCatalog {
    OutcomeCatalog::from_meta(serde_json::from_value(meta).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(value: serde_json::Value) -> OutcomeCatalog {
        OutcomeCatalog::from_meta(serde_json::from_value(value).unwrap())
    }

    fn templates(value: serde_json::Value) -> Result<TemplateList, String> {
        let entries: Vec<Value> = serde_json::from_value(value).unwrap();
        Ok(super::super::outcome_templates::templates_from_entries(
            entries,
        ))
    }

    #[test]
    fn sides_are_ordered_by_outcome_then_side_and_carry_their_names() {
        let rows = catalog(serde_json::json!({
            "outcomes": [
                {"outcome": 2, "name": "B", "description": "d2",
                 "sideSpecs": [{"name": "Yes", "token": 21}, {"name": "No"}]},
                {"outcome": 1, "name": "A", "description": "d1",
                 "sideSpecs": [{"name": "Up"}]}
            ]
        }))
        .sides;

        let order: Vec<(u64, u64)> = rows.iter().map(|row| (row.outcome, row.side)).collect();
        assert_eq!(order, [(1, 0), (2, 0), (2, 1)]);
        assert_eq!(rows[0].side_name, "Up");
        assert_eq!(rows[1].coin, "#20");
        assert_eq!(rows[1].token, "+20");
        assert_eq!(rows[1].asset_id, 100_000_020);
        assert_eq!(rows[1].side_token, Some(21));
        assert_eq!(rows[2].side_token, None);
    }

    #[test]
    fn quote_token_venue_and_fee_scale_come_from_the_listing() {
        let rows = catalog(serde_json::json!({
            "outcomes": [
                {"outcome": 1, "name": "A", "description": "", "quoteToken": "USDH",
                 "venue": "out", "deployerFeeScale": "1.5",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 2, "name": "Recurring", "description": "",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]}
            ]
        }))
        .sides;

        assert_eq!(rows[0].quote_token.as_deref(), Some("USDH"));
        assert_eq!(rows[0].venue.as_deref(), Some("out"));
        assert_eq!(rows[0].deployer_fee_scale, Some("1.5".parse().unwrap()));
        assert_eq!(rows[2].quote_token, None);
        assert_eq!(rows[2].venue, None);
        assert_eq!(rows[2].deployer_fee_scale, None);
    }

    #[test]
    fn a_side_that_cannot_be_encoded_is_reported_and_the_rest_still_load() {
        let catalog = catalog(serde_json::json!({
            "outcomes": [
                {"outcome": 1, "name": "A", "description": "", "sideSpecs": [{"name": "Yes"}]},
                {"outcome": u64::MAX, "name": "Z", "description": "", "sideSpecs": [{"name": "Yes"}]}
            ]
        }));

        assert_eq!(catalog.sides.len(), 1);
        assert_eq!(catalog.skipped.len(), 1);
        assert!(
            catalog.skipped[0].contains("overflowed"),
            "{:?}",
            catalog.skipped
        );
        assert!(catalog.skipped[0].contains(&u64::MAX.to_string()));
    }

    #[test]
    fn extra_sides_are_ignored_with_a_warning_on_the_rows_of_that_outcome() {
        let catalog = catalog(serde_json::json!({
            "outcomes": [{"outcome": 1, "name": "A", "description": "",
                "sideSpecs": [{"name": "Yes"}, {"name": "No"}, {"name": "Maybe"}]}]
        }));

        assert_eq!(catalog.sides.len(), 2);
        for row in &catalog.sides {
            assert_eq!(row.warnings.len(), 1);
            assert!(row.warnings[0].contains("Maybe"), "{:?}", row.warnings);
        }
        assert!(catalog.skipped.is_empty());
    }

    #[test]
    fn an_outcome_without_sides_and_a_malformed_entry_are_skipped_by_name() {
        let catalog = catalog(serde_json::json!({
            "outcomes": [
                {"outcome": 1, "name": "A", "description": "", "sideSpecs": []},
                {"outcome": "x"},
                {"outcome": 3, "name": "C", "description": "", "sideSpecs": [{"name": "Yes"}, {"name": "No"}]}
            ],
            "questions": [{"question": "bad"}]
        }));

        assert_eq!(catalog.sides.len(), 2);
        assert_eq!(catalog.skipped.len(), 3);
        assert!(
            catalog
                .skipped
                .iter()
                .any(|line| line.contains("lists no sides"))
        );
    }

    #[test]
    fn questions_tag_their_named_fallback_and_settled_outcomes() {
        let catalog = catalog(serde_json::json!({
            "outcomes": [
                {"outcome": 10, "name": "fallback", "description": "", "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 11, "name": "A", "description": "", "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 12, "name": "B", "description": "", "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 99, "name": "alone", "description": "", "sideSpecs": [{"name": "Yes"}, {"name": "No"}]}
            ],
            "questions": [{"question": 7, "name": "Q", "description": "",
                "fallbackOutcome": 10, "namedOutcomes": [11, 12, 13], "settledNamedOutcomes": [12]}]
        }));

        let by_outcome = |outcome: u64| {
            catalog
                .sides
                .iter()
                .find(|row| row.outcome == outcome && row.side == 0)
                .unwrap()
        };
        assert_eq!(by_outcome(10).question_role, Some(QuestionRole::Fallback));
        assert_eq!(by_outcome(11).question_role, Some(QuestionRole::Named));
        assert!(!by_outcome(11).settled);
        assert!(by_outcome(12).settled);
        assert_eq!(by_outcome(11).question, Some(7));
        assert_eq!(by_outcome(11).question_name.as_deref(), Some("Q"));
        assert_eq!(by_outcome(99).question, None);
        assert_eq!(by_outcome(99).question_role, None);
        assert_eq!(
            catalog.questions[0].warnings,
            ["outcomes not listed by outcomeMeta: 13"]
        );
    }

    #[test]
    fn template_names_and_side_names_resolve_from_templates_and_the_description() {
        let mut catalog = catalog(serde_json::json!({
            "outcomes": [{"outcome": 5, "name": "template:contest", "venue": "out",
                "description": "shortNameA:PHX|shortNameB:SAS|teamA:Phoenix Suns",
                "sideSpecs": [{"name": "template:{shortNameA}"}, {"name": "template:{shortNameB}"}]}],
            "questions": [{"question": 1, "name": "template:race", "description": "year:2026",
                "fallbackOutcome": 5, "namedOutcomes": [], "settledNamedOutcomes": []}]
        }));
        catalog.resolve_templates(&templates(serde_json::json!([
            {"id": "contest", "role": "question", "name": "{teamA} ({shortNameA}) v {shortNameB}",
             "description": "", "keywords": []},
            {"id": "race", "role": "question", "name": "{year} race", "description": "", "keywords": []}
        ])));

        let row = &catalog.sides[0];
        assert_eq!(row.outcome_name, "Phoenix Suns (PHX) v SAS");
        assert_eq!(row.template.as_deref(), Some("contest"));
        assert_eq!(row.side_name, "PHX");
        assert_eq!(catalog.sides[1].side_name, "SAS");
        assert_eq!(row.question_name.as_deref(), Some("2026 race"));
        assert_eq!(catalog.questions[0].name, "2026 race");
        assert_eq!(catalog.questions[0].template.as_deref(), Some("race"));
        assert!(row.warnings.is_empty(), "{:?}", row.warnings);
    }

    #[test]
    fn unresolvable_template_names_stay_raw_with_the_reason_on_the_row() {
        let meta = serde_json::json!({
            "outcomes": [
                {"outcome": 1, "name": "template:gone", "description": "",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 2, "name": "template:half", "description": "a:1",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]}
            ]
        });
        let mut resolved = catalog(meta.clone());
        resolved.resolve_templates(&templates(serde_json::json!([
            {"id": "half", "role": "question", "name": "{a} {b}", "description": "", "keywords": []}
        ])));
        assert_eq!(resolved.sides[0].outcome_name, "template:gone");
        assert!(resolved.sides[0].warnings[0].contains("not in outcomeTemplates"));
        assert_eq!(resolved.sides[2].outcome_name, "1 {b}");
        assert!(resolved.sides[2].warnings[0].contains("b"));

        let mut offline = catalog(meta);
        offline.resolve_templates(&Err("rate limited".to_string()));
        assert_eq!(offline.sides[0].outcome_name, "template:gone");
        assert!(offline.sides[0].warnings[0].contains("rate limited"));
    }

    fn settled(value: serde_json::Value) -> SettledOutcomeRow {
        settled_row(
            serde_json::from_value(value).unwrap(),
            &templates(serde_json::json!([])),
        )
    }

    #[test]
    fn settled_outcomes_pay_the_fraction_to_side_zero_and_the_rest_to_side_one() {
        let row = settled(serde_json::json!({
            "spec": {"outcome": 95, "name": "Recurring", "description": "d",
                "sideSpecs": [{"name": "Yes"}, {"name": "No"}], "quoteToken": "USDC"},
            "settleFraction": "0.25", "details": "price:76876.9"
        }));

        assert_eq!(
            row.side_payouts,
            ["0.25".parse().unwrap(), "0.75".parse().unwrap()]
        );
        assert_eq!(row.quote_token.as_deref(), Some("USDC"));
        assert_eq!(row.question, None);
        assert!(row.warnings.is_empty());
    }

    #[test]
    fn a_settled_outcome_reports_its_question_state_and_odd_fractions() {
        let row = settled(serde_json::json!({
            "spec": {"outcome": 200, "name": "Netherlands", "description": "d",
                "sideSpecs": [{"name": "Yes"}, {"name": "No"}], "quoteToken": "USDC"},
            "settleFraction": "1.5", "details": "out",
            "question": {"question": {"settled": 32}, "name": "World Cup", "description": ""}
        }));
        assert_eq!(row.question, Some(32));
        assert_eq!(row.question_state.as_deref(), Some("settled"));
        assert_eq!(row.question_name.as_deref(), Some("World Cup"));
        assert!(row.warnings[0].contains("outside 0..=1"));

        let odd = settled(serde_json::json!({
            "spec": {"outcome": 1, "name": "n", "description": "",
                "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
            "settleFraction": "0", "details": "",
            "question": {"question": {"paused": 3}, "name": "Q", "description": ""}
        }));
        assert_eq!(odd.question, None);
        assert!(odd.warnings[0].contains("unrecognized question state"));
    }
}
