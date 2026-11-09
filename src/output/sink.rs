//! The only writers to the process's stdout and stderr.
//!
//! One rule for a reader that has gone away (`| head`, a closed pipe, an agent
//! that dropped the descriptor): it is the end of the conversation, never a
//! panic and never a failure of the run.
//!
//! - Answers ([`out`], [`out_line`]) ignore a closed pipe; the run carries on
//!   to its own exit status. A write that fails any other way (a full disk)
//!   cannot be reported on the stream that broke, so it says so on stderr and
//!   exits 1.
//! - Diagnostics ([`err`], [`err_line`]) are best effort: a closed or failing
//!   stderr never stops a run, because the work they report on may be done.
//! - A stream ([`stream_line`], [`write_line`]) reports the closed pipe as
//!   [`StdoutClosed`], so the stream stops on its own and exits 0.
//! - A write a decision depends on, such as a consent block or a prompt
//!   label, uses [`try_err`] and fails the run, because the person never saw
//!   what they were asked to approve.
//!
//! No other module prints with `println!` or `eprintln!` (a test enforces it):
//! those panic on a closed pipe.

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};

/// The reader of stdout went away.
#[derive(Debug)]
pub struct StdoutClosed;

impl fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stdout closed")
    }
}

impl StdError for StdoutClosed {}

#[must_use]
pub fn is_stdout_closed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<StdoutClosed>().is_some()
}

fn is_closed(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::BrokenPipe
}

/// Write `line` plus a newline to `output` and flush. A closed pipe becomes
/// [`StdoutClosed`].
pub fn write_line<W: Write + ?Sized>(output: &mut W, line: &str) -> Result<(), anyhow::Error> {
    output
        .write_all(line.as_bytes())
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
        .map_err(|err| {
            if is_closed(&err) {
                StdoutClosed.into()
            } else {
                err.into()
            }
        })
}

/// [`write_line`] to stdout: one complete line of a stream.
pub fn stream_line(line: &str) -> Result<(), anyhow::Error> {
    write_line(&mut io::stdout().lock(), line)
}

/// Write `text` to stdout as is. A closed pipe is `Ok`; the caller decides
/// what any other failure means.
pub fn try_out(text: &str) -> io::Result<()> {
    write_stdout(text, false)
}

fn write_stdout(text: &str, newline: bool) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let written = out
        .write_all(text.as_bytes())
        .and_then(|()| {
            if newline {
                out.write_all(b"\n")
            } else {
                Ok(())
            }
        })
        .and_then(|()| out.flush());
    match written {
        Err(err) if is_closed(&err) => Ok(()),
        other => other,
    }
}

fn out_or_exit(text: &str, newline: bool) {
    if let Err(failure) = write_stdout(text, newline) {
        err_line(&format!("Failed to write to stdout: {failure}"));
        std::process::exit(1);
    }
}

/// Print `text` to stdout as is.
pub fn out(text: &str) {
    out_or_exit(text, false);
}

/// Print `text` and a newline to stdout.
pub fn out_line(text: &str) {
    out_or_exit(text, true);
}

/// Write `text` to stderr as is, and say whether it was written.
pub fn try_err(text: &str) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(text.as_bytes())
        .and_then(|()| stderr.flush())
}

/// [`try_err`] with a newline after `line`.
pub fn try_err_line(line: &str) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(line.as_bytes())
        .and_then(|()| stderr.write_all(b"\n"))
        .and_then(|()| stderr.flush())
}

/// Print `text` to stderr as is; a failure is ignored.
pub fn err(text: &str) {
    let _ = try_err(text);
}

/// Print `line` and a newline to stderr; a failure is ignored.
pub fn err_line(line: &str) {
    let _ = try_err_line(line);
}

/// Flush both streams before the process exits for a signal.
pub fn flush() {
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_is_the_end_of_a_stream_not_a_failure() {
        let err = write_line(&mut Failing(io::ErrorKind::BrokenPipe), "{}").unwrap_err();
        assert!(is_stdout_closed(&err));
    }

    #[test]
    fn any_other_write_failure_is_still_a_failure() {
        let err = write_line(&mut Failing(io::ErrorKind::StorageFull), "{}").unwrap_err();
        assert!(!is_stdout_closed(&err));
    }
}
