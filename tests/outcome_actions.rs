//! `outcomes split|merge|merge-question|negate`: the HIP-4 `userOutcome` L1 action.

mod support;

use hypersdk::hypercore::Chain;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, default_reply};
use support::signing::{recover_signer, test_signer};
use support::{IsolatedHome, VALID_PRIVATE_KEY, expected_address, json_stdout};

/// A question (7) with a fallback (10) and two named outcomes (11 USDC, 12 USDH),
/// and a standalone outcome (99).
fn meta() -> Value {
    let sides = json!([{"name": "Yes"}, {"name": "No"}]);
    json!({
        "outcomes": [
            {"outcome": 10, "name": "Fallback", "description": "other", "sideSpecs": sides, "quoteToken": "USDC"},
            {"outcome": 11, "name": "A", "description": "", "sideSpecs": sides, "quoteToken": "USDC"},
            {"outcome": 12, "name": "B", "description": "", "sideSpecs": sides, "quoteToken": "USDH"},
            {"outcome": 99, "name": "Alone", "description": "", "sideSpecs": sides, "quoteToken": "USDH"}
        ],
        "questions": [{"question": 7, "name": "Q", "description": "",
            "fallbackOutcome": 10, "namedOutcomes": [11, 12], "settledNamedOutcomes": []}]
    })
}

async fn exchange() -> ExchangeFixture {
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), meta())
        .await;
    server.mount_exchange(default_reply()).await;
    server
}

/// The one `/exchange` request the command sent: a nonce and no vault.
async fn sent_body(server: &ExchangeFixture) -> Value {
    let bodies = server.exchange_bodies().await;
    assert_eq!(bodies.len(), 1, "exactly one exchange request: {bodies:?}");
    let body = bodies[0].clone();
    assert!(
        body["nonce"].as_u64().is_some_and(|nonce| nonce > 0),
        "{body}"
    );
    assert!(
        body.get("vaultAddress").is_none_or(Value::is_null),
        "{body}"
    );
    body
}

/// [`sent_body`]'s action, after checking the test key signed it.
async fn sent_action(server: &ExchangeFixture) -> Value {
    let body = sent_body(server).await;
    assert_eq!(recover_signer(&body, Chain::Testnet), test_signer());
    body["action"].clone()
}

#[tokio::test]
async fn split_sends_the_documented_action_and_reports_the_quote_token() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let assert = env
        .live_json(&server)
        .arg("outcomes")
        .args(&["split", "--outcome", "12", "--amount", "10.50"])
        .assert()
        .success();

    assert_eq!(
        sent_action(&server).await,
        json!({"type": "userOutcome", "splitOutcome": {"outcome": 12, "amount": "10.5"}})
    );
    assert_eq!(
        json_stdout(&assert),
        json!([{
            "action": "split-outcome", "status": "confirmed", "outcome": 12,
            "amount": "10.5", "quote_token": "USDH"
        }])
    );
}

#[tokio::test]
async fn an_unreadable_outcome_meta_entry_is_named_on_stderr_and_the_rest_still_convert() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let mut listed = meta();
    listed["outcomes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"outcome": "not a number"}));
    server
        .override_info(json!({"type": "outcomeMeta"}), listed)
        .await;

    env.live_json(&server)
        .arg("outcomes")
        .args(&["split", "--outcome", "12", "--amount", "10.50"])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("outcomeMeta outcome")
                .and(predicate::str::contains("was not read")),
        );
}

#[tokio::test]
async fn merge_sends_an_amount_or_null_for_the_maximum() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    env.live_json(&server)
        .arg("outcomes")
        .args(&["merge", "--outcome", "11", "--amount", "3"])
        .assert()
        .success();
    assert_eq!(
        sent_action(&server).await,
        json!({"type": "userOutcome", "mergeOutcome": {"outcome": 11, "amount": "3"}})
    );

    let env = IsolatedHome::new();
    let server = exchange().await;
    let assert = env
        .live_json(&server)
        .arg("outcomes")
        .args(&["merge", "--outcome", "11", "--max"])
        .assert()
        .success();
    // hypersdk cannot read a null amount, so the signer is not recovered here.
    assert_eq!(
        sent_body(&server).await["action"],
        json!({"type": "userOutcome", "mergeOutcome": {"outcome": 11, "amount": null}})
    );
    assert_eq!(json_stdout(&assert)[0]["amount"], "max");
}

#[tokio::test]
async fn merge_question_sends_the_question_id() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    let assert = env
        .live_json(&server)
        .arg("outcomes")
        .args(&["merge-question", "--question", "7", "--amount", "2.5"])
        .assert()
        .success();

    assert_eq!(
        sent_action(&server).await,
        json!({"type": "userOutcome", "mergeQuestion": {"question": 7, "amount": "2.5"}})
    );
    let row = &json_stdout(&assert)[0];
    assert_eq!(row["action"], "merge-question");
    assert_eq!(row["question"], 7);
    assert!(row.get("outcome").is_none(), "{row}");
    // A question spanning two quote tokens names both.
    assert_eq!(row["quote_token"], "USDC/USDH");
}

#[tokio::test]
async fn negate_sends_question_outcome_and_amount() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    env.live_json(&server)
        .arg("outcomes")
        .args(&[
            "negate",
            "--question",
            "7",
            "--outcome",
            "10",
            "--amount",
            "1",
        ])
        .assert()
        .success();

    assert_eq!(
        sent_action(&server).await,
        json!({"type": "userOutcome", "negateOutcome": {"question": 7, "outcome": 10, "amount": "1"}})
    );
}

#[tokio::test]
async fn dry_run_previews_the_exact_action_and_sends_nothing() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let signer = expected_address(VALID_PRIVATE_KEY);

    let assert = env
        .live_command(&server)
        .args([
            "--format",
            "json",
            "--dry-run",
            "--testnet",
            "outcomes",
            "split",
            "--outcome",
            "12",
            "--amount",
            "4",
        ])
        .assert()
        .success();

    assert_eq!(
        json_stdout(&assert),
        json!({
            "dry_run": true,
            "command": "outcomes split",
            "would_execute": "split_outcome",
            "args": {
                "outcome": 12,
                "question": null,
                "amount": "4",
                "quote_token": "USDH",
                "action": {"type": "userOutcome", "splitOutcome": {"outcome": 12, "amount": "4"}}
            },
            "network": "testnet",
            "signer": signer.clone(),
            "acting_as": signer,
            "vault_address": null
        })
    );
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn unknown_outcomes_and_questions_are_refused_before_anything_is_signed() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    env.live_json(&server)
        .arg("outcomes")
        .args(&["split", "--outcome", "5", "--amount", "1"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "outcome 5 is not listed by outcomeMeta",
        ))
        .stdout(predicate::str::contains("outcomes settled 5"));
    env.live_json(&server)
        .arg("outcomes")
        .args(&["merge-question", "--question", "8", "--max"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "question 8 is not listed by outcomeMeta",
        ));
    env.live_json(&server)
        .arg("outcomes")
        .args(&[
            "negate",
            "--question",
            "7",
            "--outcome",
            "99",
            "--amount",
            "1",
        ])
        .assert()
        .code(13)
        .stdout(predicate::str::contains(
            "outcome 99 is not part of question 7",
        ));

    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn bad_amounts_are_configuration_errors_and_merge_needs_exactly_one_of_amount_and_max() {
    let env = IsolatedHome::new();
    let server = exchange().await;

    for amount in ["0", "-1"] {
        env.live_json(&server)
            .arg("outcomes")
            .args(&["split", "--outcome", "11", "--amount", amount])
            .assert()
            .code(2)
            .stdout(predicate::str::contains("amount must be greater than zero"));
    }
    env.live_json(&server)
        .arg("outcomes")
        .args(&["merge", "--outcome", "11"])
        .assert()
        .code(2);
    env.live_json(&server)
        .arg("outcomes")
        .args(&["merge", "--outcome", "11", "--amount", "1", "--max"])
        .assert()
        .code(2);
    assert!(server.exchange_actions().await.is_empty());
}

#[tokio::test]
async fn an_exchange_rejection_is_an_error_not_a_confirmation() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    server
        .mount_info(json!({"type": "outcomeMeta"}), meta())
        .await;
    server
        .mount_exchange(json!({"status": "err", "response": "Insufficient balance"}))
        .await;

    env.live_json(&server)
        .arg("outcomes")
        .args(&["split", "--outcome", "11", "--amount", "1"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Insufficient balance"))
        .stdout(predicate::str::contains("confirmed").not());
}

#[tokio::test]
async fn mainnet_asks_for_confirmation_and_yes_skips_it() {
    let env = IsolatedHome::new();
    let server = exchange().await;
    let mainnet = |args: &[&str]| {
        let mut command = env.live_command(&server);
        command.args(["outcomes"]).args(args);
        command
    };

    mainnet(&["split", "--outcome", "11", "--amount", "5"])
        .write_stdin("n\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "Split 5 USDC into 5 shares of each side of outcome 11",
        ))
        .stderr(predicate::str::contains("outcome conversion aborted"));
    assert!(server.exchange_actions().await.is_empty());

    mainnet(&["merge", "--outcome", "11", "--max"])
        .write_stdin("n\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "Merge the maximum available shares of each side of outcome 11 into USDC",
        ));
    assert!(server.exchange_actions().await.is_empty());

    mainnet(&["split", "--outcome", "11", "--amount", "5"])
        .env("HYPERLIQUID_AGENT", "1")
        .assert()
        .code(13)
        .stderr(predicate::str::contains(
            "without confirmation in non-interactive mode",
        ));
    assert!(server.exchange_actions().await.is_empty());

    mainnet(&["split", "--outcome", "11", "--amount", "5"])
        .write_stdin("yes\n")
        .assert()
        .success();
    assert_eq!(server.exchange_actions().await.len(), 1);

    mainnet(&["split", "--outcome", "11", "--amount", "5", "--yes"])
        .assert()
        .success();
    assert_eq!(server.exchange_actions().await.len(), 2);
}

#[tokio::test]
async fn schema_declares_funds_movement_dry_run_confirmation_and_l1_signing_for_every_conversion() {
    let env = IsolatedHome::new();

    for command in ["split", "merge", "merge-question", "negate"] {
        let assert = env
            .command()
            .args(["--format", "json", "schema", "outcomes", command])
            .assert()
            .success();
        let schema = json_stdout(&assert);
        let meta = &schema;
        assert_eq!(meta["risk"], "funds_movement", "{command}");
        assert_eq!(meta["dry_run"], "optional", "{command}");
        assert_eq!(meta["confirmation"], "prompt", "{command}");
        assert_eq!(meta["bankr_signer"], "supported", "{command}");
        assert_eq!(meta["raw_payload"], "dry_run_only", "{command}");
        assert_eq!(
            meta["ows_signer"], "experimental_feature_gated",
            "{command}"
        );
        assert_eq!(meta["confirmation_bypass"]["arg"], "yes", "{command}");
    }
}
