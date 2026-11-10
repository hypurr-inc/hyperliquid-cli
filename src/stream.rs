//! Streaming runtime shared by snapshot watch, `subscribe` and `watch risk`.
//!
//! Every stream stops on SIGINT, SIGTERM, and SIGHUP: the loop ends, the
//! terminal is restored, the current output line is complete, and the process
//! exits `128 + signal`. A closed stdout (for example `| head`) ends the stream
//! quietly with exit 0.

use std::io::{self, Write};
use std::pin::Pin;
use std::time::Duration;

use futures::{Stream, StreamExt};
use hypersdk::Address;
use hypersdk::hypercore::{Incoming, Subscription, UserEvent, WebSocket, ws::Event as WsEvent};
use serde::Serialize;
use tokio::time::Sleep;

use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::mode::DisplayBlocker;
use crate::output::JsonRenderOptions;
use crate::output::notice::{self, Notice};
use crate::output::sink;
use crate::stream_bounds::{StreamBoundArgs, StreamBounds};
use crate::ws::BookAggregationArgs;
use crate::{output, ws};

/// Exit status for SIGINT and the Ctrl+C key (`128 + 2`). A stop signal exits
/// `128 +` its number.
pub(crate) const EXIT_SIGINT: i32 = 130;

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
    sink::write_line(output, &line)
}

struct ShutdownSignals {
    /// Each handled signal with the exit status it ends the process with.
    #[cfg(unix)]
    signals: Vec<(tokio::signal::unix::Signal, i32)>,
}

impl ShutdownSignals {
    #[cfg(unix)]
    fn install() -> Result<Self, anyhow::Error> {
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
    fn install() -> Result<Self, anyhow::Error> {
        Ok(Self {})
    }

    #[cfg(unix)]
    async fn recv(&mut self) -> i32 {
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
    async fn recv(&mut self) -> i32 {
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
    sink::flush();
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
        Err(err) if sink::is_stdout_closed(&err) => Ok(()),
        Err(err) => Err(err),
    }
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
    target: SubscribeTarget,
    args: &StreamBoundArgs,
) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    ws::ensure_within_limits(target.subscriptions())?;
    let mode = inv.mode();
    let projection = inv.session().json_options();
    let bounds = StreamBounds::subscribe(args, inv.settings().env());
    if !mode.machine_format()
        && let Some(blocker @ DisplayBlocker::StdoutNotTerminal) = mode.live_display_blocker()
    {
        Notice::JsonLines(blocker).eprint();
    }
    let announce_loss = mode.person_on_stderr();

    run_stream(async move {
        let mut stdout = io::stdout();
        if bounds.max_items == Some(0) {
            emit_subscribed_line(&mut stdout, &target.label(), projection)?;
            return Ok(StreamEnd::Finished);
        }
        stream_target(
            &target,
            api.websocket(),
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

/// The `subscribed` line that opens every stream. A zero event bound prints it
/// and stops before any connection is made.
fn emit_subscribed_line<W>(
    output: &mut W,
    subscription_label: &str,
    projection: &JsonRenderOptions,
) -> Result<(), anyhow::Error>
where
    W: Write + ?Sized,
{
    emit_json_line(
        output,
        &serde_json::json!({
            "type": "subscribed",
            "subscription": subscription_label,
        }),
        projection,
    )
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
    emit_subscribed_line(output, subscription_label, projection)?;

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
                    Notice::ConnectionLost.eprint();
                }
            }
            Ok(false)
        }
        WsEvent::Connected => {
            if *gap_pending {
                *gap_pending = false;
                // Control line: written unprojected so --select cannot drop the gap marker.
                sink::write_line(output, &notice::stream_gap(subscription_label).to_string())?;
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
    use crate::output::sink::{StdoutClosed, is_stdout_closed};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_signals_exit_with_128_plus_the_signal_number() {
        let signals = ShutdownSignals::install().unwrap();
        let mut codes: Vec<i32> = signals.signals.iter().map(|(_, code)| *code).collect();
        codes.sort_unstable();
        assert_eq!(codes, [129, 130, 143], "SIGHUP, SIGINT and SIGTERM");
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
