//! The balance a transfer is checked against depends on the account's
//! abstraction: a standard account spends its perp balance, while a unified or
//! portfolio-margin account keeps every balance in the spot state and its perp
//! `withdrawable` is not the real balance.

mod support;

use serde_json::{Value, json};
use support::exchange::ExchangeFixture;
use support::{API_OVERRIDE_ENV, IsolatedHome, PRIVATE_KEY_ENV, VALID_PRIVATE_KEY};

const RECIPIENT: &str = "0x0000000000000000000000000000000000000001";

fn perp_state(withdrawable: &str) -> Value {
    json!({
        "marginSummary": {
            "accountValue": "10000", "totalNtlPos": "0",
            "totalRawUsd": "10000", "totalMarginUsed": "0"
        },
        "crossMarginSummary": {
            "accountValue": "10000", "totalNtlPos": "0",
            "totalRawUsd": "10000", "totalMarginUsed": "0"
        },
        "crossMaintenanceMarginUsed": "0",
        "withdrawable": withdrawable,
        "assetPositions": [],
        "time": 1_700_000_000_000_u64
    })
}

/// An account in the raw `userAbstraction` mode with the given perp
/// `withdrawable` and spot USDC `total` and `hold`.
async fn account_server(
    raw_mode: &str,
    withdrawable: &str,
    total: &str,
    hold: &str,
) -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(json!({"type": "userAbstraction"}), json!(raw_mode))
        .await;
    server
        .mount_info_containing(
            json!({"type": "clearinghouseState"}),
            perp_state(withdrawable),
        )
        .await;
    server
        .mount_info_containing(
            json!({"type": "spotClearinghouseState"}),
            json!({"balances": [
                {"coin": "USDC", "token": 0, "hold": hold, "total": total, "entryNtl": "0"}
            ]}),
        )
        .await;
    server
        .mount_exchange(json!({"status": "ok", "response": {"type": "default"}}))
        .await;
    server
}

/// The argument lists of the three commands that spend perp-side USDC.
fn spending_commands() -> [(&'static str, Vec<&'static str>); 3] {
    [
        (
            "send",
            vec![
                "transfer", "send", "--to", RECIPIENT, "--amount", "100", "--yes",
            ],
        ),
        (
            "withdraw",
            vec![
                "transfer", "withdraw", "--to", RECIPIENT, "--amount", "100", "--yes",
            ],
        ),
        (
            "perp-to-spot",
            vec!["transfer", "perp-to-spot", "--amount", "100", "--yes"],
        ),
    ]
}

fn run(env: &IsolatedHome, server: &ExchangeFixture, args: &[&str]) -> assert_cmd::assert::Assert {
    env.command()
        .env(API_OVERRIDE_ENV, server.uri())
        .env(PRIVATE_KEY_ENV, VALID_PRIVATE_KEY)
        .args(["--format", "json", "--testnet"])
        .args(args)
        .assert()
}

async fn exchange_posts(server: &ExchangeFixture) -> usize {
    server.exchange_actions().await.len()
}

// Each mock server holds file descriptors for the life of the test process, so
// one server serves every command of a case instead of one per command.

#[tokio::test]
async fn a_pooled_account_spends_from_its_spot_balance_not_the_empty_perp_one() {
    // Live unified accounts report a perp `withdrawable` that is not the balance:
    // here 0, while the spot USDC has 1000 less a hold of 100 available.
    for raw_mode in ["unifiedAccount", "portfolioMargin"] {
        let server = account_server(raw_mode, "0", "1000", "100").await;
        for (sent, (name, args)) in spending_commands().into_iter().enumerate() {
            let env = IsolatedHome::new();

            run(&env, &server, &args).success();

            assert_eq!(exchange_posts(&server).await, sent + 1, "{raw_mode} {name}");
        }
    }
}

#[tokio::test]
async fn a_pooled_account_is_refused_by_its_spot_available_balance() {
    // The perp side claims 5000, but total 100 less hold 50 leaves 50 spot USDC.
    for raw_mode in ["unifiedAccount", "portfolioMargin"] {
        let server = account_server(raw_mode, "5000", "100", "50").await;
        for (name, args) in spending_commands() {
            let env = IsolatedHome::new();

            run(&env, &server, &args)
                .code(13)
                .stdout(predicates::str::contains("insufficient balance"))
                .stdout(predicates::str::contains("available spot USDC 50"));

            assert_eq!(exchange_posts(&server).await, 0, "{raw_mode} {name}");
        }
    }
}

#[tokio::test]
async fn standard_and_dex_abstraction_accounts_keep_the_perp_withdrawable_check() {
    for raw_mode in ["disabled", "default", "dexAbstraction"] {
        // Plenty of spot USDC does not help: the perp side is empty.
        let empty_perp = account_server(raw_mode, "20", "1000", "0").await;
        // And an empty spot balance does not matter when the perp side covers it.
        let funded_perp = account_server(raw_mode, "1000", "0", "0").await;
        for (sent, (name, args)) in spending_commands().into_iter().enumerate() {
            let env = IsolatedHome::new();

            run(&env, &empty_perp, &args)
                .code(13)
                .stdout(predicates::str::contains("available perp withdrawable 20"));
            assert_eq!(exchange_posts(&empty_perp).await, 0, "{raw_mode} {name}");

            run(&env, &funded_perp, &args).success();
            assert_eq!(
                exchange_posts(&funded_perp).await,
                sent + 1,
                "{raw_mode} {name}"
            );
        }
    }
}

#[tokio::test]
async fn spot_to_perp_never_reads_the_abstraction() {
    let env = IsolatedHome::new();
    let server = account_server("unifiedAccount", "0", "1000", "0").await;

    run(
        &env,
        &server,
        &["transfer", "spot-to-perp", "--amount", "100", "--yes"],
    )
    .success();

    assert_eq!(server.info_requests("userAbstraction").await.len(), 0);
}

#[tokio::test]
async fn usdc_is_the_spot_token_at_index_zero_not_any_balance_named_usdc() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info_containing(json!({"type": "userAbstraction"}), json!("unifiedAccount"))
        .await;
    server
        .mount_info_containing(
            json!({"type": "spotClearinghouseState"}),
            json!({"balances": [
                {"coin": "USDC", "token": 7, "hold": "0", "total": "1000", "entryNtl": "0"}
            ]}),
        )
        .await;

    run(
        &env,
        &server,
        &[
            "transfer", "send", "--to", RECIPIENT, "--amount", "100", "--yes",
        ],
    )
    .code(13)
    .stdout(predicates::str::contains("available spot USDC 0"));
}
