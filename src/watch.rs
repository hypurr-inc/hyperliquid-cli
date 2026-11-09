//! Renders snapshots in-place using crossterm's alternate screen for human
//! formats and emits newline-delimited JSON for `--format json`. Signal,
//! closed-pipe and bound handling live in [`crate::stream`].

use std::io::{self, Write};
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{self, Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size as terminal_size,
};
use futures::StreamExt;
use hypersdk::hypercore::{Chain, Subscription, WebSocket, ws::Event as WsEvent};

use crate::env;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::mode::{DisplayBlocker, Mode, NON_INTERACTIVE_FLAG};
use crate::output::notice::Notice;
use crate::output::{self, Session, TableData, sink};
use crate::stream::{EXIT_SIGINT, StreamEnd, run_stream};
use crate::stream_bounds::StreamBounds;
use crate::terminal::OutputFormat;
use crate::ws::websocket_for_chain;

const WATCH_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const KEY_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// What ends the wait between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchEvent {
    /// Time passed, the socket delivered data or `r` was pressed: take another snapshot.
    Refresh,
    ConnectionLost,
    ConnectionRestored,
    Quit,
    Interrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotWatchRenderMode {
    Ndjson,
    Tui(OutputFormat),
}

trait WatchInput {
    async fn next_event(&mut self) -> Result<WatchEvent, anyhow::Error>;
}

trait WatchOutput {
    fn emit_json(&mut self, line: &str) -> Result<(), anyhow::Error>;

    fn render_tui(
        &mut self,
        session: &Session,
        title: &str,
        data: &dyn TableData,
        format: OutputFormat,
        connection_lost: bool,
    ) -> Result<(), anyhow::Error>;
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, anyhow::Error> {
        Self::try_enter().map_err(|err| {
            CliError::Internal(anyhow::anyhow!(
                "failed to initialize watch terminal: {err}"
            ))
            .into()
        })
    }

    fn try_enter() -> Result<Self, io::Error> {
        enable_raw_mode()?;
        let guard = Self;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            cursor::Hide,
            Clear(ClearType::All)
        )?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
    }
}

struct StdoutWatchOutput;

impl WatchOutput for StdoutWatchOutput {
    fn emit_json(&mut self, line: &str) -> Result<(), anyhow::Error> {
        sink::stream_line(line)
    }

    fn render_tui(
        &mut self,
        session: &Session,
        title: &str,
        data: &dyn TableData,
        format: OutputFormat,
        connection_lost: bool,
    ) -> Result<(), anyhow::Error> {
        let body = session.render_as(data, format);

        let mut stdout = io::stdout();
        execute!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
        let (_, height) = terminal_size().unwrap_or((120, 36));
        let available_body_lines = usize::from(height.saturating_sub(4));
        let body_lines = body
            .lines()
            .take(available_body_lines)
            .collect::<Vec<_>>()
            .join("\n");

        writeln!(stdout, "{}", output::colors::cyan(title))?;
        writeln!(
            stdout,
            "{}",
            output::colors::gray(
                "q/Ctrl-C/Ctrl-D/Esc quit • r refresh • live updates via Hyperliquid WebSocket",
            )
        )?;
        if connection_lost {
            writeln!(
                stdout,
                "{}",
                output::colors::yellow(&Notice::ConnectionLost.text())
            )?;
        } else {
            writeln!(stdout)?;
        }
        writeln!(stdout, "{body_lines}")?;
        stdout.flush().map_err(Into::into)
    }
}

struct RealWatchInput {
    ws: WebSocket,
    /// The socket has ended; the refresh timer alone keeps the view current.
    ws_closed: bool,
    read_keys: bool,
}

impl WatchInput for RealWatchInput {
    async fn next_event(&mut self) -> Result<WatchEvent, anyhow::Error> {
        let refresh_sleep = tokio::time::sleep(WATCH_REFRESH_INTERVAL);
        tokio::pin!(refresh_sleep);

        loop {
            tokio::select! {
                _ = &mut refresh_sleep => return Ok(WatchEvent::Refresh),
                event = self.ws.next(), if !self.ws_closed => match event {
                    Some(WsEvent::Message(_)) => return Ok(WatchEvent::Refresh),
                    Some(WsEvent::Disconnected) => return Ok(WatchEvent::ConnectionLost),
                    Some(WsEvent::Connected) => return Ok(WatchEvent::ConnectionRestored),
                    None => self.ws_closed = true,
                },
                _ = tokio::time::sleep(KEY_POLL_INTERVAL), if self.read_keys => {
                    if let Some(event) = try_read_key_event().await?.and_then(watch_key_event_action) {
                        return Ok(event);
                    }
                }
            }
        }
    }
}

pub async fn run_snapshot_watch<T, F, Fut>(
    inv: &Invocation,
    title: &str,
    chain: Chain,
    subscriptions: Vec<Subscription>,
    max_ticks: Option<usize>,
    refresh: F,
) -> Result<(), anyhow::Error>
where
    T: TableData,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, anyhow::Error>>,
{
    let session = inv.session();
    let mode = inv.mode();
    let format = session.format();
    let bounds = StreamBounds::snapshot_watch(max_ticks, inv.settings().env());
    let render_mode = snapshot_watch_render_mode(mode, bounds.max_items)?;

    // A human format that reaches the NDJSON path was asked for explicitly:
    // non-terminal stdout defaults to JSON before this point.
    if render_mode == SnapshotWatchRenderMode::Ndjson
        && format != OutputFormat::Json
        && let Some(blocker) = mode.full_screen_blocker()
    {
        Notice::JsonLines(blocker).eprint();
    }

    run_stream(async move {
        let ws = websocket_for_chain(chain);
        for subscription in subscriptions {
            ws.subscribe(subscription);
        }

        let mut input = RealWatchInput {
            ws,
            ws_closed: false,
            read_keys: matches!(render_mode, SnapshotWatchRenderMode::Tui(_)),
        };
        let mut output = StdoutWatchOutput;

        match render_mode {
            SnapshotWatchRenderMode::Ndjson => {
                run_snapshot_watch_core(
                    session,
                    title,
                    OutputFormat::Json,
                    bounds.max_items,
                    refresh,
                    &mut input,
                    &mut output,
                )
                .await
            }
            SnapshotWatchRenderMode::Tui(format) => {
                let _terminal = TerminalGuard::enter()?;
                run_snapshot_watch_core(
                    session,
                    title,
                    format,
                    bounds.max_items,
                    refresh,
                    &mut input,
                    &mut output,
                )
                .await
            }
        }
    })
    .await
}

async fn run_snapshot_watch_core<T, F, Fut, I, O>(
    session: &Session,
    title: &str,
    format: OutputFormat,
    max_ticks: Option<usize>,
    mut refresh: F,
    input: &mut I,
    output: &mut O,
) -> Result<StreamEnd, anyhow::Error>
where
    T: TableData,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, anyhow::Error>>,
    I: WatchInput + ?Sized,
    O: WatchOutput + ?Sized,
{
    let mut rendered_ticks = 0usize;
    // Each tick is a full snapshot, so a dropped connection never loses data;
    // the flag only drives the TUI notice.
    let mut connection_lost = false;

    loop {
        let snapshot = refresh().await?;
        if format == OutputFormat::Json {
            output.emit_json(&session.render_json_line(&snapshot))?;
        } else {
            output.render_tui(session, title, &snapshot, format, connection_lost)?;
        }

        rendered_ticks += 1;
        if max_ticks.is_some_and(|max| rendered_ticks >= max) {
            return Ok(StreamEnd::Finished);
        }

        match input.next_event().await? {
            WatchEvent::Quit => return Ok(StreamEnd::Finished),
            WatchEvent::Interrupt => return Ok(StreamEnd::Signal(EXIT_SIGINT)),
            WatchEvent::ConnectionLost => connection_lost = true,
            WatchEvent::ConnectionRestored => connection_lost = false,
            WatchEvent::Refresh => {}
        }
    }
}

/// The TUI needs a person at a terminal on both streams; anything else is
/// NDJSON when bounded and refused when not.
fn snapshot_watch_render_mode(
    mode: &Mode,
    max_ticks: Option<usize>,
) -> Result<SnapshotWatchRenderMode, CliError> {
    if mode.machine_format() {
        return Ok(SnapshotWatchRenderMode::Ndjson);
    }
    match mode.full_screen_blocker() {
        None => Ok(SnapshotWatchRenderMode::Tui(mode.format)),
        Some(_) if max_ticks.is_some() => Ok(SnapshotWatchRenderMode::Ndjson),
        Some(blocker) => Err(tui_refused(blocker)),
    }
}

fn tui_refused(blocker: DisplayBlocker) -> CliError {
    let (subject, reason) = match blocker {
        DisplayBlocker::NoTerminal => (
            "Non-TTY watch output",
            "stdin and stdout are not TTYs".to_string(),
        ),
        DisplayBlocker::StdinNotTerminal => {
            ("Non-TTY watch output", "stdin is not a TTY".to_string())
        }
        DisplayBlocker::StdoutNotTerminal => {
            ("Non-TTY watch output", "stdout is not a TTY".to_string())
        }
        DisplayBlocker::Driven => (
            "Watch output of a driven run",
            format!(
                "{}, {} or {} is driving this run",
                env::AGENT,
                env::CI,
                NON_INTERACTIVE_FLAG
            ),
        ),
    };
    CliError::Unsupported(format!(
        "{subject} cannot enter the alternate-screen TUI safely ({reason}). \
         For scripts/agents, pass --format json and --max-ticks <N> \
         (or set {}) to receive bounded NDJSON.",
        env::WATCH_MAX_TICKS
    ))
}

/// The action for a key press; `None` for keys the view does not use.
fn watch_key_event_action(key: KeyEvent) -> Option<WatchEvent> {
    let has_control = key.modifiers.contains(KeyModifiers::CONTROL);
    let plain_or_shift = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();

    match key.code {
        KeyCode::Esc => Some(WatchEvent::Quit),
        KeyCode::Char('c' | 'C') if has_control => Some(WatchEvent::Interrupt),
        KeyCode::Char('d' | 'D') if has_control => Some(WatchEvent::Quit),
        KeyCode::Char('q' | 'Q') if plain_or_shift => Some(WatchEvent::Quit),
        KeyCode::Char('r' | 'R') if plain_or_shift => Some(WatchEvent::Refresh),
        _ => None,
    }
}

async fn try_read_key_event() -> Result<Option<KeyEvent>, anyhow::Error> {
    tokio::task::spawn_blocking(|| -> Result<Option<KeyEvent>, io::Error> {
        if event::poll(Duration::ZERO)?
            && let TerminalEvent::Key(key) = event::read()?
        {
            return Ok(Some(key));
        }
        Ok(None)
    })
    .await
    .map_err(|err| anyhow::anyhow!("watch keyboard task failed: {err}"))?
    .map_err(|err| anyhow::anyhow!("failed to read watch keyboard input: {err}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use std::collections::VecDeque;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn render_mode_is_tui_only_for_interactive_terminals_and_ndjson_when_bounded_or_json() {
        use OutputFormat::{Json, Pretty, Table};
        use SnapshotWatchRenderMode::{Ndjson, Tui};
        for (format, max_ticks, stdin_tty, stdout_tty, expected) in [
            (Pretty, None, true, true, Tui(Pretty)),
            (Table, Some(1), true, true, Tui(Table)),
            (Json, None, false, false, Ndjson),
            (Json, None, true, true, Ndjson),
            (Pretty, Some(1), false, false, Ndjson),
            (Table, Some(1), true, false, Ndjson),
            (Pretty, Some(1), false, true, Ndjson),
        ] {
            assert_eq!(
                snapshot_watch_render_mode(&mode(format, stdin_tty, stdout_tty), max_ticks)
                    .unwrap(),
                expected,
                "{format:?} ticks={max_ticks:?} stdin={stdin_tty} stdout={stdout_tty}"
            );
        }
    }

    fn mode(format: OutputFormat, stdin: bool, stdout: bool) -> Mode {
        Mode {
            format,
            terminal: Terminal {
                stdin,
                stdout,
                stderr: true,
            },
            ..Mode::default()
        }
    }

    #[test]
    fn an_agent_ci_or_non_interactive_run_never_draws_the_tui_even_on_a_terminal() {
        let on_a_terminal = mode(OutputFormat::Pretty, true, true);
        for (name, driven) in [
            (
                "agent",
                Mode {
                    agent: true,
                    ..on_a_terminal
                },
            ),
            (
                "ci",
                Mode {
                    ci: true,
                    ..on_a_terminal
                },
            ),
            (
                "non-interactive",
                Mode {
                    non_interactive_flag: true,
                    ..on_a_terminal
                },
            ),
        ] {
            assert_eq!(
                snapshot_watch_render_mode(&driven, Some(3)).unwrap(),
                SnapshotWatchRenderMode::Ndjson,
                "{name}: bounded"
            );
            let refused = snapshot_watch_render_mode(&driven, None).unwrap_err();
            assert_eq!(refused.exit_code(), 13, "{name}: unbounded");
            let message = refused.to_string();
            assert!(message.contains("--format json"), "{name}: {message}");
            assert!(message.contains("is driving this run"), "{name}: {message}");
            assert_eq!(
                driven.full_screen_blocker(),
                Some(DisplayBlocker::Driven),
                "{name}"
            );
        }
    }

    #[test]
    fn the_ndjson_notice_names_the_stream_that_is_not_a_terminal() {
        assert_eq!(
            mode(OutputFormat::Pretty, true, false).full_screen_blocker(),
            Some(DisplayBlocker::StdoutNotTerminal)
        );
        assert_eq!(
            mode(OutputFormat::Pretty, false, true).full_screen_blocker(),
            Some(DisplayBlocker::StdinNotTerminal)
        );
    }

    #[test]
    fn unbounded_watch_without_both_terminals_is_refused_with_the_json_alternative() {
        for (stdin_tty, stdout_tty) in [(false, true), (true, false), (false, false)] {
            let error = snapshot_watch_render_mode(
                &mode(OutputFormat::Pretty, stdin_tty, stdout_tty),
                None,
            )
            .unwrap_err();
            assert_eq!(
                error.exit_code(),
                13,
                "stdin={stdin_tty} stdout={stdout_tty}"
            );
            let message = error.to_string();
            assert!(message.contains("--format json"), "{message}");
            assert!(message.contains("--max-ticks"), "{message}");
        }
    }

    #[test]
    fn watch_keys_quit_interrupt_refresh_and_ignore_everything_else() {
        let key = |code, modifiers| watch_key_event_action(KeyEvent::new(code, modifiers));
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        for (code, modifiers, expected) in [
            (KeyCode::Esc, none, Some(WatchEvent::Quit)),
            (KeyCode::Char('d'), ctrl, Some(WatchEvent::Quit)),
            (KeyCode::Char('q'), none, Some(WatchEvent::Quit)),
            (
                KeyCode::Char('Q'),
                KeyModifiers::SHIFT,
                Some(WatchEvent::Quit),
            ),
            (KeyCode::Char('c'), ctrl, Some(WatchEvent::Interrupt)),
            (KeyCode::Char('C'), ctrl, Some(WatchEvent::Interrupt)),
            (KeyCode::Char('r'), none, Some(WatchEvent::Refresh)),
            (KeyCode::Char('c'), none, None),
            (KeyCode::Char('d'), none, None),
            (KeyCode::Char('r'), ctrl, None),
            (KeyCode::Char('x'), none, None),
            (KeyCode::Enter, none, None),
            (KeyCode::Up, none, None),
            (KeyCode::Tab, none, None),
        ] {
            assert_eq!(key(code, modifiers), expected, "{code:?} {modifiers:?}");
        }
    }

    #[derive(Clone)]
    struct TickData {
        tick: usize,
    }

    impl TableData for TickData {
        fn headers(&self) -> Vec<&str> {
            vec!["Tick"]
        }

        fn rows(&self) -> Vec<Vec<String>> {
            vec![vec![self.tick.to_string()]]
        }

        fn to_json_value(&self) -> serde_json::Value {
            serde_json::json!({ "tick": self.tick })
        }
    }

    struct ScriptedInput {
        events: VecDeque<WatchEvent>,
    }

    impl ScriptedInput {
        fn new(events: impl Into<VecDeque<WatchEvent>>) -> Self {
            Self {
                events: events.into(),
            }
        }
    }

    impl WatchInput for ScriptedInput {
        async fn next_event(&mut self) -> Result<WatchEvent, anyhow::Error> {
            Ok(self.events.pop_front().unwrap_or(WatchEvent::Refresh))
        }
    }

    #[derive(Default)]
    struct CapturingOutput {
        json_lines: Vec<String>,
        tui_renders: Vec<TuiRender>,
    }

    struct TuiRender {
        title: String,
        format: OutputFormat,
        rows: Vec<Vec<String>>,
        connection_lost: bool,
    }

    impl WatchOutput for CapturingOutput {
        fn emit_json(&mut self, line: &str) -> Result<(), anyhow::Error> {
            self.json_lines.push(line.to_string());
            Ok(())
        }

        fn render_tui(
            &mut self,
            _session: &Session,
            title: &str,
            data: &dyn TableData,
            format: OutputFormat,
            connection_lost: bool,
        ) -> Result<(), anyhow::Error> {
            self.tui_renders.push(TuiRender {
                title: title.to_string(),
                format,
                rows: data.rows(),
                connection_lost,
            });
            Ok(())
        }
    }

    fn counting_refresh(
        counter: Arc<AtomicUsize>,
    ) -> impl FnMut() -> std::future::Ready<Result<TickData, anyhow::Error>> {
        move || {
            let tick = counter.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(Ok(TickData { tick }))
        }
    }

    #[tokio::test]
    async fn watch_json_max_tick_output_emits_expected_snapshots_and_terminates() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut input = ScriptedInput::new([WatchEvent::Refresh]);
        let mut output = CapturingOutput::default();

        run_snapshot_watch_core(
            &Session::for_test(OutputFormat::Pretty),
            "Live test",
            OutputFormat::Json,
            Some(2),
            counting_refresh(Arc::clone(&counter)),
            &mut input,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert_eq!(output.json_lines, vec![r#"{"tick":1}"#, r#"{"tick":2}"#]);
        assert!(output.tui_renders.is_empty());
    }

    #[tokio::test]
    async fn human_formats_render_to_the_screen_and_not_as_json_lines() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut input = ScriptedInput::new([]);
        let mut output = CapturingOutput::default();

        run_snapshot_watch_core(
            &Session::for_test(OutputFormat::Pretty),
            "Live TUI",
            OutputFormat::Pretty,
            Some(1),
            counting_refresh(counter),
            &mut input,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(output.tui_renders.len(), 1);
        assert_eq!(output.tui_renders[0].title, "Live TUI");
        assert_eq!(output.tui_renders[0].format, OutputFormat::Pretty);
        assert_eq!(output.tui_renders[0].rows, vec![vec!["1".to_string()]]);
        assert!(output.json_lines.is_empty());
    }

    #[tokio::test]
    async fn watch_q_quits_after_current_snapshot() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut input = ScriptedInput::new([WatchEvent::Quit]);
        let mut output = CapturingOutput::default();

        run_snapshot_watch_core(
            &Session::for_test(OutputFormat::Pretty),
            "Live TUI",
            OutputFormat::Table,
            None,
            counting_refresh(Arc::clone(&counter)),
            &mut input,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(output.tui_renders.len(), 1);
    }

    #[tokio::test]
    async fn watch_r_refreshes_before_q_quits() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut input = ScriptedInput::new([WatchEvent::Refresh, WatchEvent::Quit]);
        let mut output = CapturingOutput::default();

        run_snapshot_watch_core(
            &Session::for_test(OutputFormat::Pretty),
            "Live TUI",
            OutputFormat::Pretty,
            None,
            counting_refresh(Arc::clone(&counter)),
            &mut input,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert_eq!(output.tui_renders.len(), 2);
        assert_eq!(output.tui_renders[1].rows, vec![vec!["2".to_string()]]);
    }

    #[tokio::test]
    async fn watch_refresh_error_propagates_clearly() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut input = ScriptedInput::new([WatchEvent::Refresh]);
        let mut output = CapturingOutput::default();

        let result = run_snapshot_watch_core(
            &Session::for_test(OutputFormat::Pretty),
            "Live TUI",
            OutputFormat::Pretty,
            None,
            {
                let counter = Arc::clone(&counter);
                move || {
                    let tick = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    std::future::ready(if tick == 1 {
                        Ok(TickData { tick })
                    } else {
                        Err(CliError::Unavailable(
                            "refresh failed in deterministic test".to_string(),
                        )
                        .into())
                    })
                }
            },
            &mut input,
            &mut output,
        )
        .await;

        let err = result.unwrap_err().to_string();
        assert!(err.contains("refresh failed in deterministic test"));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert_eq!(output.tui_renders.len(), 1);
    }

    #[tokio::test]
    async fn watch_ctrl_c_key_ends_with_sigint_status() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut input = ScriptedInput::new([WatchEvent::Interrupt]);
        let mut output = CapturingOutput::default();

        let end = run_snapshot_watch_core(
            &Session::for_test(OutputFormat::Pretty),
            "Live TUI",
            OutputFormat::Pretty,
            None,
            counting_refresh(counter),
            &mut input,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(end, StreamEnd::Signal(130));
    }

    #[tokio::test]
    async fn connection_notice_follows_lost_and_restored_events_without_stopping() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut input =
            ScriptedInput::new([WatchEvent::ConnectionLost, WatchEvent::ConnectionRestored]);
        let mut output = CapturingOutput::default();

        let end = run_snapshot_watch_core(
            &Session::for_test(OutputFormat::Pretty),
            "Live TUI",
            OutputFormat::Pretty,
            Some(3),
            counting_refresh(Arc::clone(&counter)),
            &mut input,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(end, StreamEnd::Finished);
        let lost: Vec<bool> = output
            .tui_renders
            .iter()
            .map(|render| render.connection_lost)
            .collect();
        assert_eq!(lost, [false, true, false]);
    }
}
