//! The passive update notice: a cached release lookup refreshed in the
//! background, and a once-a-day stderr line when a newer release exists.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::release::{InstallMethod, ReleaseClient, is_newer_version, normalize_version};
use crate::clock;
use crate::mode::Mode;
use crate::output::sink;

use crate::env::{self, EnvSource};

const UPDATE_CHECK_INTERVAL: Duration = clock::HOUR.of(20);
const NOTICE_INTERVAL_SECS: i64 = clock::DAY.of(1).as_secs() as i64;
/// Longest an automatic update may spend downloading.
const AUTO_UPDATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a command waits for the background refresh before exiting.
const REFRESH_GRACE: Duration = Duration::from_millis(150);

/// The optional timestamps are absent from caches written by older versions,
/// which still load.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct VersionCache {
    latest_version: String,
    last_checked_at: i64,
    /// When the update notice last printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_notified_at: Option<i64>,
    /// When an automatic update was last attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_attempted_at: Option<i64>,
}

impl VersionCache {
    /// The update notice may print: none printed in the last day. A timestamp
    /// in the future (the clock moved back) counts as long ago, so a wrong
    /// clock cannot silence the notice.
    fn notice_due(&self, now: i64) -> bool {
        !within_notice_interval(self.last_notified_at, now)
    }

    /// An automatic update may start: none attempted in the last day.
    fn auto_update_due(&self, now: i64) -> bool {
        !within_notice_interval(self.last_attempted_at, now)
    }
}

fn within_notice_interval(last: Option<i64>, now: i64) -> bool {
    last.is_some_and(|last| (0..NOTICE_INTERVAL_SECS).contains(&now.saturating_sub(last)))
}

/// Handle for one invocation's passive update check.
///
/// Created by [`maybe_start_update_check`] before the command runs and
/// consumed by [`flush_update_check`] after its result is printed.
pub struct UpdateCheck {
    cache_path: Option<PathBuf>,
    refresh: Option<tokio::task::JoinHandle<()>>,
    /// `HYPERLIQUID_AUTO_UPDATE` was on when the check started.
    auto_update: bool,
    /// The environment the check started in, for locating the running install.
    env: EnvSource,
}

impl UpdateCheck {
    fn disabled() -> Self {
        Self {
            cache_path: None,
            refresh: None,
            auto_update: false,
            env: EnvSource::default(),
        }
    }
}

/// Conditions under which the passive check and its notice stay silent. Whether
/// a person is at the terminal comes from the run's [`Mode`]; the rest are this
/// feature's own switches.
#[derive(Debug, Clone, Copy, Default)]
struct Suppression {
    disabled: bool,
    no_update_check_env: bool,
    no_update_notifier: bool,
    person_at_terminal: bool,
}

impl Suppression {
    fn current(mode: &Mode, disabled: bool, source: &EnvSource) -> Self {
        Self {
            disabled,
            no_update_check_env: env::flag(source, env::NO_UPDATE_CHECK) == env::EnvFlag::On,
            no_update_notifier: source
                .get_os(env::NO_UPDATE_NOTIFIER)
                .is_some_and(|value| !value.is_empty()),
            person_at_terminal: mode.person_at_terminal(),
        }
    }

    fn allows_check(self) -> bool {
        self.person_at_terminal
            && !(self.disabled || self.no_update_check_env || self.no_update_notifier)
    }
}

/// Start a passive update check. Returns a disabled handle in JSON, agent, CI,
/// `NO_UPDATE_NOTIFIER`, and non-terminal runs. When the cached release info
/// is stale it spawns a detached background refresh. Nothing is printed here:
/// pass the handle to [`flush_update_check`] after the command's result.
pub fn maybe_start_update_check(mode: &Mode, disabled: bool, source: &EnvSource) -> UpdateCheck {
    start_update_check(
        Suppression::current(mode, disabled, source),
        crate::paths::version_cache_file(source),
        env::flag(source, env::AUTO_UPDATE) == env::EnvFlag::On,
        source,
    )
}

fn start_update_check(
    suppression: Suppression,
    cache_path: Option<PathBuf>,
    auto_update: bool,
    source: &EnvSource,
) -> UpdateCheck {
    let Some(cache_path) = cache_path.filter(|_| suppression.allows_check()) else {
        return UpdateCheck::disabled();
    };

    let refresh_due = read_version_cache(&cache_path)
        .is_none_or(|cache| cache_age(&cache) >= UPDATE_CHECK_INTERVAL);
    let refresh = refresh_due.then(|| tokio::spawn(refresh_version_cache(cache_path.clone())));
    UpdateCheck {
        cache_path: Some(cache_path),
        refresh,
        auto_update,
        env: source.clone(),
    }
}

/// Give a pending refresh a moment to finish. The refresh is detached: if it
/// is not done in time the command still exits right away.
async fn settle_refresh(check: &mut UpdateCheck) {
    if let Some(handle) = check.refresh.take() {
        let _ = tokio::time::timeout(REFRESH_GRACE, handle).await;
    }
}

/// Finish the passive check after a successful command: wait at most
/// [`REFRESH_GRACE`] for the background refresh, then print the update notice on stderr
/// when a newer release is cached and none was shown within [`NOTICE_INTERVAL_SECS`].
pub async fn flush_update_check(mut check: UpdateCheck) {
    settle_refresh(&mut check).await;
    if let Some(cache_path) = check.cache_path.as_deref() {
        if check.auto_update {
            maybe_auto_update(cache_path, &check.env).await;
        } else {
            notify_if_due(cache_path, &check.env);
        }
    }
}

async fn maybe_auto_update(cache_path: &Path, source: &EnvSource) {
    let Some(cache) = read_version_cache(cache_path) else {
        return;
    };
    let current_version = env!("CARGO_PKG_VERSION");
    if !is_newer_version(&cache.latest_version, current_version) {
        return;
    }
    let now = now_unix_seconds();
    if !cache.auto_update_due(now) {
        return;
    }
    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };
    let install_method = InstallMethod::detect(&current_exe, source);
    if install_method != InstallMethod::Standalone {
        return;
    }
    let Ok(client) = ReleaseClient::new() else {
        return;
    };
    if !claim_auto_update_attempt(cache_path, now).unwrap_or(false) {
        return;
    }
    sink::err_line(&format!(
        "Automatically updating hyperliquid from {current_version} to {}...",
        cache.latest_version
    ));
    if let Err(e) = super::self_update::download_and_replace_binary(
        &client,
        &cache.latest_version,
        &current_exe,
        Some(AUTO_UPDATE_TIMEOUT),
    )
    .await
    {
        sink::err_line(&format!("Automatic update failed: {e}"));
    } else {
        sink::err_line(&format!(
            "Successfully updated hyperliquid to {}.",
            cache.latest_version
        ));
    }
}

/// Persist the attempt before downloading, checking the timestamp under the cache lock.
fn claim_auto_update_attempt(cache_path: &Path, now: i64) -> Result<bool, anyhow::Error> {
    let mut claimed = false;
    update_version_cache(cache_path, |cache| {
        if !cache.auto_update_due(now) {
            return;
        }
        cache.last_attempted_at = Some(now);
        claimed = true;
    })?;
    Ok(claimed)
}

/// Finish the passive check after a failed command: same bounded wait, no notice.
pub async fn flush_update_check_silent(mut check: UpdateCheck) {
    settle_refresh(&mut check).await;
}

/// The update notice for a cache state, or `None` when nothing is due.
fn update_notice(
    cache: &VersionCache,
    current_version: &str,
    now: i64,
    upgrade_command: &str,
) -> Option<String> {
    if cache.latest_version.is_empty() || !is_newer_version(&cache.latest_version, current_version)
    {
        return None;
    }
    if !cache.notice_due(now) {
        return None;
    }
    Some(format!(
        "hyperliquid {} is available. You have {current_version}.\nUpgrade:\n    {upgrade_command}",
        normalize_version(&cache.latest_version)
    ))
}

fn notify_if_due(cache_path: &Path, source: &EnvSource) {
    let Some(cache) = read_version_cache(cache_path) else {
        return;
    };
    let upgrade_command = std::env::current_exe()
        .map(|exe| InstallMethod::detect(&exe, source))
        .unwrap_or(InstallMethod::Standalone)
        .upgrade_command();
    let now = now_unix_seconds();
    let Some(notice) = update_notice(&cache, env!("CARGO_PKG_VERSION"), now, upgrade_command)
    else {
        return;
    };
    sink::err_line(&notice);
    let _ = update_version_cache(cache_path, |cache| cache.last_notified_at = Some(now));
}

/// Read-modify-write the version cache atomically and owner-only.
fn update_version_cache(
    cache_path: &Path,
    change: impl FnOnce(&mut VersionCache),
) -> Result<(), anyhow::Error> {
    let _lock = crate::private_fs::FileLock::acquire(cache_path, Duration::ZERO)
        .map_err(|err| anyhow::anyhow!("version cache lock: {err:?}"))?;
    let mut cache = read_version_cache(cache_path).unwrap_or_default();
    change(&mut cache);
    let contents = format!("{}\n", serde_json::to_string(&cache)?);
    crate::private_fs::write_atomic(cache_path, contents.as_bytes())?;
    Ok(())
}

/// Fetch the latest release and record it. A failed fetch still records
/// `last_checked_at` so the next runs back off for the check interval.
async fn refresh_version_cache(cache_path: PathBuf) {
    let latest = match ReleaseClient::new() {
        Ok(client) => client.latest_version().await.ok(),
        Err(_) => None,
    };
    let _ = update_version_cache(&cache_path, |cache| {
        if let Some(latest) = latest {
            cache.latest_version = latest;
        }
        cache.last_checked_at = now_unix_seconds();
    });
}

fn read_version_cache(path: &Path) -> Option<VersionCache> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn cache_age(cache: &VersionCache) -> Duration {
    let elapsed = now_unix_seconds().saturating_sub(cache.last_checked_at);
    if elapsed < 0 {
        return Duration::MAX;
    }
    Duration::from_secs(elapsed as u64)
}

fn now_unix_seconds() -> i64 {
    (clock::now_ms() / clock::SECOND.ms(1)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::OutputFormat;
    use crate::terminal::Terminal;
    use tempfile::TempDir;

    fn allowing() -> Suppression {
        Suppression {
            person_at_terminal: true,
            ..Suppression::default()
        }
    }

    #[test]
    fn check_runs_only_for_interactive_human_sessions() {
        assert!(allowing().allows_check());
        for suppressed in [
            Suppression {
                disabled: true,
                ..allowing()
            },
            Suppression {
                no_update_check_env: true,
                ..allowing()
            },
            Suppression {
                no_update_notifier: true,
                ..allowing()
            },
            Suppression {
                person_at_terminal: false,
                ..allowing()
            },
        ] {
            assert!(!suppressed.allows_check(), "{suppressed:?}");
        }
    }

    #[test]
    fn suppression_follows_the_mode_of_the_run() {
        let terminal = Mode {
            terminal: Terminal {
                stdin: true,
                stdout: true,
                stderr: true,
            },
            ..Mode::default()
        };
        assert!(
            Suppression::current(&terminal, false, &EnvSource::fixed::<&str, &str>([]))
                .person_at_terminal
        );
        for mode in [
            Mode {
                format: OutputFormat::Json,
                ..terminal
            },
            Mode {
                agent: true,
                ..terminal
            },
            Mode {
                ci: true,
                ..terminal
            },
            Mode {
                terminal: Terminal {
                    stdout: false,
                    ..terminal.terminal
                },
                ..terminal
            },
            Mode {
                terminal: Terminal {
                    stderr: false,
                    ..terminal.terminal
                },
                ..terminal
            },
        ] {
            assert!(
                !Suppression::current(&mode, false, &EnvSource::fixed::<&str, &str>([]))
                    .person_at_terminal
            );
        }
    }

    #[test]
    fn suppressed_check_spawns_nothing_and_touches_no_cache() {
        let tmpdir = TempDir::new().unwrap();
        let path = tmpdir.path().join("version.json");
        let check = start_update_check(
            Suppression {
                person_at_terminal: false,
                ..allowing()
            },
            Some(path.clone()),
            false,
            &EnvSource::default(),
        );
        assert!(check.cache_path.is_none());
        assert!(check.refresh.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn fresh_cache_needs_no_refresh() {
        let tmpdir = TempDir::new().unwrap();
        let path = tmpdir.path().join("version.json");
        update_version_cache(&path, |cache| {
            cache.latest_version = "v99.0.0".to_string();
            cache.last_checked_at = now_unix_seconds();
        })
        .unwrap();
        let check =
            start_update_check(allowing(), Some(path.clone()), false, &EnvSource::default());
        assert_eq!(check.cache_path.as_deref(), Some(path.as_path()));
        assert!(check.refresh.is_none());
    }

    #[test]
    fn old_cache_files_without_last_notified_at_still_load() {
        let tmpdir = TempDir::new().unwrap();
        let path = tmpdir.path().join("version.json");
        std::fs::write(
            &path,
            "{\"latest_version\":\"v1.2.3\",\"last_checked_at\":10}\n",
        )
        .unwrap();
        let cache = read_version_cache(&path).unwrap();
        assert_eq!(cache.latest_version, "v1.2.3");
        assert_eq!(cache.last_notified_at, None);

        update_version_cache(&path, |cache| cache.last_notified_at = Some(20)).unwrap();
        let cache = read_version_cache(&path).unwrap();
        assert_eq!(cache.latest_version, "v1.2.3");
        assert_eq!(cache.last_checked_at, 10);
        assert_eq!(cache.last_notified_at, Some(20));
    }

    #[cfg(unix)]
    #[test]
    fn cache_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmpdir = TempDir::new().unwrap();
        let path = tmpdir.path().join("nested").join("version.json");
        update_version_cache(&path, |cache| cache.latest_version = "v1.0.0".to_string()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn cache_with(latest: &str, notified: Option<i64>) -> VersionCache {
        VersionCache {
            latest_version: latest.to_string(),
            last_checked_at: 0,
            last_notified_at: notified,
            last_attempted_at: None,
        }
    }

    #[test]
    fn notice_names_versions_and_upgrade_command() {
        let notice = update_notice(
            &cache_with("v0.12.0", None),
            "0.11.1",
            1_000,
            "hyperliquid update",
        );
        assert_eq!(
            notice.as_deref(),
            Some(
                "hyperliquid 0.12.0 is available. You have 0.11.1.\nUpgrade:\n    hyperliquid update"
            )
        );
    }

    #[test]
    fn notice_shows_at_most_once_per_day() {
        let now = 10 * 24 * 60 * 60;
        let due = |notified| {
            update_notice(&cache_with("v0.12.0", Some(notified)), "0.11.1", now, "x").is_some()
        };
        assert!(!due(now - 60));
        assert!(!due(now - 24 * 60 * 60 + 1));
        assert!(due(now - 24 * 60 * 60));
        assert!(
            due(now + 60),
            "a notification timestamp in the future is ignored"
        );
    }

    #[test]
    fn the_notice_and_the_automatic_attempt_throttle_independently() {
        let now = 10 * 24 * 60 * 60;
        let notified = VersionCache {
            last_notified_at: Some(now - 60),
            ..cache_with("v0.12.0", None)
        };
        assert!(!notified.notice_due(now));
        assert!(notified.auto_update_due(now));
        let attempted = VersionCache {
            last_attempted_at: Some(now - 60),
            ..cache_with("v0.12.0", None)
        };
        assert!(attempted.notice_due(now));
        assert!(!attempted.auto_update_due(now));
    }

    #[test]
    fn a_timestamp_in_the_future_never_throttles_the_attempt_either() {
        let now = 10 * 24 * 60 * 60;
        let skewed = VersionCache {
            last_notified_at: Some(now + 60),
            last_attempted_at: Some(now + 60),
            ..cache_with("v0.12.0", None)
        };
        assert!(skewed.notice_due(now));
        assert!(skewed.auto_update_due(now));
    }

    #[test]
    fn notice_needs_a_newer_release() {
        assert!(update_notice(&cache_with("v0.11.1", None), "0.11.1", 0, "x").is_none());
        assert!(update_notice(&cache_with("v0.10.0", None), "0.11.1", 0, "x").is_none());
        assert!(update_notice(&cache_with("", None), "0.11.1-rc.1", 0, "x").is_none());
    }

    /// `claim_auto_update_attempt` never waits for the cache lock, and a
    /// concurrently forked child can briefly keep a just-released lock held, so
    /// calls that are not asserting contention retry while the lock is busy.
    fn claim_when_unlocked(path: &Path, now: i64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match claim_auto_update_attempt(path, now) {
                Ok(claimed) => return claimed,
                Err(err) if std::time::Instant::now() >= deadline => {
                    panic!("cache lock stayed busy: {err}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn auto_update_attempt_is_persisted_and_throttles_other_invocations() {
        let tmpdir = TempDir::new().unwrap();
        let path = tmpdir.path().join("version.json");
        update_version_cache(&path, |cache| {
            cache.latest_version = "v99.0.0".into();
            cache.last_checked_at = 123;
        })
        .unwrap();
        let now = 1_000_000;
        assert!(claim_when_unlocked(&path, now));
        let cache = read_version_cache(&path).unwrap();
        assert_eq!(cache.last_attempted_at, Some(now));
        assert_eq!(
            cache.last_notified_at, None,
            "an attempt is not a notification"
        );
        assert_eq!(cache.latest_version, "v99.0.0");
        assert_eq!(cache.last_checked_at, 123);
        assert!(!claim_when_unlocked(&path, now + 1));
        let lock = crate::private_fs::FileLock::acquire(&path, Duration::from_secs(5)).unwrap();
        assert!(claim_auto_update_attempt(&path, now + 86400).is_err());
        drop(lock);
        assert!(claim_when_unlocked(&path, now + 86400));
    }
}
