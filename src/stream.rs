//! Streaming runtime shared by snapshot watch, `subscribe` and `watch risk`.
//!
//! Every stream stops on SIGINT, SIGTERM, and SIGHUP: the loop ends, the
//! terminal is restored, the current output line is complete, and the process
//! exits `128 + signal`. A closed stdout (for example `| head`) ends the stream
//! quietly with exit 0.

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};
use std::pin::Pin;
use std::time::Duration;

use clap::{ArgMatches, Args};
use futures::{Stream, StreamExt};
use hypersdk::Address;
use hypersdk::hypercore::{
    Chain, Incoming, Subscription, UserEvent, WebSocket, ws::Event as WsEvent,
};
use serde::Serialize;
use tokio::time::Sleep;

use crate::command_spec::{EnvZero, StreamBound, StreamEnv};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::output::JsonRenderOptions;
use crate::settings::EnvSource;
use crate::ws::BookAggregationArgs;
use crate::{output, ws};

/// Exit status for SIGINT and the Ctrl+C key (`128 + 2`). A stop signal exits
/// `128 +` its number.
pub(crate) const EXIT_SIGINT: i32 = 130;

/// A polling stream with no explicit idle bound ends after this long without output.
const POLL_DEFAULT_IDLE: Duration = Duration::from_secs(60 * 60);

pub(crate) const JSON_LINES_STDOUT_NOTICE: &str =
    "Streaming JSON lines because stdout isn't a terminal.";
pub(crate) const JSON_LINES_STDIN_NOTICE: &str =
    "Streaming JSON lines because stdin isn't a terminal.";
pub(crate) const JSON_LINES_DRIVEN_NOTICE: &str =
    "Streaming JSON lines because an agent, CI or --non-interactive run is driving.";
pub(crate) const CONNECTION_LOST_NOTICE: &str = "! Lost connection. Retrying automatically…";
const STREAM_GAP_MESSAGE: &str = "Connection was lost and restored. Events may have been missed.";

/// Stdout was closed by the reader (`| head`); the stream should end quietly.
#[derive(Debug)]
pub struct StdoutClosed;

impl fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stdout closed")
    }
}

impl StdError for StdoutClosed {}

pub fn is_stdout_closed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<StdoutClosed>().is_some()
}

fn map_write_error(err: io::Error) -> anyhow::Error {
    if err.kind() == io::ErrorKind::BrokenPipe {
        StdoutClosed.into()
    } else {
        err.into()
    }
}

/// Write `line` plus a newline to `output` and flush. A broken pipe becomes [`StdoutClosed`].
fn write_line<W: Write + ?Sized>(output: &mut W, line: &str) -> Result<(), anyhow::Error> {
    output
        .write_all(line.as_bytes())
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
        .map_err(map_write_error)
}

/// Write one complete line to stdout. A broken pipe becomes [`StdoutClosed`].
pub(crate) fn write_stdout_line(line: &str) -> Result<(), anyhow::Error> {
    write_line(&mut io::stdout().lock(), line)
}

/// Write one compact JSON line, applying `--select`, `--results-only` and
/// `--max-results` with stream-envelope rules.
pub(crate) fn emit_json_line<T, W>(
    output: &mut W,
    item: &T,
    projection: &JsonRenderOptions,
) -> Result<(), anyhow::Error>
where
    T: Serialize + ?Sized,
    W: Write + ?Sized,
{
    let line = if projection.is_active() {
        let value = projection.apply_stream(serde_json::to_value(item)?);
        serde_json::to_string(&value)?
    } else {
        serde_json::to_string(item)?
    };
    write_line(output, &line)
}

pub struct ShutdownSignals {
    /// Each handled signal with the exit status it ends the process with.
    #[cfg(unix)]
    signals: Vec<(tokio::signal::unix::Signal, i32)>,
}

impl ShutdownSignals {
    #[cfg(unix)]
    pub fn install() -> Result<Self, anyhow::Error> {
        use tokio::signal::unix::{SignalKind, signal};

        let signals = [
            (SignalKind::interrupt(), "SIGINT"),
            (SignalKind::terminate(), "SIGTERM"),
            (SignalKind::hangup(), "SIGHUP"),
        ]
        .into_iter()
        .map(|(kind, name)| {
            let handler = signal(kind).map_err(|err| {
                CliError::Internal(anyhow::anyhow!("failed to install {name} handler: {err}"))
            })?;
            Ok((handler, 128 + kind.as_raw_value()))
        })
        .collect::<Result<_, CliError>>()?;
        Ok(Self { signals })
    }

    #[cfg(not(unix))]
    pub fn install() -> Result<Self, anyhow::Error> {
        Ok(Self {})
    }

    #[cfg(unix)]
    pub async fn recv(&mut self) -> i32 {
        let waits = self.signals.iter_mut().map(|(signal, code)| {
            Box::pin(async move {
                match signal.recv().await {
                    Some(()) => *code,
                    None => std::future::pending().await,
                }
            })
        });
        futures::future::select_all(waits).await.0
    }

    #[cfg(not(unix))]
    pub async fn recv(&mut self) -> i32 {
        match tokio::signal::ctrl_c().await {
            Ok(()) => EXIT_SIGINT,
            Err(_) => std::future::pending().await,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    Finished,
    Signal(i32),
}

/// Flush the standard streams and exit with a signal status. Call after cleanup.
fn exit_for_signal(code: i32) -> ! {
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    std::process::exit(code)
}

/// Run `work` until it ends, a stop signal arrives, or stdout closes.
///
/// Anything `work` must restore (the terminal) lives inside it, so it is
/// dropped before the process exits for a signal. Signal handlers are
/// installed before `work` first runs.
pub(crate) async fn run_stream<F>(work: F) -> Result<(), anyhow::Error>
where
    F: Future<Output = Result<StreamEnd, anyhow::Error>>,
{
    let mut signals = ShutdownSignals::install()?;
    let end = tokio::select! {
        end = work => end,
        code = signals.recv() => Ok(StreamEnd::Signal(code)),
    };

    match end {
        Ok(StreamEnd::Finished) => Ok(()),
        Ok(StreamEnd::Signal(code)) => exit_for_signal(code),
        Err(err) if is_stdout_closed(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

/// A count of at least 1.
pub fn parse_positive_usize(raw: &str) -> Result<usize, String> {
    let value = raw
        .parse::<usize>()
        .map_err(|err| format!("invalid positive integer: {err}"))?;
    if value == 0 {
        return Err("value must be at least 1".to_string());
    }
    Ok(value)
}

/// `--watch` / `--max-ticks`, shared by every snapshot command.
#[derive(Args, Debug, Clone, Default)]
pub struct SnapshotWatchArgs {
    /// Watch live updates in-place
    #[arg(short = 'w', long)]
    pub watch: bool,
    /// Stop after rendering this many watch snapshots (useful for automation)
    #[arg(long, value_name = "TICKS", requires = "watch", value_parser = parse_positive_usize)]
    pub max_ticks: Option<usize>,
}

/// `--max-events` / `--idle-timeout-ms`, shared by `subscribe` and `watch risk`.
#[derive(Args, Debug, Clone, Default)]
pub struct StreamBoundArgs {
    /// Stop after emitting this many matching events (useful for automation)
    #[arg(long)]
    pub max_events: Option<usize>,
    /// Stop if no matching events are emitted within this many milliseconds
    #[arg(long, value_name = "MILLISECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    pub idle_timeout_ms: Option<u64>,
}

impl StreamBoundArgs {
    fn idle(&self) -> Option<Duration> {
        self.idle_timeout_ms.map(Duration::from_millis)
    }
}

/// The environment variable behind a bound, read as a count. A value that does
/// not parse is ignored here and rejected by [`validate_env_bound`].
fn read_env_bound(env: StreamEnv, source: &EnvSource) -> Option<usize> {
    source
        .get(env.name)
        .and_then(|raw| parse_env_count(&raw, env.zero))
}

/// `0` is a bound only when the variable's [`EnvZero`] says so.
fn parse_env_count(raw: &str, zero: EnvZero) -> Option<usize> {
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|value| zero == EnvZero::StopImmediately || *value > 0)
}

fn validate_env_bound(env: StreamEnv, source: &EnvSource) -> Result<(), CliError> {
    let Some(raw) = source.get(env.name) else {
        return Ok(());
    };
    raw.trim().parse::<usize>().map(|_| ()).map_err(|err| {
        CliError::Configuration(format!(
            "{} must be a non-negative integer: {err}",
            env.name
        ))
    })
}

/// Resolved stop conditions of one stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamBounds {
    /// Ticks, events or alerts, depending on the stream.
    pub max_items: Option<usize>,
    /// Stop after this long without output.
    pub idle: Option<Duration>,
}

impl StreamBounds {
    /// `--max-ticks`, falling back to `HYPERLIQUID_WATCH_MAX_TICKS`.
    pub fn snapshot_watch(max_ticks: Option<usize>, source: &EnvSource) -> Self {
        Self {
            max_items: max_ticks.or_else(|| {
                StreamBound::SnapshotWatch
                    .env()
                    .and_then(|env| read_env_bound(env, source))
            }),
            idle: None,
        }
    }

    /// `--max-events`, falling back to `HYPERLIQUID_SUBSCRIBE_MAX_EVENTS`, plus
    /// `--idle-timeout-ms`. A bound of `0` is valid: the stream ends after its
    /// `subscribed` line.
    pub fn subscribe(args: &StreamBoundArgs, source: &EnvSource) -> Self {
        Self {
            max_items: args.max_events.or_else(|| {
                StreamBound::Subscribe
                    .env()
                    .and_then(|env| read_env_bound(env, source))
            }),
            idle: args.idle(),
        }
    }

    /// `watch risk` takes its bounds from flags only, and a polling stream
    /// with no explicit idle bound ends after [`POLL_DEFAULT_IDLE`] without output.
    pub fn watch_risk(args: &StreamBoundArgs) -> Self {
        Self {
            max_items: args.max_events,
            idle: args.idle().or(Some(POLL_DEFAULT_IDLE)),
        }
    }

    fn reached(&self, emitted: usize) -> bool {
        self.max_items.is_some_and(|max| emitted >= max)
    }
}

/// Machine contexts must not start an endless stream: reject a run of `bound`'s
/// command, given the command's own matches, that declares no stop condition.
/// The environment variable is consulted, and checked for validity, only when
/// no flag bounds the run.
pub fn require_bound(
    bound: StreamBound,
    matches: &ArgMatches,
    source: &EnvSource,
) -> Result<(), CliError> {
    let internal = |err: clap::parser::MatchesError| CliError::Internal(anyhow::anyhow!(err));
    if let Some(guard) = bound.required_when()
        && !matches
            .try_get_one::<bool>(guard)
            .map_err(internal)?
            .copied()
            .unwrap_or(false)
    {
        return Ok(());
    }
    for id in bound.cli_args() {
        if matches.try_contains_id(id).map_err(internal)? {
            return Ok(());
        }
    }
    if let Some(env) = bound.env() {
        validate_env_bound(env, source)?;
        if read_env_bound(env, source).is_some() {
            return Ok(());
        }
    }
    Err(CliError::Unsupported(bound.unbounded_message()))
}

/// Restarts on every emitted item; never fires when no idle bound is set.
struct IdleTimer(Option<(Duration, Pin<Box<Sleep>>)>);

impl IdleTimer {
    fn new(idle: Option<Duration>) -> Self {
        Self(idle.map(|idle| (idle, Box::pin(tokio::time::sleep(idle)))))
    }

    async fn expired(&mut self) -> Duration {
        match &mut self.0 {
            Some((idle, sleep)) => {
                sleep.as_mut().await;
                *idle
            }
            None => std::future::pending().await,
        }
    }

    fn restart(&mut self) {
        if let Some((idle, sleep)) = &mut self.0 {
            sleep.as_mut().reset(tokio::time::Instant::now() + *idle);
        }
    }
}

/// One poll of a source that yields zero or more items per tick.
pub(crate) trait Ticker {
    type Item;

    async fn tick(&mut self) -> Result<Vec<Self::Item>, anyhow::Error>;
}

/// Poll `ticker` every `interval`, passing each item to `emit`, until the
/// item bound is reached or nothing was emitted for the idle bound.
pub(crate) async fn poll_stream<T: Ticker>(
    ticker: &mut T,
    interval: Duration,
    bounds: StreamBounds,
    mut emit: impl FnMut(T::Item) -> Result<(), anyhow::Error>,
) -> Result<(), anyhow::Error> {
    let mut emitted = 0usize;
    let mut idle = IdleTimer::new(bounds.idle);

    while !bounds.reached(emitted) {
        let items = tokio::select! {
            _ = idle.expired() => return Ok(()),
            items = ticker.tick() => items?,
        };
        for item in items {
            emit(item)?;
            emitted += 1;
            idle.restart();
            if bounds.reached(emitted) {
                return Ok(());
            }
        }

        tokio::select! {
            _ = idle.expired() => return Ok(()),
            () = tokio::time::sleep(interval) => {}
        }
    }
    Ok(())
}

/// WebSocket subscriptions paired with the messages that belong to them.
pub struct SubscribeTarget {
    subscriptions: Vec<Subscription>,
    matches: fn(&Incoming) -> bool,
    skip_snapshot: bool,
}

/// Whether `message` is the `isSnapshot: true` history the exchange sends first
/// on a user subscription, as opposed to a live update. Only the channels whose
/// payload carries the flag are known to have one.
fn is_snapshot(message: &Incoming) -> bool {
    match message {
        Incoming::UserFills { is_snapshot, .. } | Incoming::UserFundings { is_snapshot, .. } => {
            *is_snapshot
        }
        Incoming::UserTwapSliceFills(slice_fills) => slice_fills.is_snapshot,
        _ => false,
    }
}

impl SubscribeTarget {
    fn single(subscription: Subscription, matches: fn(&Incoming) -> bool) -> Self {
        Self {
            subscriptions: vec![subscription],
            matches,
            skip_snapshot: false,
        }
    }

    /// One subscription per dex (`None` is the first perp dex), in order, without repeats.
    fn per_dex(
        dexs: Vec<Option<String>>,
        subscription: impl Fn(Option<String>) -> Subscription,
        matches: fn(&Incoming) -> bool,
    ) -> Self {
        let mut seen = Vec::new();
        for dex in dexs {
            if !seen.contains(&dex) {
                seen.push(dex);
            }
        }
        if seen.is_empty() {
            seen.push(None);
        }
        Self {
            subscriptions: seen.into_iter().map(subscription).collect(),
            matches,
            skip_snapshot: false,
        }
    }

    /// Drop the `isSnapshot: true` first message of a user subscription: it is
    /// neither written nor counted against `--max-events`.
    #[must_use]
    pub fn skipping_snapshot(mut self, skip: bool) -> Self {
        self.skip_snapshot = skip;
        self
    }

    pub fn trades(coin: String) -> Self {
        Self::single(Subscription::Trades { coin }, |message| {
            matches!(message, Incoming::Trades(_))
        })
    }

    pub fn orderbook(coin: String, aggregation: BookAggregationArgs) -> Self {
        Self::single(aggregation.subscription(coin), |message| {
            matches!(message, Incoming::L2Book(_))
        })
    }

    pub fn bbo(coin: String) -> Self {
        Self::single(Subscription::Bbo { coin }, |message| {
            matches!(message, Incoming::Bbo(_))
        })
    }

    pub fn candles(coin: String, interval: String) -> Self {
        Self::single(Subscription::Candle { coin, interval }, |message| {
            matches!(message, Incoming::Candle(_))
        })
    }

    pub fn all_mids(dex: Option<String>) -> Self {
        Self::single(Subscription::AllMids { dex }, |message| {
            matches!(message, Incoming::AllMids { .. })
        })
    }

    pub fn active_asset_ctx(coin: String) -> Self {
        Self::single(Subscription::ActiveAssetCtx { coin }, |message| {
            matches!(
                message,
                Incoming::ActiveAssetCtx { .. } | Incoming::ActiveSpotAssetCtx { .. }
            )
        })
    }

    pub fn active_asset_data(user: Address, coin: String) -> Self {
        Self::single(Subscription::ActiveAssetData { user, coin }, |message| {
            matches!(message, Incoming::ActiveAssetData(_))
        })
    }

    pub fn order_updates(user: Address) -> Self {
        Self::single(Subscription::OrderUpdates { user }, |message| {
            matches!(message, Incoming::OrderUpdates(_))
        })
    }

    pub fn fills(user: Address) -> Self {
        Self::single(Subscription::UserFills { user }, |message| {
            matches!(
                message,
                Incoming::UserFills { .. } | Incoming::UserEvents(UserEvent::Fills { .. })
            )
        })
    }

    pub fn user_events(user: Address) -> Self {
        Self::single(Subscription::UserEvents { user }, |message| {
            matches!(message, Incoming::UserEvents(_))
        })
    }

    pub fn user_fundings(user: Address) -> Self {
        Self::single(Subscription::UserFundings { user }, |message| {
            matches!(message, Incoming::UserFundings { .. })
        })
    }

    pub fn notifications(user: Address) -> Self {
        Self::single(Subscription::Notification { user }, |message| {
            matches!(message, Incoming::Notification { .. })
        })
    }

    pub fn twap_slice_fills(user: Address) -> Self {
        Self::single(Subscription::UserTwapSliceFills { user }, |message| {
            matches!(message, Incoming::UserTwapSliceFills(_))
        })
    }

    pub fn twap_states(user: Address, dexs: Vec<Option<String>>) -> Self {
        Self::per_dex(
            dexs,
            |dex| Subscription::TwapStates { user, dex },
            |message| matches!(message, Incoming::TwapStates { .. }),
        )
    }

    pub fn clearinghouse_state(user: Address, dexs: Vec<Option<String>>) -> Self {
        Self::per_dex(
            dexs,
            |dex| Subscription::ClearinghouseState { user, dex },
            |message| matches!(message, Incoming::ClearinghouseState { .. }),
        )
    }

    pub fn open_orders(user: Address, dexs: Vec<Option<String>>) -> Self {
        Self::per_dex(
            dexs,
            |dex| Subscription::OpenOrders { user, dex },
            |message| matches!(message, Incoming::OpenOrders { .. }),
        )
    }

    pub fn spot_state(user: Address) -> Self {
        Self::single(
            Subscription::SpotState {
                user,
                is_portfolio_margin: None,
            },
            |message| matches!(message, Incoming::SpotState { .. }),
        )
    }

    pub fn subscriptions(&self) -> &[Subscription] {
        &self.subscriptions
    }

    /// The `subscription` text of this target's NDJSON lines: one entry per
    /// subscription, joined by `, `. An aggregated order book names its
    /// aggregation; every other label is the SDK's.
    pub fn label(&self) -> String {
        self.subscriptions
            .iter()
            .map(|subscription| match subscription {
                Subscription::L2Book {
                    coin,
                    n_sig_figs: Some(figs),
                    mantissa,
                    ..
                } => match mantissa {
                    Some(mantissa) => format!("l2Book({coin},nSigFigs={figs},mantissa={mantissa})"),
                    None => format!("l2Book({coin},nSigFigs={figs})"),
                },
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn matches(&self, message: &Incoming) -> bool {
        (self.matches)(message) && !(self.skip_snapshot && is_snapshot(message))
    }
}

/// Stream a WebSocket subscription to stdout as NDJSON.
///
/// A human output format on a non-terminal stdout only happens when it was
/// asked for explicitly, so it earns a notice that the stream is JSON lines.
pub async fn stream_subscription(
    inv: &Invocation,
    chain: Chain,
    target: SubscribeTarget,
    args: &StreamBoundArgs,
) -> Result<(), anyhow::Error> {
    ws::ensure_within_limits(target.subscriptions())?;
    let mode = inv.mode();
    let projection = inv.session().json_options();
    let bounds = StreamBounds::subscribe(args, inv.settings().env());
    if !mode.terminal.stdout && !mode.machine_format() {
        eprintln!("{JSON_LINES_STDOUT_NOTICE}");
    }
    let announce_loss = mode.person_on_stderr();

    run_stream(async move {
        let mut stdout = io::stdout();
        stream_target(
            &target,
            ws::websocket_for_chain(chain),
            bounds,
            &mut stdout,
            projection,
            announce_loss,
        )
        .await?;
        Ok(StreamEnd::Finished)
    })
    .await
}

/// Subscribe `target` on `socket` and stream its matching messages to `output`.
///
/// Every subscription of the target is sent on the one connection. Production
/// passes the Hyperliquid socket; a test can point it at a local server.
pub async fn stream_target<W>(
    target: &SubscribeTarget,
    mut socket: WebSocket,
    bounds: StreamBounds,
    output: &mut W,
    projection: &JsonRenderOptions,
    announce_loss: bool,
) -> Result<(), anyhow::Error>
where
    W: Write + ?Sized,
{
    for subscription in target.subscriptions() {
        socket.subscribe(subscription.clone());
    }
    stream_subscription_events(
        &target.label(),
        bounds,
        |message| target.matches(message),
        &mut socket,
        output,
        projection,
        announce_loss,
    )
    .await
}

/// Stream pre-built WebSocket events to a writer as subscription NDJSON.
///
/// This is the deterministic core behind [`stream_subscription`]. Production
/// passes the real Hyperliquid WebSocket stream, while tests can pass a fake
/// stream of [`WsEvent`] values and a byte buffer.
///
/// A disconnect prints a stderr notice when `announce_loss` is set (see
/// [`crate::mode::Mode::person_on_stderr`]). When the connection comes back, one
/// `{"type":"warning","reason":"stream_gap"}` line marks the point where
/// events may have been missed.
pub async fn stream_subscription_events<S, F, W>(
    subscription_label: &str,
    bounds: StreamBounds,
    event_matches: F,
    events: &mut S,
    output: &mut W,
    projection: &JsonRenderOptions,
    announce_loss: bool,
) -> Result<(), anyhow::Error>
where
    S: Stream<Item = WsEvent> + Unpin + ?Sized,
    F: Fn(&Incoming) -> bool,
    W: Write + ?Sized,
{
    emit_json_line(
        output,
        &serde_json::json!({
            "type": "subscribed",
            "subscription": subscription_label,
        }),
        projection,
    )?;

    if bounds.max_items == Some(0) {
        return Ok(());
    }

    let mut emitted_events = 0usize;
    let mut gap_pending = false;
    let mut idle = IdleTimer::new(bounds.idle);

    loop {
        tokio::select! {
            idle_for = idle.expired() => {
                return Err(subscription_idle_timeout_error(idle_for).into());
            }
            event = events.next() => {
                let Some(event) = event else {
                    return Err(ws::closed_before("subscription events").into());
                };

                if handle_subscription_event(
                    event,
                    subscription_label,
                    &event_matches,
                    &mut gap_pending,
                    output,
                    projection,
                    announce_loss,
                )? {
                    emitted_events += 1;
                    if bounds.reached(emitted_events) {
                        return Ok(());
                    }
                    idle.restart();
                }
            }
        }
    }
}

/// Apply one WebSocket event to the stream. Returns whether an event line was written.
fn handle_subscription_event<F, W>(
    event: WsEvent,
    subscription_label: &str,
    event_matches: &F,
    gap_pending: &mut bool,
    output: &mut W,
    projection: &JsonRenderOptions,
    announce_loss: bool,
) -> Result<bool, anyhow::Error>
where
    F: Fn(&Incoming) -> bool,
    W: Write + ?Sized,
{
    match event {
        WsEvent::Message(message) if event_matches(&message) => {
            emit_json_line(
                output,
                &serde_json::json!({
                    "type": "event",
                    "subscription": subscription_label,
                    "data": message,
                }),
                projection,
            )?;
            Ok(true)
        }
        WsEvent::Message(_) => Ok(false),
        WsEvent::Disconnected => {
            if !*gap_pending {
                *gap_pending = true;
                if announce_loss {
                    eprintln!("{CONNECTION_LOST_NOTICE}");
                }
            }
            Ok(false)
        }
        WsEvent::Connected => {
            if *gap_pending {
                *gap_pending = false;
                // Control line: written unprojected so --select cannot drop the gap marker.
                write_line(
                    output,
                    &serde_json::to_string(&serde_json::json!({
                        "type": "warning",
                        "reason": "stream_gap",
                        "subscription": subscription_label,
                        "message": STREAM_GAP_MESSAGE,
                    }))?,
                )?;
            }
            Ok(false)
        }
    }
}

fn subscription_idle_timeout_error(idle_timeout: Duration) -> CliError {
    CliError::Timeout(format!(
        "Timed out waiting for subscription events after {}.",
        output::format::duration(idle_timeout)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn broken_pipe_write_becomes_stdout_closed() {
        struct ClosedPipe;
        impl Write for ClosedPipe {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let err = write_line(&mut ClosedPipe, "{}").unwrap_err();
        assert!(is_stdout_closed(&err));
    }

    #[test]
    fn other_write_errors_stay_errors() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let err = write_line(&mut Failing, "{}").unwrap_err();
        assert!(!is_stdout_closed(&err));
        assert!(err.to_string().contains("disk full"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_signals_exit_with_128_plus_the_signal_number() {
        let signals = ShutdownSignals::install().unwrap();
        let mut codes: Vec<i32> = signals.signals.iter().map(|(_, code)| *code).collect();
        codes.sort_unstable();
        assert_eq!(codes, [129, 130, 143], "SIGHUP, SIGINT and SIGTERM");
        assert_eq!(EXIT_SIGINT, 130);
    }

    #[test]
    fn watch_risk_bounds_default_the_idle_end_and_keep_explicit_ones() {
        let defaulted = StreamBounds::watch_risk(&StreamBoundArgs::default());
        assert_eq!(defaulted.max_items, None);
        assert_eq!(defaulted.idle, Some(POLL_DEFAULT_IDLE));
        assert_eq!(
            StreamBounds::watch_risk(&StreamBoundArgs {
                max_events: Some(3),
                idle_timeout_ms: Some(250),
            }),
            StreamBounds {
                max_items: Some(3),
                idle: Some(Duration::from_millis(250)),
            }
        );
    }

    #[test]
    fn zero_in_the_environment_follows_the_bound() {
        let ticks = StreamBound::SnapshotWatch.env().unwrap().zero;
        let events = StreamBound::Subscribe.env().unwrap().zero;
        assert_eq!(parse_env_count("0", ticks), None, "0 ticks is not a bound");
        assert_eq!(
            parse_env_count("0", events),
            Some(0),
            "0 events stops at once"
        );
        assert_eq!(parse_env_count(" 3 ", ticks), Some(3));
        assert_eq!(parse_env_count("3", events), Some(3));
        assert_eq!(parse_env_count("many", events), None);
    }

    #[test]
    fn every_bound_names_args_its_command_flattens() {
        use clap::{Args, Command};
        let watch = SnapshotWatchArgs::augment_args(Command::new("c"));
        let events = StreamBoundArgs::augment_args(Command::new("c"));
        for (bound, command) in [
            (StreamBound::SnapshotWatch, &watch),
            (StreamBound::Subscribe, &events),
            (StreamBound::Polled, &events),
        ] {
            let ids = |id: &str| command.get_arguments().any(|arg| arg.get_id() == id);
            for id in bound
                .cli_args()
                .iter()
                .copied()
                .chain(bound.required_when())
            {
                assert!(
                    ids(id),
                    "{bound:?} names `{id}`, which its args do not define"
                );
            }
        }
    }

    #[test]
    fn the_environment_help_documents_every_stream_bound_variable() {
        let help = crate::settings::env_help_section();
        for bound in [
            StreamBound::SnapshotWatch,
            StreamBound::Subscribe,
            StreamBound::Polled,
        ] {
            if let Some(env) = bound.env() {
                assert!(
                    help.contains(env.name),
                    "{} is missing from ENV_HELP",
                    env.name
                );
            }
        }
    }

    fn matches(args: &[&str]) -> ArgMatches {
        use clap::{Args, Command};
        let command =
            StreamBoundArgs::augment_args(SnapshotWatchArgs::augment_args(Command::new("c")));
        command.try_get_matches_from(args).unwrap()
    }

    fn env(pairs: &[(&'static str, &'static str)]) -> EnvSource {
        EnvSource::fixed(pairs.iter().copied())
    }

    #[test]
    fn polled_streams_must_declare_a_bound_with_the_watch_risk_hint() {
        let none = env(&[]);
        let err = require_bound(StreamBound::Polled, &matches(&["c"]), &none).unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("--max-events 20"), "{err}");
        for bounded in [["c", "--max-events", "3"], ["c", "--idle-timeout-ms", "50"]] {
            assert!(require_bound(StreamBound::Polled, &matches(&bounded), &none).is_ok());
        }
    }

    #[test]
    fn snapshot_watch_is_bounded_only_when_watching() {
        let none = env(&[]);
        assert!(require_bound(StreamBound::SnapshotWatch, &matches(&["c"]), &none).is_ok());
        assert!(
            require_bound(
                StreamBound::SnapshotWatch,
                &matches(&["c", "--watch", "--max-ticks", "2"]),
                &none
            )
            .is_ok()
        );
        let err = require_bound(
            StreamBound::SnapshotWatch,
            &matches(&["c", "--watch"]),
            &none,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert_eq!(
            err.to_string(),
            "Unsupported input: JSON watch output must be bounded with --max-ticks <N> or HYPERLIQUID_WATCH_MAX_TICKS"
        );
    }

    #[test]
    fn subscribe_is_bounded_by_events_idle_or_the_environment() {
        let none = env(&[]);
        let err = require_bound(StreamBound::Subscribe, &matches(&["c"]), &none).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unsupported input: JSON subscribe output must be bounded with --max-events <N>, --idle-timeout-ms <N>, or HYPERLIQUID_SUBSCRIBE_MAX_EVENTS"
        );
        for bounded in [["c", "--max-events", "0"], ["c", "--idle-timeout-ms", "10"]] {
            assert!(require_bound(StreamBound::Subscribe, &matches(&bounded), &none).is_ok());
        }
        let zero = env(&[("HYPERLIQUID_SUBSCRIBE_MAX_EVENTS", "0")]);
        assert!(require_bound(StreamBound::Subscribe, &matches(&["c"]), &zero).is_ok());
    }

    #[test]
    fn the_environment_bound_is_read_from_the_given_source_and_checked_only_when_consulted() {
        let ticks = env(&[("HYPERLIQUID_WATCH_MAX_TICKS", "4")]);
        assert_eq!(
            StreamBounds::snapshot_watch(None, &ticks).max_items,
            Some(4)
        );
        assert_eq!(
            StreamBounds::snapshot_watch(Some(2), &ticks).max_items,
            Some(2),
            "a flag beats the variable"
        );
        assert_eq!(
            StreamBounds::snapshot_watch(None, &env(&[])).max_items,
            None
        );

        let garbage = env(&[("HYPERLIQUID_SUBSCRIBE_MAX_EVENTS", "many")]);
        let err = require_bound(StreamBound::Subscribe, &matches(&["c"]), &garbage).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .contains("HYPERLIQUID_SUBSCRIBE_MAX_EVENTS must be")
        );
        assert!(
            require_bound(
                StreamBound::Subscribe,
                &matches(&["c", "--max-events", "3"]),
                &garbage
            )
            .is_ok(),
            "a flag bounds the run without consulting the variable"
        );
    }

    struct CountingTicker {
        ticks: Arc<AtomicUsize>,
        per_tick: usize,
    }

    impl Ticker for CountingTicker {
        type Item = usize;

        async fn tick(&mut self) -> Result<Vec<usize>, anyhow::Error> {
            let tick = self.ticks.fetch_add(1, Ordering::SeqCst) + 1;
            Ok((0..self.per_tick).map(|index| tick * 10 + index).collect())
        }
    }

    #[tokio::test]
    async fn poll_stream_stops_at_item_bound_mid_tick() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let mut ticker = CountingTicker {
            ticks: Arc::clone(&ticks),
            per_tick: 2,
        };
        let mut seen = Vec::new();

        poll_stream(
            &mut ticker,
            Duration::ZERO,
            StreamBounds {
                max_items: Some(3),
                idle: None,
            },
            |item| {
                seen.push(item);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(seen, vec![10, 11, 20]);
        assert_eq!(ticks.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn poll_stream_with_zero_item_bound_never_ticks() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let mut ticker = CountingTicker {
            ticks: Arc::clone(&ticks),
            per_tick: 1,
        };

        poll_stream(
            &mut ticker,
            Duration::ZERO,
            StreamBounds {
                max_items: Some(0),
                idle: None,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(ticks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn poll_stream_ends_cleanly_after_idle_without_output() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let mut ticker = CountingTicker {
            ticks: Arc::clone(&ticks),
            per_tick: 0,
        };

        poll_stream(
            &mut ticker,
            Duration::from_millis(2),
            StreamBounds {
                max_items: None,
                idle: Some(Duration::from_millis(10)),
            },
            |_| Ok(()),
        )
        .await
        .unwrap();

        assert!(ticks.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn poll_stream_idle_bound_also_ends_a_poll_that_never_returns() {
        struct Stalled;
        impl Ticker for Stalled {
            type Item = ();

            async fn tick(&mut self) -> Result<Vec<()>, anyhow::Error> {
                std::future::pending().await
            }
        }

        poll_stream(
            &mut Stalled,
            Duration::from_secs(1),
            StreamBounds {
                max_items: None,
                idle: Some(Duration::from_millis(20)),
            },
            |()| Ok(()),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn poll_stream_propagates_emit_errors() {
        let mut ticker = CountingTicker {
            ticks: Arc::new(AtomicUsize::new(0)),
            per_tick: 1,
        };

        let err = poll_stream(&mut ticker, Duration::ZERO, StreamBounds::default(), |_| {
            Err(StdoutClosed.into())
        })
        .await
        .unwrap_err();

        assert!(is_stdout_closed(&err));
    }
}
