//! Top-level JSON output contracts that belong to no one command family,
//! pinned as snapshots in `tests/fixtures/contracts`.

mod contract_support;
mod support;

use contract_support::assert_json_fixture;
use support::exchange::ExchangeFixture;
use support::{API_OVERRIDE_ENV, IsolatedHome, json_command, run_json, run_json_failure};

#[test]
fn a_usage_error_prints_this_json_envelope() {
    let envelope = run_json_failure(json_command().args(["wallet", "show", "--bogus"]), 2);

    assert_json_fixture("output_error_envelope.json", &envelope);
}

#[tokio::test]
async fn results_only_max_results_and_select_compose_on_a_list() {
    let home = IsolatedHome::new();
    let server = ExchangeFixture::start().await;

    let output = run_json(
        home.json_command()
            .env(API_OVERRIDE_ENV, server.uri())
            .args([
                "--results-only",
                "--max-results",
                "2",
                "--select",
                "name,max_leverage",
                "perps",
                "list",
            ]),
    );

    assert_json_fixture("output_projection.json", &output);
}
