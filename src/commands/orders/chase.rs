use std::time::{Duration, Instant};

use hypersdk::Decimal;
use hypersdk::hypercore::Cloid;
use hypersdk::hypercore::types::OrderResponseStatus;
use serde_json::json;

use crate::assets::{AssetResolver, Market};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::TableData;
use crate::quantity::not_positive;
use crate::settings::Settings;

use super::args::{BPS_DENOMINATOR, ChaseArgs, DEFAULT_TRACKING_TIMEOUT, OrderSide, TifArg};
use super::cloid::generated_cloid;
use super::context::OrderSubmissionContext;
use super::exchange::{Placement, Wording, rejection_error};
use super::live_order::{CloidAsset, LiveOrder, elapsed_ms};
use super::planning::{market_dex, market_mid_price, prepare_order_in, resolve_perp_market};
use super::preview::OrderDryRunPlan;
use super::rendering::single_status;
use super::status::Status;
use super::submit::{Ungated, submit_orders};
use super::validation::OrderSpec;

/// Chase stops when the unfilled remainder is worth less than the exchange's
/// minimum order value.
const MIN_NOTIONAL_USDC: i64 = 10;

const EXAMPLE: &str = "orders chase --coin ETH --side buy --size 0.5 --timeout 60s";

fn dry_run_example() -> String {
    format!("hyperliquid --format json --dry-run {EXAMPLE}")
}

/// The dry run and every live requote plan it through here, so the preview shows the
/// price and size that are sent.
fn quote_spec(
    args: &ChaseArgs,
    price: Decimal,
    size: Decimal,
    cloid: Option<Cloid>,
) -> Result<OrderSpec, CliError> {
    Ok(OrderSpec::limit_leg(
        &args.coin,
        args.dex.as_deref(),
        args.side,
        price,
        size,
        TifArg::Alo,
    )?
    .reduce_only(args.reduce_only)
    .with_cloid(cloid))
}

pub(super) async fn chase_dry_run_plan(
    client: &ApiClient,
    resolver: &AssetResolver,
    settings: &Settings,
    args: &ChaseArgs,
    timeout: Duration,
) -> Result<OrderDryRunPlan, CliError> {
    let perp = resolve_perp_market(
        client,
        resolver,
        "orders chase",
        args.dex.as_deref(),
        &args.coin,
    )
    .await?;
    let market = Market::Perp(perp);
    let mid = market_mid_price(client, &market, &args.coin, args.dex.as_deref()).await?;
    let first_quote = prepare_order_in(
        client,
        &market,
        settings,
        &quote_spec(
            args,
            quote_price(mid, args.side, args.offset),
            args.size,
            None,
        )?,
    )
    .await?;
    Ok(OrderDryRunPlan::unattributed(
        "chase",
        dry_run_document(args, timeout, first_quote.price()),
    ))
}

fn dry_run_document(
    args: &ChaseArgs,
    timeout: Duration,
    first_price: Decimal,
) -> serde_json::Value {
    json!({
        "coin": args.coin,
        "dex": args.dex,
        "side": args.side.to_string(),
        "size": args.size.to_string(),
        "offset_bps": args.offset,
        "timeout_secs": timeout.as_secs(),
        "interval_secs": args.interval.as_secs(),
        "max_chase_bps": args.max_chase,
        "tif": "alo",
        "reduce_only": args.reduce_only,
        "first_price": first_price.to_string(),
        "would_execute": "chase",
    })
}

pub(super) async fn chase(
    context: OrderSubmissionContext<'_>,
    resolver: &AssetResolver,
    args: &ChaseArgs,
    timeout: Duration,
) -> Result<(), anyhow::Error> {
    context
        .refuse_unattended_without_yes("chase", &format!("hyperliquid --format json {EXAMPLE}"))?;

    let started = Instant::now();
    let client = context.api;
    let perp = resolve_perp_market(
        client,
        resolver,
        "orders chase",
        args.dex.as_deref(),
        &args.coin,
    )
    .await?;
    let asset = CloidAsset::new(perp.index)?;
    let market = Market::Perp(perp);
    let dex = market_dex(&market);
    let mid = || market_mid_price(client, &market, &args.coin, args.dex.as_deref());
    let start_mid = mid().await?;
    let mut remaining = args.size;
    let mut status = Status::Timeout;
    let mut working: Option<LiveOrder> = None;
    let mut seq: u64 = 0;
    let user = context.account();
    let deadline = Instant::now() + timeout;

    let outcome: Result<(), CliError> = async {
        while Instant::now() < deadline && remaining > Decimal::ZERO {
            let mid = mid().await?;
            if start_mid > Decimal::ZERO {
                let traveled_bps =
                    ((mid - start_mid).abs() / start_mid) * Decimal::from(BPS_DENOMINATOR);
                if traveled_bps > Decimal::from(args.max_chase) {
                    status = Status::MaxChase;
                    break;
                }
            }
            if remaining * mid < Decimal::from(MIN_NOTIONAL_USDC) {
                status = Status::MinNotional;
                break;
            }

            if let Some(quote) = working.take() {
                cancel_working_quote(context, &quote).await?;
            }
            seq += 1;
            let cloid = generated_cloid(seq);
            let price = quote_price(mid, args.side, args.offset);
            let quote_status =
                place_chase_quote(context, &market, args, remaining, price, cloid).await?;
            match Placement::classify(quote_status) {
                Placement::Open { oid, .. } => {
                    working = Some(LiveOrder {
                        oid,
                        cloid,
                        asset,
                        open_size: remaining,
                    });
                }
                Placement::Filled { total_sz, .. } => {
                    remaining = (remaining - total_sz).max(Decimal::ZERO);
                }
                Placement::Rejected(rejected) => {
                    return Err(rejection_error("chase quote rejected: ", &rejected));
                }
            }

            tokio::time::sleep(args.interval).await;

            if let Some(quote) = working.as_mut() {
                let filled = quote.refresh(client, user, dex.clone()).await?;
                remaining = (remaining - filled).max(Decimal::ZERO);
                if quote.is_gone() {
                    working = None;
                }
            }
        }
        Ok(())
    }
    .await;

    if let Some(quote) = working.take()
        && let Err(cleanup) = cancel_working_quote(context, &quote).await
    {
        return Err(match outcome {
            Ok(()) => cleanup.into(),
            Err(err) => anyhow::anyhow!("{err}; chase cleanup also failed: {cleanup}"),
        });
    }
    outcome?;

    if remaining <= Decimal::ZERO {
        status = Status::Filled;
        remaining = Decimal::ZERO;
    }

    context.inv.print(&ChaseOutput {
        status,
        remaining: remaining.to_string(),
        elapsed_ms: elapsed_ms(started),
    });
    Ok(())
}

pub(super) fn chase_timeout(inv: &Invocation, args: &ChaseArgs) -> Result<Duration, CliError> {
    validate_chase_args(args)?;
    effective_timeout(args, requires_explicit_timeout(inv))
}

fn validate_chase_args(args: &ChaseArgs) -> Result<(), CliError> {
    if args.size <= Decimal::ZERO {
        return Err(CliError::Configuration(format!(
            "{}.\n  {}",
            not_positive("orders chase --size"),
            dry_run_example()
        )));
    }
    Ok(())
}

fn effective_timeout(args: &ChaseArgs, require_explicit: bool) -> Result<Duration, CliError> {
    match args.timeout {
        Some(timeout) => Ok(timeout),
        None if require_explicit => Err(CliError::Configuration(format!(
            "orders chase requires --timeout in agent mode.\n  {}",
            dry_run_example()
        ))),
        None => Ok(DEFAULT_TRACKING_TIMEOUT),
    }
}

/// A run nobody watches live cannot assume the 300s default; the shared
/// [`Mode`](crate::mode::Mode) decides that, not a local env read.
fn requires_explicit_timeout(inv: &Invocation) -> bool {
    !inv.mode().live_display()
}

fn quote_price(mid: Decimal, side: OrderSide, offset_bps: u32) -> Decimal {
    let offset = mid * Decimal::from(offset_bps) / Decimal::from(BPS_DENOMINATOR);
    match side {
        OrderSide::Buy => mid - offset,
        OrderSide::Sell => mid + offset,
    }
}

async fn cancel_working_quote(
    context: OrderSubmissionContext<'_>,
    quote: &LiveOrder,
) -> Result<(), CliError> {
    context
        .send(quote.cancel_by_cloid(), Wording::CHASE_CANCEL)
        .await?;
    Ok(())
}

async fn place_chase_quote(
    context: OrderSubmissionContext<'_>,
    market: &Market,
    args: &ChaseArgs,
    size: Decimal,
    price: Decimal,
    cloid: Cloid,
) -> Result<OrderResponseStatus, anyhow::Error> {
    let spec = quote_spec(args, price, size, Some(cloid))?;
    let prepared = prepare_order_in(context.api, market, context.inv.settings(), &spec).await?;
    let statuses = submit_orders(
        context,
        &Ungated::Chase.cleared(),
        prepared.batch(),
        Wording::CHASE_QUOTE,
    )
    .await?;
    Ok(single_status(statuses)?)
}

struct ChaseOutput {
    status: Status,
    remaining: String,
    elapsed_ms: u64,
}

impl TableData for ChaseOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Status", "Remaining", "Elapsed ms"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.status.to_string(),
            self.remaining.clone(),
            self.elapsed_ms.to_string(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        json!({
            "kind": "result",
            "status": self.status,
            "remaining": self.remaining,
            "elapsed_ms": self.elapsed_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_args(timeout: Option<Duration>) -> ChaseArgs {
        ChaseArgs {
            coin: "ETH".to_string(),
            dex: None,
            side: OrderSide::Buy,
            size: Decimal::new(5, 1),
            offset: 5,
            timeout,
            interval: Duration::from_secs(2),
            max_chase: 100,
            reduce_only: false,
            on_behalf_of: None,
            yes: true,
        }
    }

    #[test]
    fn dry_run_includes_bounds_and_first_price() {
        let args = sample_args(Some(Duration::from_secs(60)));
        let plan = dry_run_document(
            &args,
            Duration::from_secs(60),
            quote_price(Decimal::from(100), args.side, args.offset),
        );
        assert_eq!(plan["would_execute"], "chase");
        assert_eq!(plan["offset_bps"], 5);
        assert_eq!(plan["timeout_secs"], 60);
        assert_eq!(plan["interval_secs"], 2);
        assert_eq!(plan["max_chase_bps"], 100);
        assert_eq!(plan["first_price"], "99.95");
        assert_eq!(plan["tif"], "alo");
    }

    #[test]
    fn timeout_is_required_only_when_nobody_watches() {
        assert_eq!(
            effective_timeout(&sample_args(None), false).unwrap(),
            DEFAULT_TRACKING_TIMEOUT
        );
        let unwatched = effective_timeout(&sample_args(None), true).unwrap_err();
        assert_eq!(unwatched.exit_code(), 2);
        assert!(unwatched.to_string().contains("requires --timeout"));
        assert!(unwatched.to_string().contains(&dry_run_example()));
    }

    #[test]
    fn quotes_rest_on_the_passive_side_of_the_mid() {
        assert_eq!(
            quote_price(Decimal::from(100), OrderSide::Buy, 5),
            Decimal::new(9995, 2)
        );
        assert_eq!(
            quote_price(Decimal::from(100), OrderSide::Sell, 5),
            Decimal::new(10005, 2)
        );
    }

    #[test]
    fn a_quote_is_a_post_only_limit_order_that_carries_the_chase_flags() {
        let mut args = sample_args(None);
        args.reduce_only = true;
        let cloid = generated_cloid(1);
        let spec = quote_spec(&args, Decimal::from(100), Decimal::ONE, Some(cloid)).unwrap();
        assert!(spec.reduce_only);
        assert_eq!(spec.cloid, Some(cloid));
        assert!(matches!(
            spec.kind,
            super::super::validation::OrderKind::Limit {
                tif: TifArg::Alo,
                ..
            }
        ));
    }
}
