//! Nonce allocation: strictly increasing across threads and across the
//! concurrent CLI processes of one machine.
//!
//! The exchange keeps the 100 highest nonces per signer. A new nonce must be
//! above the smallest of them, never used before, and inside
//! `(T - 2 days, T + 1 day)` of the block time
//! (<https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets>).
//! The wall clock alone repeats a nonce when two actions land in one
//! millisecond, and goes backwards when the clock is stepped, so each nonce is
//! `max(now_ms, previous + 1)`: `previous` is the last nonce of this process and
//! the mark every process on the machine records in the data directory under an
//! advisory lock.
//!
//! The mark is one sequence for every signer rather than one per signer: a
//! user-signed action puts its nonce in the signed document before any signer
//! is at hand, and a sequence that only ever grows is strictly increasing for
//! each signer within it.
//!
//! A mark that cannot be read, locked or written never blocks signing: the
//! in-process guard still holds, so threads stay unique, and the caller is told
//! once that other processes may collide.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::private_fs::{self, FileLock, LockError};
use crate::settings::EnvSource;

/// How far past the clock a recorded nonce may sit and still be believed. The
/// exchange refuses nonces more than a day ahead, so a mark beyond this is
/// damage (a corrupt file, a clock that was once far wrong), not history.
pub(crate) const MAX_AHEAD_MS: u64 = 12 * 60 * 60 * 1000;

/// How long to wait for another process that is allocating a nonce.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// A nonce, and why it could not be recorded for other processes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Issued {
    pub(crate) nonce: u64,
    pub(crate) unrecorded: Option<String>,
}

/// The last nonce this process issued.
pub(crate) struct NonceGuard {
    last: Mutex<u64>,
}

impl NonceGuard {
    pub(crate) const fn new() -> Self {
        Self {
            last: Mutex::new(0),
        }
    }

    /// The next nonce at wall-clock time `now_ms`, recorded in `store` when given.
    pub(crate) fn next(&self, now_ms: u64, store: Option<&Path>) -> Issued {
        // Held through the file work so threads queue instead of racing the mark.
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let mut unrecorded = None;
        let mut recorded = None;
        let mut lock = None;
        if let Some(path) = store {
            match FileLock::acquire(path, LOCK_WAIT) {
                Ok(held) => {
                    lock = Some(held);
                    match read_mark(path) {
                        Ok(mark) => recorded = mark,
                        Err(err) => {
                            unrecorded = Some(format!("couldn't read {}: {err}", path.display()));
                        }
                    }
                }
                Err(LockError::Busy) => {
                    unrecorded = Some(format!(
                        "another process kept {} locked for {}s",
                        path.display(),
                        LOCK_WAIT.as_secs()
                    ));
                }
                Err(LockError::Io(err)) => {
                    unrecorded = Some(format!("couldn't lock {}: {err}", path.display()));
                }
            }
        }

        let ceiling = now_ms.saturating_add(MAX_AHEAD_MS);
        let floor = [recorded.unwrap_or(0), *last]
            .into_iter()
            .filter(|mark| *mark <= ceiling)
            .max()
            .unwrap_or(0);
        let nonce = now_ms.max(floor.saturating_add(1));
        *last = nonce;

        if let (Some(path), Some(_held)) = (store, &lock)
            && let Err(err) = private_fs::write_atomic(path, nonce.to_string().as_bytes())
        {
            unrecorded.get_or_insert_with(|| format!("couldn't write {}: {err}", path.display()));
        }
        Issued { nonce, unrecorded }
    }
}

/// The mark in `path`. A missing file is no mark; so is text that is not a
/// number, which the next write replaces.
fn read_mark(path: &Path) -> std::io::Result<Option<u64>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.trim().parse().ok()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

static GUARD: NonceGuard = NonceGuard::new();
static WARNED: AtomicBool = AtomicBool::new(false);

/// The mark file. Unit tests never touch the machine's data directory.
fn store_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::paths::nonce_high_water_file(&EnvSource::Process)
}

/// The nonce for the next signed action of this process.
pub(crate) fn next() -> u64 {
    let issued = GUARD.next(crate::clock::now_ms(), store_path().as_deref());
    if let Some(problem) = issued.unrecorded
        && !WARNED.swap(true, Ordering::Relaxed)
    {
        crate::output::warn(&format!(
            "Nonce high-water mark not recorded ({problem}); concurrent hyperliquid processes may reuse a nonce."
        ));
    }
    issued.nonce
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    const NOW: u64 = 1_700_000_000_000;

    fn mark_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("data").join("nonce-high-water");
        (dir, path)
    }

    fn recorded(path: &Path) -> u64 {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn a_second_nonce_in_the_same_millisecond_is_larger() {
        let guard = NonceGuard::new();
        let first = guard.next(NOW, None).nonce;
        let second = guard.next(NOW, None).nonce;
        assert_eq!((first, second), (NOW, NOW + 1));
    }

    #[test]
    fn the_clock_going_backwards_does_not_repeat_a_nonce() {
        let guard = NonceGuard::new();
        let first = guard.next(NOW, None).nonce;
        let after_step_back = guard.next(NOW - 5_000, None).nonce;
        assert_eq!(after_step_back, first + 1);
    }

    #[test]
    fn threads_never_share_a_nonce() {
        let guard = Arc::new(NonceGuard::new());
        let (_dir, path) = mark_file();
        let path = Arc::new(path);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (guard, path) = (guard.clone(), path.clone());
                std::thread::spawn(move || {
                    (0..50)
                        .map(|_| guard.next(NOW, Some(&path)).nonce)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let nonces: Vec<u64> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(nonces.iter().collect::<HashSet<_>>().len(), nonces.len());
        assert_eq!(recorded(&path), *nonces.iter().max().unwrap());
    }

    #[test]
    fn guards_sharing_a_mark_file_never_share_a_nonce() {
        // Two processes each own a guard; the file is all they share.
        let (_dir, path) = mark_file();
        let (a, b) = (NonceGuard::new(), NonceGuard::new());
        let mut seen = HashSet::new();
        for _ in 0..20 {
            assert!(seen.insert(a.next(NOW, Some(&path)).nonce));
            assert!(seen.insert(b.next(NOW, Some(&path)).nonce));
        }
    }

    #[test]
    fn a_mark_ahead_of_the_clock_within_the_window_is_respected() {
        let (_dir, path) = mark_file();
        private_fs::write_atomic(&path, (NOW + MAX_AHEAD_MS).to_string().as_bytes()).unwrap();
        let issued = NonceGuard::new().next(NOW, Some(&path));
        assert_eq!(issued.nonce, NOW + MAX_AHEAD_MS + 1);
        assert_eq!(issued.unrecorded, None);
        assert_eq!(recorded(&path), issued.nonce);
    }

    #[test]
    fn a_mark_beyond_the_window_is_ignored_and_replaced() {
        let (_dir, path) = mark_file();
        private_fs::write_atomic(&path, (NOW + MAX_AHEAD_MS + 1).to_string().as_bytes()).unwrap();
        let issued = NonceGuard::new().next(NOW, Some(&path));
        assert_eq!(issued.nonce, NOW);
        assert_eq!(recorded(&path), NOW);
    }

    #[test]
    fn a_garbled_mark_is_no_mark() {
        let (_dir, path) = mark_file();
        private_fs::write_atomic(&path, b"not a number").unwrap();
        assert_eq!(NonceGuard::new().next(NOW, Some(&path)).nonce, NOW);
        assert_eq!(recorded(&path), NOW);
    }

    #[test]
    fn the_clock_going_backwards_respects_the_recorded_mark() {
        let (_dir, path) = mark_file();
        NonceGuard::new().next(NOW, Some(&path));
        // A new process after the clock stepped back by a minute.
        let issued = NonceGuard::new().next(NOW - 60_000, Some(&path));
        assert_eq!(issued.nonce, NOW + 1);
    }

    #[cfg(unix)]
    #[test]
    fn the_mark_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = mark_file();
        NonceGuard::new().next(NOW, Some(&path));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(private_fs::loose_mode(&path), None);
    }

    #[test]
    fn an_unusable_mark_file_still_yields_unique_nonces_and_says_why() {
        let dir = tempfile::TempDir::new().unwrap();
        // A file where the data directory should be.
        let blocker = dir.path().join("data");
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("nonce-high-water");
        let guard = NonceGuard::new();
        let first = guard.next(NOW, Some(&path));
        let second = guard.next(NOW, Some(&path));
        assert_eq!((first.nonce, second.nonce), (NOW, NOW + 1));
        assert!(first.unrecorded.is_some(), "{first:?}");
    }
}
