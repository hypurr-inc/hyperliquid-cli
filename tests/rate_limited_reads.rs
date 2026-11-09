//! A rate-limited read ends as exit 11 `rate_limited`, and is retried first,
//! whichever client the command reads through (#42).
//!
//! The exchange answers a limited `/info` read with HTTP 429 and either the
//! body `null` or an nginx page, and an address that has spent its budget with
//! the request-budget text. Reads through `hypersdk` used to decode that body as
//! market metadata and fail with `Couldn't reach ... invalid type: null`.
//!
//! Each command is run against a mock that limits every kind of `/info` read it
//! makes: once (the retry succeeds and the command prints its answer), and
//! always (the command exits 11). The override probe, the first unscoped
//! `allMids`, is never limited: it is sent once and not retried, so a limit on
//! it is an exit 11 at once. The phases of a command run side by side because
//! the CLI's backoff is real time.

mod support;

use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::exchange::{ExchangeFixture, Hip3Dex};
use support::orders::answer_info;
use support::{IsolatedHome, std_command, stdout_json};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, Request, ResponseTemplate};

const USER: &str = "0x0000000000000000000000000000000000000001";
const ADDRESS_LIMIT: &str = "Too many cumulative requests sent (37986 > 10436) for cumulative volume traded $437.92. Place taker orders to free up 1 request per USDC traded.";

#[derive(Clone, Copy, Debug)]
enum Shape {
    /// 65% of the exchange's 429s: `null`, as JSON.
    NullBody,
    /// The rest: the nginx error page.
    Html,
    /// An address over its request budget.
    AddressText,
}

impl Shape {
    fn response(self) -> ResponseTemplate {
        match self {
            Self::NullBody => ResponseTemplate::new(429).set_body_raw("null", "application/json"),
            Self::Html => ResponseTemplate::new(429).set_body_raw(
                "<html>\r\n<head><title>429 Too Many Requests</title></head>\r\n<body>\r\n<center><h1>429 Too Many Requests</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n",
                "text/html",
            ),
            Self::AddressText => ResponseTemplate::new(429).set_body_string(ADDRESS_LIMIT),
        }
    }
}

/// Matches the `/info` reads of one `type` from the `from`-th (0-based) on, at
/// most `count` of them (all of them when `None`).
struct Reads {
    kind: &'static str,
    from: usize,
    count: Option<usize>,
    seen: AtomicUsize,
}

impl Match for Reads {
    fn matches(&self, request: &Request) -> bool {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return false;
        };
        if body["type"] != self.kind {
            return false;
        }
        let nth = self.seen.fetch_add(1, Ordering::SeqCst);
        nth >= self.from && self.count.is_none_or(|count| nth < self.from + count)
    }
}

/// Limit the reads of every kind in `kinds`: the first one each (`times` of
/// them), or all of them. The first unscoped `allMids` is the override probe.
async fn limit(
    fixture: &ExchangeFixture,
    kinds: &[&'static str],
    shape: Shape,
    times: Option<usize>,
) {
    for &kind in kinds {
        Mock::given(method("POST"))
            .and(path("/info"))
            .and(Reads {
                kind,
                from: usize::from(kind == "allMids"),
                count: times,
                seen: AtomicUsize::new(0),
            })
            .respond_with(shape.response())
            .with_priority(1)
            .mount(fixture)
            .await;
    }
}

fn clearinghouse_state() -> Value {
    let summary = json!({
        "accountValue": "42",
        "totalNtlPos": "0",
        "totalRawUsd": "42",
        "totalMarginUsed": "0"
    });
    json!({
        "marginSummary": summary,
        "crossMarginSummary": summary,
        "crossMaintenanceMarginUsed": "0",
        "withdrawable": "42",
        "assetPositions": [],
        "time": 1_700_000_000_000_u64
    })
}

fn usdc_spot_meta_with_hype() -> Value {
    json!({
        "universe": [],
        "tokens": [
            {"name": "USDC", "index": 0, "tokenId": "0x00000000000000000000000000000000", "szDecimals": 8, "weiDecimals": 8, "evmContract": null},
            {"name": "HYPE", "index": 150, "tokenId": "0x00000000000000000000000000000001", "szDecimals": 8, "weiDecimals": 8, "evmContract": null}
        ]
    })
}

/// A command, the reads it makes beyond the fixture's, and the answer it prints.
struct Case {
    args: &'static [&'static str],
    /// A signer is needed (a read of the signer's own account).
    signed: bool,
    dexes: fn() -> Vec<Hip3Dex>,
    extra_reads: fn() -> Vec<(Value, Value)>,
    /// The `type` of every `/info` read the command makes.
    kinds: &'static [&'static str],
    answers: fn(&Value) -> bool,
}

fn no_dexes() -> Vec<Hip3Dex> {
    Vec::new()
}

fn no_reads() -> Vec<(Value, Value)> {
    Vec::new()
}

impl Case {
    async fn fixture(&self) -> ExchangeFixture {
        let fixture = ExchangeFixture::start_with((self.dexes)()).await;
        for (request, response) in (self.extra_reads)() {
            fixture.mount_info_containing(request, response).await;
        }
        fixture
    }

    fn command(&self, home: &IsolatedHome, fixture: &ExchangeFixture) -> Command {
        let mut command = if self.signed {
            home.live_command(fixture)
        } else {
            home.command_with_server(fixture)
        };
        command.args(["--format", "json"]).args(self.args);
        command
    }

    /// Every kind of read is limited once: the command still answers.
    async fn retries(&self, shape: Shape) {
        let (home, fixture) = (IsolatedHome::new(), self.fixture().await);
        limit(&fixture, self.kinds, shape, Some(1)).await;

        let output = finished(self.command(&home, &fixture)).await.success();
        let answer = stdout_json(&output.get_output().stdout);
        assert!(
            (self.answers)(&answer),
            "{:?} {shape:?}: {answer}",
            self.args
        );
        for kind in self.kinds {
            let reads = fixture.info_requests(kind).await.len();
            let probe = usize::from(*kind == "allMids");
            assert!(
                reads >= 2 + probe,
                "{:?} {shape:?}: {kind} was read {reads} times, so its 429 was not retried",
                self.args
            );
        }
    }

    /// Every kind of read is limited for good: the command exits 11.
    async fn stays_limited(&self, shape: Shape) {
        let (home, fixture) = (IsolatedHome::new(), self.fixture().await);
        limit(&fixture, self.kinds, shape, None).await;

        let output = finished(self.command(&home, &fixture)).await.code(11);
        let envelope = stdout_json(&output.get_output().stdout);
        assert_eq!(envelope["exit_code"], 11, "{:?} {shape:?}", self.args);
        assert_eq!(
            envelope["reason"], "rate_limited",
            "{:?} {shape:?}",
            self.args
        );
        let message = envelope["error"].as_str().unwrap_or_default();
        for refused in ["Couldn't reach", "invalid type", "decode"] {
            assert!(
                !message.contains(refused),
                "{:?} {shape:?}: {message}",
                self.args
            );
        }
    }

    /// The phases are independent processes that mostly wait out the CLI's
    /// backoff, so they run side by side.
    async fn run_all(&self) {
        tokio::join!(
            self.retries(Shape::NullBody),
            self.retries(Shape::Html),
            self.retries(Shape::AddressText),
            self.stays_limited(Shape::NullBody),
            self.stays_limited(Shape::Html),
        );
    }
}

/// Run `command` to completion without holding up the other phases.
async fn finished(mut command: Command) -> assert_cmd::assert::Assert {
    tokio::task::spawn_blocking(move || command.assert())
        .await
        .unwrap()
}

#[tokio::test]
async fn perps_list_through_a_rate_limit() {
    Case {
        args: &["perps", "list"],
        signed: false,
        dexes: no_dexes,
        extra_reads: no_reads,
        kinds: &["meta", "spotMeta"],
        answers: |answer| answer.as_array().is_some_and(|rows| rows.len() == 2),
    }
    .run_all()
    .await;
}

#[tokio::test]
async fn perps_get_through_a_rate_limit() {
    Case {
        args: &["perps", "get", "BTC"],
        signed: false,
        dexes: no_dexes,
        extra_reads: no_reads,
        kinds: &["meta", "spotMeta"],
        answers: |answer| answer["name"] == "BTC",
    }
    .run_all()
    .await;
}

/// The command of the issue: a dex-scoped read of an account's orders.
#[tokio::test]
async fn account_orders_on_a_dex_through_a_rate_limit() {
    Case {
        args: &["account", "orders", USER, "--dex", "xyz"],
        signed: false,
        dexes: || vec![Hip3Dex::xyz()],
        extra_reads: || vec![(json!({"type": "frontendOpenOrders"}), json!([]))],
        kinds: &["perpDexs", "meta", "spotMeta", "frontendOpenOrders"],
        answers: |answer| answer.as_array().is_some_and(Vec::is_empty),
    }
    .run_all()
    .await;
}

#[tokio::test]
async fn account_portfolio_through_a_rate_limit() {
    Case {
        args: &["account", "portfolio", USER],
        signed: false,
        dexes: no_dexes,
        extra_reads: || {
            vec![
                (json!({"type": "userAbstraction"}), json!("disabled")),
                (json!({"type": "clearinghouseState"}), clearinghouse_state()),
                (
                    json!({"type": "spotClearinghouseState"}),
                    json!({"balances": []}),
                ),
                (json!({"type": "userVaultEquities"}), json!([])),
            ]
        },
        kinds: &[
            "userAbstraction",
            "clearinghouseState",
            "spotClearinghouseState",
            "userVaultEquities",
        ],
        answers: |answer| answer["summary"]["account_value"] == "42",
    }
    .run_all()
    .await;
}

#[tokio::test]
async fn positions_list_through_a_rate_limit() {
    Case {
        args: &["positions", "list"],
        signed: true,
        dexes: no_dexes,
        extra_reads: || vec![(json!({"type": "clearinghouseState"}), clearinghouse_state())],
        kinds: &["clearinghouseState"],
        answers: |answer| answer.as_array().is_some_and(Vec::is_empty),
    }
    .run_all()
    .await;
}

#[tokio::test]
async fn status_through_a_rate_limit() {
    Case {
        args: &["status"],
        signed: false,
        dexes: no_dexes,
        extra_reads: no_reads,
        kinds: &["meta", "allMids"],
        answers: |answer| answer["perps_count"] == 2 && answer["mids_count"] == 2,
    }
    .run_all()
    .await;
}

#[tokio::test]
async fn borrowlend_rates_through_a_rate_limit() {
    Case {
        args: &["borrowlend", "rates"],
        signed: false,
        dexes: no_dexes,
        extra_reads: || {
            vec![
                (json!({"type": "spotMeta"}), usdc_spot_meta_with_hype()),
                (
                    json!({"type": "allBorrowLendReserveStates"}),
                    json!([[0, {
                        "borrowYearlyRate": "0.05",
                        "supplyYearlyRate": "0.0142",
                        "balance": "1",
                        "utilization": "0.3",
                        "oraclePx": "1.0",
                        "ltv": "0.0",
                        "totalSupplied": "10",
                        "totalBorrowed": "3"
                    }]]),
                ),
            ]
        },
        kinds: &["allBorrowLendReserveStates", "spotMeta"],
        answers: |answer| answer[0]["token"] == "USDC",
    }
    .run_all()
    .await;
}

/// `book` ends on a WebSocket that cannot be pointed at a mock, so the CLI is
/// stopped once it has looked the market up on its dex: past the limited reads.
#[tokio::test]
async fn book_looks_its_market_up_through_a_rate_limit() {
    let (home, fixture) = (
        IsolatedHome::new(),
        ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await,
    );
    limit(
        &fixture,
        &["perpDexs", "meta", "spotMeta"],
        Shape::NullBody,
        Some(1),
    )
    .await;
    let mut command = home.command_with_server(&fixture);
    command.args(["--format", "json", "book", "xyz:XYZ100"]);

    let mut child = std_command(&command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the CLI starts");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut exit_code = None;
    loop {
        let looked_up = fixture
            .info_requests("meta")
            .await
            .iter()
            .any(|body| body["dex"] == "xyz");
        if looked_up {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            exit_code = status.code();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the CLI never looked the market up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if exit_code.is_none() {
        child.kill().ok();
    }
    let output = child.wait_with_output().unwrap();

    assert_ne!(
        exit_code,
        Some(11),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        fixture
            .info_requests("meta")
            .await
            .iter()
            .any(|body| body["dex"] == "xyz"),
        "the dex's meta was read after the limited reads were retried"
    );
}

#[tokio::test]
async fn book_stays_limited_as_a_rate_limit_not_an_unknown_market() {
    let (home, fixture) = (
        IsolatedHome::new(),
        ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await,
    );
    limit(
        &fixture,
        &["perpDexs", "meta", "spotMeta"],
        Shape::Html,
        None,
    )
    .await;
    let mut command = home.command_with_server(&fixture);
    command.args(["--format", "json", "book", "xyz:XYZ100"]);

    let output = command.assert().code(11);
    let envelope = stdout_json(&output.get_output().stdout);
    assert_eq!(envelope["reason"], "rate_limited");
}

#[tokio::test]
async fn a_429_on_the_override_probe_exits_11_not_unreachable() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    answer_info(
        &server,
        json!({}),
        429,
        "Too many requests: rate limit exceeded",
    )
    .await;

    env.command_with_server(&server)
        .args(["perps", "list"])
        .assert()
        .code(11)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Too many requests to the Hyperliquid API",
        ))
        .stderr(predicate::str::contains("Couldn't reach").not());
}

#[tokio::test]
async fn a_structured_rate_limit_error_in_a_200_reply_exits_11() {
    let env = IsolatedHome::new();
    let server = ExchangeFixture::start().await;
    answer_info(
        &server,
        json!({}),
        200,
        r#"{"status": "err", "response": "rate limit exceeded"}"#,
    )
    .await;

    env.command_with_server(&server)
        .arg("status")
        .assert()
        .code(11)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Too many requests to the Hyperliquid API",
        ))
        .stderr(predicate::str::contains("Couldn't reach").not());
}

#[test]
fn an_unreachable_override_endpoint_exits_12_not_rate_limited() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    IsolatedHome::new()
        .command_with_api_url(&url)
        .args(["perps", "list"])
        .assert()
        .code(12)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("127.0.0.1:"))
        .stderr(predicate::str::contains("Too many requests to the Hyperliquid API").not());
}
