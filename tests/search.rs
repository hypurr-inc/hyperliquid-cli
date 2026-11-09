mod support;

use serde_json::{Value, json};
use support::API_OVERRIDE_ENV;
use support::IsolatedHome;
use support::exchange::ExchangeFixture;

#[tokio::test]
async fn search_matches_the_query_and_the_type_filter_restricts_the_venue() {
    let server = ExchangeFixture::start().await;
    let home = IsolatedHome::new();
    let search = |venue: &str| -> Value {
        let output = home
            .command()
            .env(API_OVERRIDE_ENV, server.uri())
            .args(["--format", "json", "--select", "venue,symbol"])
            .args(["search", "ETH", "--type", venue])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    };

    assert_eq!(search("perp"), json!([{"venue": "perp", "symbol": "ETH"}]));
    assert_eq!(search("spot"), json!([]));
}
