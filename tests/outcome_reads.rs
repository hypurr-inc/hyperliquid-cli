//! `outcomes list|get|questions|settled|templates` against responses shaped like
//! the live `outcomeMeta`, `outcomeTemplates` and `settledOutcome` ones (trimmed
//! from mainnet, `tests/fixtures/outcomes`).

mod support;

use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::reads::{read, read_json};
use support::{IsolatedHome, json_stdout};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/outcomes/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// An exchange answering `outcomeMeta` with `meta` and the trimmed live
/// `outcomeTemplates`.
async fn exchange_with(meta: Value) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), meta)
        .await;
    server
        .mount_info(
            json!({"type": "outcomeTemplates"}),
            fixture("outcome_templates.json"),
        )
        .await;
    server
}

async fn exchange() -> ExchangeFixture {
    exchange_with(fixture("outcome_meta.json")).await
}

fn outcomes(server: &ExchangeFixture, args: &[&str]) -> Value {
    read_json(server, &[&["outcomes"], args].concat())
}

fn outcome_rows(server: &ExchangeFixture, args: &[&str]) -> Vec<Value> {
    outcomes(server, args).as_array().unwrap().clone()
}

fn side(rows: &[Value], outcome: u64, side: u64) -> &Value {
    rows.iter()
        .find(|row| row["outcome"] == outcome && row["side"] == side)
        .unwrap_or_else(|| panic!("no row for outcome {outcome} side {side}"))
}

#[tokio::test]
async fn list_shows_the_newest_outcome_first_and_oldest_first_restores_the_old_order() {
    let server = exchange().await;

    let newest = outcome_rows(&server, &["list"]);
    let order: Vec<(u64, u64)> = newest
        .iter()
        .map(|row| {
            (
                row["outcome"].as_u64().unwrap(),
                row["side"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(order[..4], [(30337, 0), (30337, 1), (30018, 0), (30018, 1)]);
    assert_eq!(order.last(), Some(&(23745, 1)));
    // Each side row carries the encoding the side trades under.
    assert_eq!(newest[0]["encoding"], 303_370);
    assert_eq!(newest[0]["coin"], "#303370");
    assert_eq!(newest[0]["token"], "+303370");
    assert_eq!(newest[0]["asset_id"], 100_303_370);

    let oldest = outcome_rows(&server, &["list", "--oldest-first"]);
    assert_eq!(oldest[0]["outcome"], 23745);
    assert_eq!(oldest.last().unwrap()["outcome"], 30337);

    let limited = outcome_rows(&server, &["list", "--limit", "3"]);
    assert_eq!(limited.len(), 3);
    assert_eq!(limited[2]["outcome"], 30018);
}

#[tokio::test]
async fn list_resolves_template_names_and_side_names() {
    let server = exchange().await;

    let rows = outcome_rows(&server, &["list", "--limit", "100"]);

    let contest = side(&rows, 27949, 0);
    assert_eq!(
        contest["outcome_name"],
        "NBA Regular Season: Phoenix Suns v San Antonio Spurs"
    );
    assert_eq!(contest["template"], "sportsContestWinner");
    assert_eq!(contest["side_name"], "PHX");
    assert_eq!(side(&rows, 27949, 1)["side_name"], "SAS");
    assert_eq!(side(&rows, 30337, 0)["side_name"], "Yes");
    assert_eq!(side(&rows, 30337, 1)["side_name"], "No");
    assert_eq!(
        side(&rows, 30337, 0)["outcome_name"],
        "BTC above 74402 at 20261109-1600?"
    );
    // A plain name is not touched and is not a template.
    let recurring = side(&rows, 30011, 0);
    assert_eq!(recurring["outcome_name"], "Recurring");
    assert_eq!(recurring["template"], Value::Null);
    // The exchange's own description stays raw.
    assert!(
        contest["description"]
            .as_str()
            .unwrap()
            .contains("shortNameA:PHX")
    );
    for row in &rows {
        assert_eq!(row["warnings"], json!([]), "{row}");
    }
}

#[tokio::test]
async fn list_carries_quote_token_venue_and_deployer_fee_scale() {
    let mut meta = fixture("outcome_meta.json");
    // Live outcomes mix quote tokens (`settledOutcome 1` is USDH).
    for outcome in meta["outcomes"].as_array_mut().unwrap() {
        if outcome["outcome"] == 30337 {
            outcome["quoteToken"] = json!("USDH");
        }
    }
    let server = exchange_with(meta).await;

    let rows = outcome_rows(&server, &["list", "--limit", "100"]);

    let usdh = side(&rows, 30337, 0);
    assert_eq!(usdh["quote_token"], "USDH");
    assert_eq!(usdh["venue"], "skew");
    assert_eq!(usdh["deployer_fee_scale"], "1.0");
    let usdc = side(&rows, 27949, 0);
    assert_eq!(usdc["quote_token"], "USDC");
    assert_eq!(usdc["venue"], "out");
    // No venue and no fee scale on a protocol-recurring outcome.
    let recurring = side(&rows, 30011, 0);
    assert_eq!(recurring["quote_token"], "USDC");
    assert_eq!(recurring["venue"], Value::Null);
    assert_eq!(recurring["deployer_fee_scale"], Value::Null);
    // `txyz` outcomes carry a venue without a fee scale.
    assert_eq!(side(&rows, 23745, 0)["venue"], "txyz");
    assert_eq!(side(&rows, 23745, 0)["deployer_fee_scale"], Value::Null);
}

#[tokio::test]
async fn list_tells_which_outcomes_belong_to_which_question_and_which_is_the_fallback() {
    let server = exchange().await;

    let rows = outcome_rows(&server, &["list", "--limit", "100"]);

    let fallback = side(&rows, 29101, 0);
    assert_eq!(fallback["question"], 620);
    assert_eq!(fallback["question_role"], "fallback");
    assert_eq!(fallback["settled"], false);
    for named in [29102, 29103, 29104] {
        let row = side(&rows, named, 1);
        assert_eq!(row["question"], 620, "{row}");
        assert_eq!(row["question_role"], "named");
    }
    assert_eq!(
        fallback["question_name"],
        "UEFA Nations League C, Matchday 5: Bulgaria v Iceland"
    );
    assert_eq!(side(&rows, 30015, 0)["question_role"], "fallback");
    assert_eq!(side(&rows, 30015, 0)["question"], 627);
    // A standalone outcome is in no question.
    let standalone = side(&rows, 27949, 0);
    assert_eq!(standalone["question"], Value::Null);
    assert_eq!(standalone["question_role"], Value::Null);
    assert_eq!(standalone["question_name"], Value::Null);
}

#[tokio::test]
async fn questions_lists_named_fallback_and_settled_outcomes_newest_first() {
    let server = exchange().await;

    let rows = outcome_rows(&server, &["questions"]);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["question"], 627);
    assert_eq!(rows[0]["name"], "Recurring");
    assert_eq!(rows[0]["fallback_outcome"], 30015);
    assert_eq!(rows[0]["named_outcomes"], json!([30016, 30017, 30018]));
    assert_eq!(rows[1]["question"], 620);
    assert_eq!(rows[1]["template"], "sportsContestResult");
    assert_eq!(
        rows[1]["name"],
        "UEFA Nations League C, Matchday 5: Bulgaria v Iceland"
    );
    assert_eq!(rows[1]["fallback_outcome"], 29101);
    assert_eq!(rows[1]["settled_named_outcomes"], json!([]));
    assert_eq!(rows[1]["warnings"], json!([]));

    let oldest = outcome_rows(&server, &["questions", "--oldest-first"]);
    assert_eq!(oldest[0]["question"], 620);
}

#[tokio::test]
async fn a_question_naming_an_unlisted_outcome_says_so_and_settled_named_outcomes_are_flagged() {
    let mut meta = fixture("outcome_meta.json");
    meta["questions"][1]["namedOutcomes"] = json!([30016, 30017, 30018, 31000]);
    meta["questions"][1]["settledNamedOutcomes"] = json!([30017]);
    let server = exchange_with(meta).await;

    let questions = outcome_rows(&server, &["questions"]);
    assert_eq!(
        questions[0]["warnings"],
        json!(["outcomes not listed by outcomeMeta: 31000"])
    );
    let rows = outcome_rows(&server, &["list", "--limit", "100"]);
    assert_eq!(side(&rows, 30017, 0)["settled"], true);
    assert_eq!(side(&rows, 30016, 0)["settled"], false);
}

#[tokio::test]
async fn one_odd_outcome_is_reported_and_does_not_fail_the_listing() {
    let mut meta = fixture("outcome_meta.json");
    let outcomes = meta["outcomes"].as_array_mut().unwrap();
    outcomes.push(
        json!({"outcome": 40001, "name": "Three way", "description": "",
        "sideSpecs": [{"name": "A"}, {"name": "B"}, {"name": "C"}], "quoteToken": "USDH"}),
    );
    outcomes.push(json!({"outcome": 40002, "name": "Empty", "description": "", "sideSpecs": []}));
    outcomes.push(json!({"outcome": "not a number"}));
    let server = exchange_with(meta).await;

    let assert = read(&server, &["outcomes", "list", "--limit", "100"])
        .success()
        .stderr(predicate::str::contains(
            "outcome 40002 ('Empty') lists no sides",
        ))
        .stderr(predicate::str::contains("outcomeMeta outcome"));
    let rows = json_stdout(&assert).as_array().unwrap().clone();

    let three_way = side(&rows, 40001, 0);
    assert_eq!(three_way["quote_token"], "USDH");
    let warning = three_way["warnings"][0].as_str().unwrap();
    assert!(
        warning.contains("lists 3 sides") && warning.contains('C'),
        "{warning}"
    );
    assert_eq!(
        side(&rows, 40001, 1)["warnings"].as_array().unwrap().len(),
        1
    );
    assert!(rows.iter().all(|row| row["outcome"] != 40002));
    // The rest of the listing is intact.
    assert_eq!(side(&rows, 30337, 0)["warnings"], json!([]));
}

#[tokio::test]
async fn unavailable_templates_keep_raw_names_and_say_why_on_each_affected_row() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), fixture("outcome_meta.json"))
        .await;
    // `outcomeTemplates` is not mounted: the mock server answers 404.

    let rows = outcome_rows(&server, &["list", "--limit", "100"]);

    let contest = side(&rows, 27949, 0);
    assert_eq!(contest["outcome_name"], "template:sportsContestWinner");
    assert!(
        contest["warnings"][0]
            .as_str()
            .unwrap()
            .contains("outcomeTemplates unavailable"),
        "{contest}"
    );
    // Side names resolve from the description alone.
    assert_eq!(contest["side_name"], "PHX");
    // Rows that need no template are untouched.
    assert_eq!(side(&rows, 30011, 0)["warnings"], json!([]));
}

#[tokio::test]
async fn a_template_missing_from_outcome_templates_is_reported_on_the_row() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), fixture("outcome_meta.json"))
        .await;
    let templates: Vec<Value> = fixture("outcome_templates.json")
        .as_array()
        .unwrap()
        .iter()
        .filter(|template| template["id"] != "binaryPrice")
        .cloned()
        .collect();
    server
        .mount_info(json!({"type": "outcomeTemplates"}), json!(templates))
        .await;

    let rows = outcome_rows(&server, &["list", "--limit", "100"]);

    let price = side(&rows, 30337, 0);
    assert_eq!(price["outcome_name"], "template:binaryPrice");
    assert!(
        price["warnings"][0]
            .as_str()
            .unwrap()
            .contains("template 'binaryPrice' is not in outcomeTemplates")
    );
    assert_eq!(side(&rows, 27949, 0)["warnings"], json!([]));
}

#[tokio::test]
async fn get_shows_the_same_fields_as_list_for_one_side() {
    let server = exchange().await;

    let by_coin = outcomes(&server, &["get", "#291011"]);
    let by_token = outcomes(&server, &["get", "+291011"]);

    assert_eq!(by_coin, by_token);
    assert_eq!(by_coin["outcome"], 29101);
    assert_eq!(by_coin["side"], 1);
    assert_eq!(by_coin["quote_token"], "USDC");
    assert_eq!(by_coin["question"], 620);
    assert_eq!(by_coin["question_role"], "fallback");
    assert_eq!(by_coin["asset_id"], 100_000_000 + 291_011);
}

#[tokio::test]
async fn list_table_adds_quote_venue_question_and_role_columns() {
    let server = exchange().await;

    let output = IsolatedHome::new()
        .command_with_server(&server)
        .args(["--format", "table", "outcomes", "list", "--limit", "100"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();

    let header = text.lines().next().unwrap();
    assert!(
        header.ends_with("Quote\tVenue\tQuestion\tRole\tWarnings"),
        "{header}"
    );
    let fallback = text
        .lines()
        .find(|line| line.starts_with("29101\t0\t"))
        .unwrap();
    assert!(
        fallback.contains("\tUSDC\tout\t620\tfallback\t"),
        "{fallback}"
    );
}

#[tokio::test]
async fn settled_reports_the_fraction_the_payouts_and_the_details() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "settledOutcome", "outcome": 95}),
            fixture("settled_95.json"),
        )
        .await;

    let row = outcomes(&server, &["settled", "95"]);

    assert_eq!(row["outcome"], 95);
    assert_eq!(row["name"], "Recurring");
    assert_eq!(row["quote_token"], "USDC");
    assert_eq!(row["settle_fraction"], "0.0");
    assert_eq!(row["side_payouts"], json!(["0", "1"]));
    assert_eq!(row["side_names"], json!(["Yes", "No"]));
    assert_eq!(row["details"], "price:76876.9");
    assert_eq!(row["question"], Value::Null);
    assert_eq!(row["warnings"], json!([]));
}

#[tokio::test]
async fn settled_names_the_question_state_of_a_question_outcome() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "settledOutcome", "outcome": 200}),
            fixture("settled_200.json"),
        )
        .await;

    let row = outcomes(&server, &["settled", "200"]);

    assert_eq!(row["name"], "Netherlands");
    assert_eq!(row["question"], 32);
    assert_eq!(row["question_state"], "settled");
    assert_eq!(row["question_name"], "2026 World Cup Champion");
}

#[tokio::test]
async fn settled_resolves_a_template_name_through_outcome_templates() {
    let server = exchange().await;
    server
        .mount_info(
            json!({"type": "settledOutcome", "outcome": 27949}),
            json!({
                "spec": {"outcome": 27949, "name": "template:sportsContestWinner",
                    "description": "competition:NBA|participantA:Phoenix Suns|participantB:San Antonio Spurs|season:2026/27|stage:Regular Season|shortNameA:PHX|shortNameB:SAS",
                    "sideSpecs": [{"name": "template:{shortNameA}"}, {"name": "template:{shortNameB}"}],
                    "quoteToken": "USDC"},
                "settleFraction": "1.0", "details": "template"
            }),
        )
        .await;

    let row = outcomes(&server, &["settled", "27949"]);

    assert_eq!(
        row["name"],
        "NBA Regular Season: Phoenix Suns v San Antonio Spurs"
    );
    assert_eq!(row["template"], "sportsContestWinner");
    assert_eq!(row["side_names"], json!(["PHX", "SAS"]));
    assert_eq!(row["side_payouts"], json!(["1", "0"]));
}

#[tokio::test]
async fn settled_for_an_unsettled_or_unknown_outcome_is_unsupported() {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(
            json!({"type": "settledOutcome", "outcome": 30337}),
            Value::Null,
        )
        .await;

    read(&server, &["outcomes", "settled", "30337"])
        .code(13)
        .stdout(predicate::str::contains(
            "has not settled or does not exist",
        ));
}

#[tokio::test]
async fn templates_lists_roles_parents_side_names_and_keyword_types() {
    let server = exchange().await;

    let rows = outcome_rows(&server, &["templates"]);

    let by_id = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
    let price = by_id("binaryPrice");
    assert_eq!(price["role"], "standalone_outcome");
    assert_eq!(price["side_names"], json!(["Yes", "No"]));
    assert_eq!(
        price["keywords"][0],
        json!({"keyword": "perp", "type": "hlPerp"})
    );
    assert_eq!(by_id("sportsContestResult")["role"], "question");
    let draw = by_id("sportsContestDraw2");
    assert_eq!(draw["role"], "question_outcome");
    assert_eq!(draw["parent"], "sportsContestResult");
    assert_eq!(rows.len(), 5);
}
