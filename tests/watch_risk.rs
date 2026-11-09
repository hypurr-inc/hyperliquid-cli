mod support;

use hyperliquid_cli::env;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::{IsolatedHome, mount_account_state};

const USER: &str = "0x0000000000000000000000000000000000000001";

#[test]
fn watch_risk_requires_bounds_in_agent_mode() {
    IsolatedHome::new()
        .command()
        .env(env::AGENT, "1")
        .args(["--format", "json", "watch", "risk", "--user", USER])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("--max-events 20"))
        .stdout(predicate::str::contains(format!(
            "hyperliquid --format json watch risk --user {USER} --max-events 20"
        )));
}

#[test]
fn watch_risk_rejects_zero_idle_timeout_like_subscribe() {
    IsolatedHome::new()
        .command()
        .args(["watch", "risk", "--user", USER, "--idle-timeout-ms", "0"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--idle-timeout-ms"));
}

/// A server where `USER` has one BTC position, and the `watch risk` command
/// line that reads one `position_lifecycle` alert from it.
async fn one_alert_exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    mount_account_state(&server, vec![], vec![], "9000", "0", 10).await;
    server
}

fn one_alert_args() -> [&'static str; 8] {
    [
        "watch",
        "risk",
        "--user",
        USER,
        "--only",
        "position_lifecycle",
        "--max-events",
        "1",
    ]
}

#[tokio::test]
async fn watch_risk_json_alerts_are_whole_unless_projected() {
    let server = one_alert_exchange().await;
    let alerts = |global_args: &[&str]| -> Vec<Value> {
        let assert = IsolatedHome::new()
            .json_command_for(&server)
            .args(global_args)
            .args(one_alert_args())
            .assert()
            .success();
        String::from_utf8(assert.get_output().stdout.clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };

    let whole = alerts(&[]);
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0]["kind"], "alert");
    assert_eq!(whole[0]["rule"], "position_lifecycle");
    assert_eq!(whole[0]["coin"], "BTC");
    assert!(whole[0].get("payload").is_some());

    assert_eq!(
        alerts(&["--select", "rule,coin"]),
        [json!({"rule": "position_lifecycle", "coin": "BTC"})]
    );

    // `--results-only` is harmless on a stream that has no result wrapper.
    assert_eq!(alerts(&["--results-only"])[0]["rule"], "position_lifecycle");
}

#[tokio::test]
async fn watch_risk_does_not_spend_signing_nonces() {
    let server = one_alert_exchange().await;
    let home = IsolatedHome::new();

    home.json_command_for(&server)
        .args(one_alert_args())
        .assert()
        .success();

    assert!(
        !home.data_dir().join("nonce-high-water").exists(),
        "an alert timestamp is the wall clock, not a nonce"
    );
}

/// What `margin_usage` measures depends on the account abstraction: the perp
/// margin summary only for accounts that keep their balances there.
mod margin_basis {
    use serde_json::{Value, json};

    use crate::support::IsolatedHome;
    use crate::support::exchange::{ExchangeFixture, Hip3Dex};
    use crate::support::wire;

    const USER: &str = "0x0000000000000000000000000000000000000001";

    fn cross_position() -> Value {
        wire::position_with("BTC", json!({"type": "cross", "value": 5}), "10", 50)
    }

    /// The default dex and `xyz`, both collateralised in USDC (token 0), for an
    /// account in `raw_mode` with the given perp state on each dex.
    async fn account(
        raw_mode: &str,
        default_dex: Value,
        xyz: Value,
        spot_usdc: &str,
    ) -> ExchangeFixture {
        let server = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;
        server
            .mount_info(
                json!({"type": "userAbstraction", "user": USER}),
                json!(raw_mode),
            )
            .await;
        server
            .mount_info(
                json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"}),
                xyz,
            )
            .await;
        server
            .mount_info(
                json!({"type": "clearinghouseState", "user": USER}),
                default_dex,
            )
            .await;
        server
            .mount_info(
                json!({"type": "spotClearinghouseState", "user": USER}),
                json!({"balances": [{
                    "coin": "USDC", "token": 0, "hold": "0",
                    "total": spot_usdc, "entryNtl": "0"
                }]}),
            )
            .await;
        server
            .mount_info(
                json!({"type": "frontendOpenOrders", "user": USER}),
                json!([]),
            )
            .await;
        server
            .mount_info(json!({"type": "userFills", "user": USER}), json!([]))
            .await;
        server
    }

    /// The JSON alert lines `watch risk --only margin_usage` prints before it
    /// has read `ticks` snapshots.
    fn margin_alerts(server: &ExchangeFixture, ticks: &str) -> Vec<Value> {
        let assert = IsolatedHome::new()
            .json_command_for(server)
            .args(["watch", "risk", "--user", USER])
            .args(["--only", "margin_usage", "--interval", "100ms"])
            .args(["--idle-timeout-ms", "1500", "--max-events", ticks])
            .assert()
            .success();
        String::from_utf8(assert.get_output().stdout.clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_standard_account_is_measured_by_its_perp_margin_summary() {
        for raw_mode in ["disabled", "default", "dexAbstraction"] {
            let server = account(
                raw_mode,
                wire::perp_state_with("100", "85", "5", "0", vec![cross_position()]),
                wire::perp_state_with("0", "0", "0", "0", vec![]),
                "0",
            )
            .await;

            let alerts = margin_alerts(&server, "1");

            assert_eq!(alerts.len(), 1, "{raw_mode}");
            assert_eq!(alerts[0]["rule"], "margin_usage");
            assert_eq!(alerts[0]["message"], "margin used 85% of equity");
            assert_eq!(alerts[0]["payload"], json!({"margin_used_pct": "85"}));
            assert!(
                server
                    .info_requests("spotClearinghouseState")
                    .await
                    .is_empty()
            );
            assert!(server.info_requests("borrowLendUserState").await.is_empty());
        }
    }

    #[tokio::test]
    async fn a_unified_account_is_measured_by_the_unified_account_ratio() {
        // Maintenance margin 500 on the default dex and 400 on xyz, over 1100 of
        // USDC less the 100 an isolated position holds: 900 / 1000. The default
        // dex's own summary says 1% used and would never alert.
        let default_dex = wire::perp_state_with(
            "10000",
            "100",
            "500",
            "0",
            vec![
                cross_position(),
                wire::position_with(
                    "ETH",
                    json!({"type": "isolated", "value": 3, "rawUsd": "100"}),
                    "100",
                    50,
                ),
            ],
        );
        let server = account(
            "unifiedAccount",
            default_dex,
            wire::perp_state_with("0", "0", "400", "0", vec![]),
            "1100",
        )
        .await;

        let alerts = margin_alerts(&server, "2");

        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0]["rule"], "margin_usage");
        assert_eq!(
            alerts[0]["message"],
            "unified account ratio 90% (maintenance margin over collateral)"
        );
        assert_eq!(
            alerts[0]["payload"],
            json!({
                "margin_used_pct": "90",
                "basis": "unified_account_ratio",
                "collateral_token": 0
            })
        );
        // The dex list and each dex's collateral are read once, not per tick.
        assert_eq!(
            server
                .info_requests("meta")
                .await
                .iter()
                .filter(|body| body["dex"] == "xyz")
                .count(),
            1
        );
        let xyz_states = server.info_requests("clearinghouseState").await;
        assert_eq!(
            xyz_states
                .iter()
                .filter(|body| body["dex"] == "xyz")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn a_unified_account_with_no_collateral_left_for_its_maintenance_margin_alerts() {
        // 500 of maintenance margin and a USDC balance of 0: there is no ratio
        // to compute, which is the most dangerous state, not a quiet one.
        let server = account(
            "unifiedAccount",
            wire::perp_state_with("100", "1", "500", "0", vec![cross_position()]),
            wire::perp_state_with("0", "0", "0", "0", vec![]),
            "0",
        )
        .await;

        let alerts = margin_alerts(&server, "1");

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["rule"], "margin_usage");
        assert_eq!(
            alerts[0]["payload"],
            json!({
                "basis": "unified_account_ratio",
                "collateral_token": 0,
                "uncovered": true
            })
        );
    }

    #[tokio::test]
    async fn a_unified_account_below_the_limit_stays_quiet_whatever_its_perp_summary_says() {
        // The perp summary claims 95% used; the account has 10000 of USDC.
        let server = account(
            "unifiedAccount",
            wire::perp_state_with("100", "95", "50", "0", vec![cross_position()]),
            wire::perp_state_with("0", "0", "0", "0", vec![]),
            "10000",
        )
        .await;

        assert!(margin_alerts(&server, "1").is_empty());
        assert!(
            !server
                .info_requests("spotClearinghouseState")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_portfolio_margin_account_alerts_on_the_health_it_reports() {
        let server = account(
            "portfolioMargin",
            wire::perp_state_with("100", "95", "50", "0", vec![cross_position()]),
            wire::perp_state_with("0", "0", "0", "0", vec![]),
            "10000",
        )
        .await;
        server
            .mount_info(
                json!({"type": "borrowLendUserState", "user": USER}),
                json!({"tokenToState": [], "health": "liquidatable", "healthFactor": 0.97}),
            )
            .await;

        let alerts = margin_alerts(&server, "1");

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["rule"], "margin_usage");
        assert_eq!(
            alerts[0]["message"],
            "portfolio margin health is liquidatable"
        );
        assert_eq!(
            alerts[0]["payload"],
            json!({
                "basis": "portfolio_margin_health",
                "health": "liquidatable",
                "health_factor": "0.97"
            })
        );
    }

    #[tokio::test]
    async fn a_healthy_portfolio_margin_account_stays_quiet_whatever_its_perp_summary_says() {
        let server = account(
            "portfolioMargin",
            wire::perp_state_with("100", "95", "50", "0", vec![cross_position()]),
            wire::perp_state_with("0", "0", "0", "0", vec![]),
            "10000",
        )
        .await;
        server
            .mount_info(
                json!({"type": "borrowLendUserState", "user": USER}),
                json!({"tokenToState": [], "health": "healthy", "healthFactor": null}),
            )
            .await;

        assert!(margin_alerts(&server, "1").is_empty());
        assert!(!server.info_requests("borrowLendUserState").await.is_empty());
    }
}
