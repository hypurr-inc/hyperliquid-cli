//! The passive update notice: a cached release lookup refreshed in the
//! background, and a once-a-day stderr line when a newer release exists.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::release::{InstallMethod, ReleaseClient, is_newer_version, normalize_version};
use crate::output::{OutputFormat, terminal};
use crate::settings::env;

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(20 * 60 * 60);
const NOTICE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Longest a command waits for the background refresh before exiting.
const REFRESH_GRACE: Duration = Duration::from_millis(150);

/// `last_notified_at` is optional so caches written by older versions still load.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct VersionCache {
    latest_version: String,
    last_checked_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_notified_at: Option<i64>,
}

/// Handle for one invocation's passive update check.
///
/// Created by [`maybe_start_update_check`] before the command runs and
/// consumed by [`flush_update_check`] after its result is printed.
pub struct UpdateCheck {
    cache_path: Option<PathBuf>,
    refresh: Option<tokio::task::JoinHandle<()>>,
}

impl UpdateCheck {
    fn disabled() -> Self {
        Self {
            cache_path: None,
            refresh: None,
        }
    }
}

/// Conditions under which the passive check and its notice stay silent.
#[derive(Debug, Clone, Copy, Default)]
struct Suppression {
    disabled: bool,
    json: bool,
    no_update_check_env: bool,
    agent: bool,
    ci: bool,
    no_update_notifier: bool,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
}

impl Suppression {
    fn current(format: OutputFormat, disabled: bool) -> Self {
        Self {
            disabled,
            json: format == OutputFormat::Json,
            no_update_check_env: terminal::env_flag(env::NO_UPDATE_CHECK) == terminal::EnvFlag::On,
            agent: terminal::agent_env(),
            ci: terminal::ci_env(),
            no_update_notifier: std::env::var_os(env::NO_UPDATE_NOTIFIER)
                .is_some_and(|value| !value.is_empty()),
            stdout_is_terminal: terminal::stdout_is_terminal(),
            stderr_is_terminal: terminal::stderr_is_terminal(),
        }
    }

    fn allows_check(self) -> bool {
        !(self.disabled
            || self.json
            || self.no_update_check_env
            || self.agent
            || self.ci
            || self.no_update_notifier
            || !self.stdout_is_terminal
            || !self.stderr_is_terminal)
    }
}

/// Start a passive update check. Returns a disabled handle in JSON, agent, CI,
/// `NO_UPDATE_NOTIFIER`, and non-terminal runs. When the cached release info
/// is stale it spawns a detached background refresh. Nothing is printed here:
/// pass the handle to [`flush_update_check`] after the command's result.
pub fn maybe_start_update_check(format: OutputFormat, disabled: bool) -> UpdateCheck {
    start_update_check(
        Suppression::current(format, disabled),
        crate::paths::version_cache_file(),
    )
}

fn start_update_check(suppression: Suppression, cache_path: Option<PathBuf>) -> UpdateCheck {
    let Some(cache_path) = cache_path.filter(|_| suppression.allows_check()) else {
        return UpdateCheck::disabled();
    };

    let refresh_due = read_version_cache(&cache_path)
        .is_none_or(|cache| cache_age(&cache) >= UPDATE_CHECK_INTERVAL);
    let refresh = refresh_due.then(|| tokio::spawn(refresh_version_cache(cache_path.clone())));
    UpdateCheck {
        cache_path: Some(cache_path),
        refresh,
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
/// ~150 ms for the background refresh, then print the update notice on stderr
/// when a newer release is cached and none was shown in the last 24 hours.
pub async fn flush_update_check(mut check: UpdateCheck) {
    settle_refresh(&mut check).await;
    if let Some(cache_path) = check.cache_path.as_deref() {
        if terminal::env_flag(env::AUTO_UPDATE) == terminal::EnvFlag::On {
            maybe_auto_update(cache_path).await;
        } else {
            notify_if_due(cache_path);
        }
    }
}

async fn maybe_auto_update(cache_path: &Path) {
    let Some(cache) = read_version_cache(cache_path) else {
        return;
    };
    let current_version = env!("CARGO_PKG_VERSION");
    if !crate::update::is_newer_version(&cache.latest_version, current_version) {
        return;
    }
    let now = now_unix_seconds();
    if let Some(last_attempt) = cache.last_notified_at
        && now.saturating_sub(last_attempt) < NOTICE_INTERVAL.as_secs() as i64
    {
        return;
    }
    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };
    let install_method = InstallMethod::detect(&current_exe);
    if install_method != InstallMethod::Standalone {
        return;
    }
    let Ok(client) = ReleaseClient::new() else {
        return;
    };
    if !claim_auto_update_attempt(cache_path, now).unwrap_or(false) {
        return;
    }
    eprintln!(
        "Automatically updating hyperliquid from {current_version} to {}...",
        cache.latest_version
    );
    let auto_update_timeout = Some(Duration::from_secs(30));
    if let Err(e) = crate::update::download_and_replace_binary(
        &client,
        &cache.latest_version,
        &current_exe,
        auto_update_timeout,
    )
    .await
    {
        eprintln!("Automatic update failed: {e}");
    } else {
        eprintln!(
            "Successfully updated hyperliquid to {}.",
            cache.latest_version
        );
    }
}

/// Persist the attempt before downloading, checking the timestamp under the cache lock.
fn claim_auto_update_attempt(cache_path: &Path, now: i64) -> Result<bool, anyhow::Error> {
    let mut claimed = false;
    update_version_cache(cache_path, |cache| {
        if cache
            .last_notified_at
            .is_some_and(|last| now.saturating_sub(last) < NOTICE_INTERVAL.as_secs() as i64)
        {
            return;
        }
        cache.last_notified_at = Some(now);
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
    let notified_recently = cache.last_notified_at.is_some_and(|notified| {
        let elapsed = now.saturating_sub(notified);
        (0..NOTICE_INTERVAL.as_secs() as i64).contains(&elapsed)
    });
    if notified_recently {
        return None;
    }
    Some(format!(
        "hyperliquid {} is available. You have {current_version}.\nUpgrade:\n    {upgrade_command}",
        normalize_version(&cache.latest_version)
    ))
}

fn notify_if_due(cache_path: &Path) {
    let Some(cache) = read_version_cache(cache_path) else {
        return;
    };
    let upgrade_command = std::env::current_exe()
        .map(|exe| InstallMethod::detect(&exe))
        .unwrap_or(InstallMethod::Standalone)
        .upgrade_command();
    let now = now_unix_seconds();
    let Some(notice) = update_notice(&cache, env!("CARGO_PKG_VERSION"), now, upgrade_command)
    else {
        return;
    };
    eprintln!("{notice}");
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn allowing() -> Suppression {
        Suppression {
            stdout_is_terminal: true,
            stderr_is_terminal: true,
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
                json: true,
                ..allowing()
            },
            Suppression {
                no_update_check_env: true,
                ..allowing()
            },
            Suppression {
                agent: true,
                ..allowing()
            },
            Suppression {
                ci: true,
                ..allowing()
            },
            Suppression {
                no_update_notifier: true,
                ..allowing()
            },
            Suppression {
                stdout_is_terminal: false,
                ..allowing()
            },
            Suppression {
                stderr_is_terminal: false,
                ..allowing()
            },
        ] {
            assert!(!suppressed.allows_check(), "{suppressed:?}");
        }
    }

    #[test]
    fn suppressed_check_spawns_nothing_and_touches_no_cache() {
        let tmpdir = TempDir::new().unwrap();
        let path = tmpdir.path().join("version.json");
        let check = start_update_check(
            Suppression {
                ci: true,
                ..allowing()
            },
            Some(path.clone()),
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
        let check = start_update_check(allowing(), Some(path.clone()));
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
    fn notice_needs_a_newer_release() {
        assert!(update_notice(&cache_with("v0.11.1", None), "0.11.1", 0, "x").is_none());
        assert!(update_notice(&cache_with("v0.10.0", None), "0.11.1", 0, "x").is_none());
        assert!(update_notice(&cache_with("", None), "0.11.1-rc.1", 0, "x").is_none());
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
        assert!(claim_auto_update_attempt(&path, now).unwrap());
        let cache = read_version_cache(&path).unwrap();
        assert_eq!(cache.last_notified_at, Some(now));
        assert_eq!(cache.latest_version, "v99.0.0");
        assert_eq!(cache.last_checked_at, 123);
        assert!(!claim_auto_update_attempt(&path, now + 1).unwrap());
        let lock = crate::private_fs::FileLock::acquire(&path, Duration::ZERO).unwrap();
        assert!(claim_auto_update_attempt(&path, now + 86400).is_err());
        drop(lock);
        assert!(claim_auto_update_attempt(&path, now + 86400).unwrap());
    }

    #[test]
    fn auto_update_honors_retry_interval() {
        let now = 10 * 24 * 60 * 60;
        let cache_recent = cache_with("v0.12.0", Some(now - 60));
        let elapsed = now.saturating_sub(cache_recent.last_notified_at.unwrap());
        assert!(elapsed < NOTICE_INTERVAL.as_secs() as i64);

        let cache_old = cache_with("v0.12.0", Some(now - 25 * 3600));
        let elapsed_old = now.saturating_sub(cache_old.last_notified_at.unwrap());
        assert!(elapsed_old >= NOTICE_INTERVAL.as_secs() as i64);
    }
}
