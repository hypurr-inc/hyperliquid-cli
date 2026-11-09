use std::time::{Duration, Instant};

use hypersdk::hypercore::{Cloid, HttpClient};
use hypersdk::{Address, Decimal};
use serde_json::json;

use crate::assets::{AssetResolver, Market, PerpAsset};
use crate::errors::{CliError, NextCommand, partial_results_fields};
use crate::output::TableData;
use crate::settings::Settings;

use super::exchange::{Placement, rejection_error};
use super::live_order::{
    DEFAULT_TRACKING_TIMEOUT, LiveOrder, elapsed_ms, generated_cloid, send_tracked_action,
};
use super::planning::{
    CreateOrderSubmission, create_dry_run_preview, market_dex, prepare_order_in,
    resolve_perp_market, resolve_trigger_prices, tpsl_dry_run_plan, validate_tpsl_price_ordering,
};
use super::rendering::{CHECK_LIVE_COMMAND, on_behalf_of_flag, partial_results_error};
use super::validation::{BuilderRequest, MarketSizing, OrderSpec, TpslSpec};
use super::{
    BracketArgs, CreateArgs, CreateOrderType, OrderExecutionContext, TifArg, place_prepared_single,
    protect_position,
};

const ENTRY_POLL_INTERVAL: Duration = Duration::from_millis(500);

enum BracketEntry {
    Limit { price: Decimal, size: Decimal },
    Market(MarketSizing),
}

impl BracketEntry {
    fn from_args(args: &BracketArgs) -> Result<Self, CliError> {
        match args.entry {
            CreateOrderType::Market => {
                if args.price.is_some() {
                    return Err(CliError::Configuration(
                        "orders bracket --entry market does not accept --price".to_string(),
                    ));
                }
                match (args.size, args.amount) {
                    (Some(size), None) => Ok(Self::Market(MarketSizing::Size(size))),
                    (None, Some(amount)) => Ok(Self::Market(MarketSizing::Amount(amount))),
                    (None, None) => Err(CliError::Configuration(
                        "orders bracket requires --size or --amount for market entry.\n  \
                         hyperliquid --format json --dry-run orders bracket --coin ETH --side buy --size 0.1 --take-profit +10% --stop-loss -5%"
                            .to_string(),
                    )),
                    (Some(_), Some(_)) => Err(CliError::Configuration(
                        "orders bracket accepts --size or --amount, not both".to_string(),
                    )),
                }
            }
            CreateOrderType::Limit => {
                if args.amount.is_some() {
                    return Err(CliError::Configuration(
                        "orders bracket --entry limit does not accept --amount".to_string(),
                    ));
                }
                match (args.price, args.size) {
                    (Some(price), Some(size)) => Ok(Self::Limit { price, size }),
                    _ => Err(CliError::Configuration(
                        "orders bracket --entry limit requires --price and --size.\n  \
                         hyperliquid --format json --dry-run orders bracket --coin ETH --side buy --entry limit --price 3000 --size 0.1 --take-profit +10% --stop-loss -5%"
                            .to_string(),
                    )),
                }
            }
            other => Err(CliError::Configuration(format!(
                "orders bracket --entry supports market or limit, not {other}"
            ))),
        }
    }

    /// The order to plan. The entry carries the default builder fee, if one is
    /// configured, like any other order placed one at a time.
    fn spec(&self, args: &BracketArgs, cloid: Option<Cloid>) -> Result<OrderSpec, CliError> {
        let spec = match *self {
            Self::Limit { price, size } => OrderSpec::limit_leg(
                &args.coin,
                args.dex.as_deref(),
                args.side,
                price,
                size,
                TifArg::Gtc,
            )?,
            Self::Market(sizing) => OrderSpec::market(
                &args.coin,
                args.dex.as_deref(),
                args.side,
                sizing,
                args.max_slippage_bps,
            )?,
        };
        Ok(spec.with_builder(BuilderRequest::Default).with_cloid(cloid))
    }
}

/// Check the flags before anything is read from the network.
pub fn validate_bracket_args(args: &BracketArgs) -> Result<(), CliError> {
    BracketEntry::from_args(args).map(drop)
}

pub async fn bracket_dry_run_plan(
    client: &HttpClient,
    resolver: &AssetResolver,
    settings: &Settings,
    args: &BracketArgs,
) -> Result<serde_json::Value, CliError> {
    let spec = BracketEntry::from_args(args)?.spec(args, None)?;
    let perp = resolve_perp_market(
        client,
        resolver,
        "orders bracket",
        args.dex.as_deref(),
        &args.coin,
    )
    .await?;
    let entry = prepare_order_in(client, &Market::Perp(perp.clone()), settings, &spec).await?;
    let entry = create_dry_run_preview(
        &CreateOrderSubmission::Single(entry),
        &entry_preview_args(args),
    )?;
    let protection = tpsl_dry_run_plan(
        &perp,
        &TpslSpec::protecting(args, args.size),
        &args.on_behalf_of,
    )?
    .into_args();
    Ok(json!({
        "coin": args.coin,
        "dex": args.dex,
        "side": args.side.to_string(),
        "entry": args.entry.to_string(),
        "entry_timeout_secs": args.entry_timeout.as_secs(),
        "take_profit": args.take_profit.display(),
        "stop_loss": args.stop_loss.display(),
        "would_execute": "bracket",
        "entry_plan": entry,
        "protection_plan": protection,
    }))
}

/// What a `--dry-run` preview of the entry echoes of the flags that made it.
/// Only the preview reads this; the live entry is planned from an [`OrderSpec`].
fn entry_preview_args(args: &BracketArgs) -> CreateArgs {
    CreateArgs {
        coin: args.coin.clone(),
        dex: args.dex.clone(),
        side: args.side,
        price: args.price,
        trigger_price: None,
        size: args.size,
        amount: args.amount,
        order_type: args.entry,
        tif: TifArg::Gtc,
        reduce_only: false,
        max_slippage_bps: args.max_slippage_bps,
        take_profit: None,
        stop_loss: None,
        grouping: None,
        on_behalf_of: args.on_behalf_of.clone(),
        margin_mode: None,
        builder: None,
        builder_fee_rate: None,
        cloid: None,
        yes: false,
    }
}

/// The entry order of a bracket as the user can name it in a command.
#[derive(Clone, Copy)]
struct EntryRef {
    oid: Option<u64>,
    cloid: Cloid,
}

impl EntryRef {
    fn of(order: &LiveOrder) -> Self {
        Self {
            oid: order.oid,
            cloid: order.cloid,
        }
    }

    fn label(&self) -> String {
        self.oid
            .map_or_else(|| format!("{:#x}", self.cloid), |oid| oid.to_string())
    }

    fn cancel_command(&self, args: &BracketArgs) -> String {
        let target = self.oid.map_or_else(
            || format!("--cloid {:#x}", self.cloid),
            |oid| oid.to_string(),
        );
        format!(
            "hyperliquid orders cancel {target}{}",
            on_behalf_of_flag(args.on_behalf_of.as_deref())
        )
    }
}

/// What the entry order is doing while no TP/SL protects it.
#[derive(Clone, Copy)]
enum Exposure {
    /// Still resting on the book: it can fill later.
    Resting,
    /// Filled: a position exists.
    Filled,
}

/// Commands that say what to do after a bracket ended with work left on the
/// exchange, led by the sentence that introduces them.
struct Followup {
    lead: &'static str,
    commands: Vec<String>,
}

/// A bracket in flight: what each step needs to arm protection or to report
/// how it ended.
struct Bracket<'a> {
    context: OrderExecutionContext<'a>,
    args: &'a BracketArgs,
    perp: PerpAsset,
    vault_address: Option<Address>,
    started: Instant,
}

pub async fn bracket(
    context: OrderExecutionContext<'_>,
    args: &BracketArgs,
    vault_address: Option<Address>,
) -> Result<(), anyhow::Error> {
    let entry = BracketEntry::from_args(args)?;
    if context.submission.require_mainnet_confirmation {
        return Err(CliError::Configuration(
            "Can't place a bracket on mainnet without --yes. Rerun with --yes once you approve it:\n    \
             hyperliquid --format json orders bracket --coin ETH --side buy --size 0.1 --take-profit +10% --stop-loss -5% --yes"
                .to_string(),
        )
        .into());
    }

    let started = Instant::now();
    let client = context.submission.api.sdk();
    let cloid = generated_cloid(1)?;
    let spec = entry.spec(args, Some(cloid))?;
    let perp = resolve_perp_market(
        client,
        context.resolver,
        "orders bracket",
        args.dex.as_deref(),
        &args.coin,
    )
    .await?;
    let market = Market::Perp(perp.clone());
    let prepared =
        prepare_order_in(client, &market, context.submission.inv.settings(), &spec).await?;
    let asset = u32::try_from(prepared.request.asset).map_err(|_| {
        CliError::Internal(anyhow::anyhow!(
            "bracket entry asset index {} does not fit cancel-by-cloid",
            prepared.request.asset
        ))
    })?;
    let requested_size = prepared.size();

    // Fail fast before the entry executes: the protection legs must be
    // constructible and correctly ordered for the position side.
    TpslSpec::protecting(args, Some(requested_size)).validate()?;
    let (est_tp, est_sl) = resolve_trigger_prices(
        Some(&args.take_profit),
        Some(&args.stop_loss),
        Some(prepared.price()),
        args.side,
    )?;
    validate_tpsl_price_ordering(args.side.opposite(), est_tp, est_sl, "orders bracket")?;

    let run = Bracket {
        context,
        args,
        perp,
        vault_address,
        started,
    };
    let statuses = place_prepared_single(context, &prepared, vault_address).await?;
    let entry_status = statuses.into_iter().next().ok_or_else(|| {
        CliError::Internal(anyhow::anyhow!("bracket entry returned no order status"))
    })?;

    let mut tracking = match Placement::classify(entry_status) {
        Placement::Filled { total_sz, oid, .. } => {
            let entry = EntryRef {
                oid: Some(oid),
                cloid,
            };
            return run.arm_protection(&entry, total_sz, "armed").await;
        }
        // Accepted without an oid is tracked by cloid and polled like a resting entry.
        Placement::Open { oid, .. } => LiveOrder {
            oid,
            cloid,
            asset,
            open_size: requested_size,
        },
        Placement::Rejected(rejected) => {
            return Err(rejection_error(
                "Couldn't place the bracket entry. Exchange said: ",
                &rejected,
            )
            .into());
        }
    };

    // Poll the entry order itself (not position presence) so pre-existing
    // exposure cannot falsely complete the bracket.
    let timeout = if args.entry_timeout.is_zero() {
        DEFAULT_TRACKING_TIMEOUT
    } else {
        args.entry_timeout
    };
    let user = context.submission.account(vault_address);
    let dex = market_dex(&market);
    while started.elapsed() < timeout {
        tokio::time::sleep(ENTRY_POLL_INTERVAL).await;
        let open_size = match tracking.open_size_now(client, user, dex.clone()).await {
            Ok(open_size) => open_size,
            Err(err) => {
                return Err(run.report_unprotected(
                    &EntryRef::of(&tracking),
                    Exposure::Resting,
                    "Couldn't check it",
                    &err,
                ));
            }
        };
        if open_size <= Decimal::ZERO {
            let filled = requested_size - tracking.open_size;
            return run
                .arm_protection(&EntryRef::of(&tracking), filled, "armed")
                .await;
        }
        tracking.open_size = open_size;
    }

    // Entry timed out: cancel the remainder so it cannot fill unprotected,
    // then protect whatever already filled.
    if let Err(err) = run.cancel_entry(&tracking).await {
        return Err(run.report_unprotected(
            &EntryRef::of(&tracking),
            Exposure::Resting,
            "Couldn't cancel it after the entry timeout",
            &err,
        ));
    }
    let filled = requested_size - tracking.open_size;
    let entry = EntryRef::of(&tracking);
    if filled > Decimal::ZERO {
        run.arm_protection(&entry, filled, "entry_cancelled_protected")
            .await
    } else {
        Err(run.report(
            "entry_cancelled",
            entry.oid,
            format!(
                "Entry order {} timed out unfilled and was cancelled. No position was opened.",
                entry.label()
            ),
            None,
        ))
    }
}

impl Bracket<'_> {
    /// Place the TP/SL for the `filled` size of the entry, or report that
    /// nothing filled.
    async fn arm_protection(
        &self,
        entry: &EntryRef,
        filled: Decimal,
        status: &str,
    ) -> Result<(), anyhow::Error> {
        if filled <= Decimal::ZERO {
            return Err(self.report(
                "entry_unfilled",
                entry.oid,
                format!(
                    "Entry order {} is no longer open and nothing filled. No position was opened.",
                    entry.label()
                ),
                Some(Followup {
                    lead: "Check what is live:",
                    commands: vec![CHECK_LIVE_COMMAND.to_string()],
                }),
            ));
        }
        if let Err(err) = protect_position(
            self.context.submission,
            &self.perp,
            &TpslSpec::protecting(self.args, Some(filled)),
            self.vault_address,
        )
        .await
        {
            return Err(self.report_unprotected(
                entry,
                Exposure::Filled,
                "Couldn't place the take-profit and stop-loss",
                &crate::errors::from_anyhow(err),
            ));
        }
        self.context
            .submission
            .inv
            .print(&self.result(status, filled.to_string()));
        Ok(())
    }

    /// Cancel the remaining entry order so a timed-out bracket cannot fill
    /// unprotected later.
    async fn cancel_entry(&self, tracking: &LiveOrder) -> Result<(), CliError> {
        send_tracked_action(
            self.context.submission,
            tracking.cancel_prefer_oid(),
            self.vault_address,
            "bracket entry cancel failed",
        )
        .await?;
        Ok(())
    }

    fn report_unprotected(
        &self,
        entry: &EntryRef,
        exposure: Exposure,
        failed_step: &str,
        cause: &CliError,
    ) -> anyhow::Error {
        let (state, status, followup) = match exposure {
            Exposure::Resting => (
                "resting",
                "entry_open_unprotected",
                Followup {
                    lead: "Check what is live, then cancel the entry if you don't want it:",
                    commands: vec![
                        CHECK_LIVE_COMMAND.to_string(),
                        entry.cancel_command(self.args),
                    ],
                },
            ),
            Exposure::Filled => (
                "filled",
                "entry_filled_unprotected",
                Followup {
                    lead: "Check what is live, then protect the position:",
                    commands: vec![
                        CHECK_LIVE_COMMAND.to_string(),
                        format!(
                            "hyperliquid orders tpsl --coin {} --take-profit {} --stop-loss {}{}",
                            self.args.coin,
                            self.args.take_profit.display(),
                            self.args.stop_loss.display(),
                            on_behalf_of_flag(self.args.on_behalf_of.as_deref())
                        ),
                    ],
                },
            ),
        };
        self.report(
            status,
            entry.oid,
            format!(
                "Entry order {} is {state} and not protected. {failed_step}: {cause}",
                entry.label()
            ),
            Some(followup),
        )
    }

    /// Print the bracket result for an entry that ended without protection or
    /// without a position, and return the error (exit 15) that tells the user
    /// how to recover. In JSON the printed result is the only document.
    fn report(
        &self,
        status: &str,
        entry_order_id: Option<u64>,
        headline: String,
        followup: Option<Followup>,
    ) -> anyhow::Error {
        let mut result = self.result(status, "0".to_string());
        result.problem = Some(BracketProblem {
            entry_order_id,
            headline: headline.clone(),
            next: followup
                .as_ref()
                .map(|followup| followup.commands.clone())
                .unwrap_or_default(),
        });
        self.context.submission.inv.print(&result);
        match followup {
            Some(followup) => {
                partial_results_error(&headline, followup.lead, &followup.commands).into()
            }
            None => CliError::PartialResults(headline).into(),
        }
    }

    fn result(&self, status: &str, protected_size: String) -> BracketOutput {
        BracketOutput {
            status: status.to_string(),
            coin: self.args.coin.clone(),
            protected_size,
            elapsed_ms: elapsed_ms(self.started),
            problem: None,
        }
    }
}

struct BracketOutput {
    status: String,
    coin: String,
    protected_size: String,
    elapsed_ms: u64,
    /// Set when the bracket ended with work left on the exchange.
    problem: Option<BracketProblem>,
}

/// What went wrong after the entry order was placed, and what to run next.
struct BracketProblem {
    entry_order_id: Option<u64>,
    headline: String,
    next: Vec<String>,
}

impl TableData for BracketOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Status", "Coin", "Protected Size", "Elapsed ms"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.status.clone(),
            self.coin.clone(),
            self.protected_size.clone(),
            self.elapsed_ms.to_string(),
        ]]
    }

    /// One document. A problem adds the standard error keys
    /// ([`partial_results_fields`]), so this result is also the error envelope
    /// and no second document is printed.
    fn to_json_value(&self) -> serde_json::Value {
        let mut document = serde_json::Map::new();
        document.insert("kind".into(), "result".into());
        document.insert("status".into(), self.status.clone().into());
        document.insert("coin".into(), self.coin.clone().into());
        document.insert("protected_size".into(), self.protected_size.clone().into());
        document.insert("elapsed_ms".into(), self.elapsed_ms.into());
        if let Some(problem) = &self.problem {
            document.extend(partial_results_fields(
                problem.headline.clone(),
                problem.next.iter().map(NextCommand::new).collect(),
            ));
            document.insert("entry_order_id".into(), json!(problem.entry_order_id));
        }
        serde_json::Value::Object(document)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;
    use crate::commands::orders::validation::ClosingSize;
    use crate::commands::orders::{OrderSide, TriggerPriceSpec};

    fn sample_args() -> BracketArgs {
        BracketArgs {
            coin: "ETH".to_string(),
            dex: None,
            side: OrderSide::Buy,
            size: Some(Decimal::new(1, 1)),
            amount: None,
            entry: CreateOrderType::Market,
            price: None,
            take_profit: TriggerPriceSpec::Percent(Decimal::from(10)),
            stop_loss: TriggerPriceSpec::Percent(Decimal::from(-5)),
            entry_timeout: Duration::from_secs(300),
            on_behalf_of: None,
            max_slippage_bps: 500,
            yes: true,
        }
    }

    #[test]
    fn market_entry_requires_size_or_amount() {
        let mut args = sample_args();
        args.size = None;
        assert!(validate_bracket_args(&args).is_err());
    }

    #[test]
    fn limit_entry_requires_price() {
        let mut args = sample_args();
        args.entry = CreateOrderType::Limit;
        assert!(validate_bracket_args(&args).is_err());
        args.price = Some(Decimal::from(100));
        assert!(validate_bracket_args(&args).is_ok());
    }

    #[test]
    fn limit_entry_refuses_a_quote_amount() {
        let mut args = sample_args();
        args.entry = CreateOrderType::Limit;
        args.price = Some(Decimal::from(100));
        args.amount = Some(Decimal::from(50));
        let err = validate_bracket_args(&args).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .contains("--entry limit does not accept --amount")
        );
    }

    #[test]
    fn amount_entry_protection_uses_filled_size() {
        // --amount entries have no --size; protection must be built from the
        // confirmed filled size, not the (absent) size flag.
        let mut args = sample_args();
        args.size = None;
        args.amount = Some(Decimal::from(100));
        let protection = TpslSpec::protecting(&args, Some(Decimal::new(25, 3)));
        assert!(matches!(
            protection.close,
            ClosingSize::Fixed { side: OrderSide::Sell, size } if size == Decimal::new(25, 3)
        ));
        assert!(protection.validate().is_ok());
    }

    #[test]
    fn identical_protection_triggers_are_refused_before_the_entry() {
        let mut args = sample_args();
        args.stop_loss = args.take_profit.clone();
        let err = TpslSpec::protecting(&args, Some(Decimal::ONE))
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("cannot be equal"));
    }

    #[test]
    fn short_bracket_percent_triggers_resolve_favorably() {
        // Short entry at 100: +10% TP resolves to 90, -5% SL to 105.
        let mut args = sample_args();
        args.side = OrderSide::Sell;
        let (tp, sl) = resolve_trigger_prices(
            Some(&args.take_profit),
            Some(&args.stop_loss),
            Some(Decimal::from(100)),
            args.side,
        )
        .unwrap();
        assert_eq!(tp, Some(Decimal::from(90)));
        assert_eq!(sl, Some(Decimal::from(105)));
        assert!(
            validate_tpsl_price_ordering(args.side.opposite(), tp, sl, "orders bracket").is_ok()
        );
    }

    #[test]
    fn bracket_result_is_one_json_object() {
        let output = BracketOutput {
            status: "armed".to_string(),
            coin: "ETH".to_string(),
            protected_size: "0.1".to_string(),
            elapsed_ms: 8,
            problem: None,
        };
        let value = output.to_json_value();
        assert!(value.is_object());
        assert_eq!(value["kind"], "result");
        assert!(
            serde_json::from_str::<serde_json::Value>(&value.to_string()).is_ok(),
            "bracket result must parse as one JSON value"
        );
    }

    #[test]
    fn unprotected_bracket_carries_the_partial_results_keys() {
        let output = BracketOutput {
            status: "entry_open_unprotected".to_string(),
            coin: "ETH".to_string(),
            protected_size: "0".to_string(),
            elapsed_ms: 8,
            problem: Some(BracketProblem {
                entry_order_id: Some(77),
                headline: "Entry order 77 is resting and not protected.".to_string(),
                next: vec![
                    CHECK_LIVE_COMMAND.to_string(),
                    "hyperliquid orders cancel 77".to_string(),
                ],
            }),
        };
        let value = output.to_json_value();
        assert_eq!(value["category"], "partial_results");
        assert_eq!(value["exit_code"], 15);
        assert_eq!(
            value["error"],
            "Entry order 77 is resting and not protected."
        );
        assert_eq!(value["entry_order_id"], 77);
        assert_eq!(value["next"][0]["command"], CHECK_LIVE_COMMAND);
        assert_eq!(value["next"][1]["command"], "hyperliquid orders cancel 77");
        assert!(value.get("next_commands").is_none());
    }
}
