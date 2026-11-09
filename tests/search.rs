mod support;

use serde_json::json;
use support::exchange::ExchangeFixture;
use support::reads::read_json;

#[tokio::test]
async fn search_matches_the_query_and_the_type_filter_restricts_the_venue() {
    let server = ExchangeFixture::start().await;
    let search = |venue: &str| {
        read_json(
            &server,
            &["--select", "venue,symbol", "search", "ETH", "--type", venue],
        )
    };

    assert_eq!(search("perp"), json!([{"venue": "perp", "symbol": "ETH"}]));
    assert_eq!(search("spot"), json!([]));
}
