// src/health.rs
//
// ============================================================================
// HEALTH MONITORING — v0.12
// ============================================================================
//
// Designed for the 4-week unattended Jetson dry-run. Three concerns:
//
//   1. LIVENESS — is the bot still running? The trade-driven status log
//      can go many minutes between prints during quiet sessions, which
//      makes it hard to distinguish "alive but quiet" from "hung". A
//      time-based heartbeat fires every HEARTBEAT_SECS regardless of
//      market activity.
//
//   2. DISK SPACE — persistence writes ~10 GB/day. A 1 TB SSD covers
//      ~3 months, but if anything else lands on the mount or the
//      buffer fills with retries, we want to know well before the wall.
//      Three thresholds: INFO at 50%, WARN at 80%, ERROR at 95%.
//
//   3. PERSISTENCE PATH HEALTH — the SSD mount could disappear
//      mid-run (cable jiggle, drive failure, mount unit issue). The
//      writers will silently start dropping records. A periodic
//      canary write tests the real failure mode (writability) and
//      surfaces the problem loudly.
//
// All three run in a single background task with a unified interval.
// The task is parameterized for testability — none of this code touches
// global state.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::interval;
use tracing::{info, warn, error};

/// Default heartbeat / health-check cadence. 60 seconds is a reasonable
/// signal-to-noise tradeoff: tight enough to notice a hang within a
/// minute, loose enough not to spam logs over a 4-week run.
pub const DEFAULT_INTERVAL_SECS: u64 = 60;

/// Disk-fill warning thresholds (percent USED, not free).
pub const DISK_INFO_PCT:  f64 = 50.0;
pub const DISK_WARN_PCT:  f64 = 80.0;
pub const DISK_ERROR_PCT: f64 = 95.0;

#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// Path to monitor (e.g. /mnt/usb_ssd/augur_data).
    pub persistence_path: PathBuf,
    /// Health-check cadence.
    pub interval: Duration,
    /// If true, perform the canary-write test each pass. Disabled
    /// in tests where we don't actually want disk side effects.
    pub canary_write: bool,
}

impl HealthConfig {
    pub fn from_env(persistence_path: PathBuf) -> Self {
        let interval = Duration::from_secs(
            std::env::var("AUGUR_HEALTH_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_INTERVAL_SECS),
        );
        HealthConfig {
            persistence_path,
            interval,
            canary_write: true,
        }
    }
}

// ============================================================================
// DISK SPACE — Linux-only via libc::statvfs
// ============================================================================

#[derive(Debug, Clone, Copy)]
pub struct DiskSpace {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub used_pct: f64,
}

/// Query disk space for the filesystem containing `path`. Returns None
/// on any error (path doesn't exist, statvfs fails). The caller treats
/// None as "couldn't measure" and logs accordingly.
#[cfg(unix)]
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    use std::os::unix::ffi::OsStrExt;
    let path_bytes = path.as_os_str().as_bytes();
    let mut c_path = Vec::with_capacity(path_bytes.len() + 1);
    c_path.extend_from_slice(path_bytes);
    c_path.push(0);  // null terminator

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr() as *const _, &mut stat) };
    if rc != 0 {
        return None;
    }

    // f_frsize: fundamental block size. f_blocks: total blocks.
    // f_bavail: blocks available to a non-root user (the relevant
    // measure of "what we can actually write").
    let total = stat.f_blocks as u64 * stat.f_frsize as u64;
    let free  = stat.f_bavail as u64 * stat.f_frsize as u64;
    if total == 0 {
        return None;
    }
    let used_pct = ((total - free) as f64 / total as f64) * 100.0;
    Some(DiskSpace { total_bytes: total, free_bytes: free, used_pct })
}

#[cfg(not(unix))]
pub fn disk_space(_path: &Path) -> Option<DiskSpace> {
    None  // not implemented on non-Unix; the dry-run target is Linux Jetson
}

// ============================================================================
// CANARY WRITE
// ============================================================================

/// Write and immediately delete a tiny file in `dir` to test that the
/// path is currently writable. Returns Err with a descriptive message
/// on any failure (mount gone, permission lost, disk full, etc.).
///
/// Uses a fixed filename — if a previous canary failed mid-write and
/// left the file behind, this overwrites it. We don't try to be clever
/// with timestamps in the name; one canary at a time.
pub fn canary_write(dir: &Path) -> Result<(), String> {
    let canary = dir.join(".augur_canary");
    // Write a small payload — large enough to actually exercise the
    // filesystem, small enough not to dent the SSD's write budget.
    if let Err(e) = std::fs::write(&canary, b"augur health check") {
        return Err(format!("canary write to {} failed: {}", canary.display(), e));
    }
    // Verify by reading back. Catches cases where write reports success
    // but the data didn't land (rare, but cheap to check).
    match std::fs::read(&canary) {
        Ok(content) if content == b"augur health check" => {}
        Ok(_) => return Err(format!("canary content mismatch at {}", canary.display())),
        Err(e) => return Err(format!("canary read-back failed: {}", e)),
    }
    if let Err(e) = std::fs::remove_file(&canary) {
        // Non-fatal — log it but don't fail the health check. A leftover
        // canary file is cosmetic.
        tracing::debug!("[health] canary cleanup failed (non-fatal): {}", e);
    }
    Ok(())
}

// ============================================================================
// HEALTH CHECK PASS
// ============================================================================

#[derive(Debug, Clone, Default)]
pub struct HealthSnapshot {
    pub disk: Option<DiskSpace>,
    pub canary_ok: bool,
    pub canary_error: Option<String>,
}

/// Perform one health check pass. Returns a snapshot caller can log
/// or aggregate. This is a sync function — we call it from inside
/// the async monitor loop via the standard `await` of an async block.
pub fn perform_check(config: &HealthConfig) -> HealthSnapshot {
    let disk = disk_space(&config.persistence_path);
    let (canary_ok, canary_error) = if config.canary_write {
        match canary_write(&config.persistence_path) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e)),
        }
    } else {
        (true, None)  // disabled in tests
    };
    HealthSnapshot { disk, canary_ok, canary_error }
}

/// Format a human-readable byte count. 0..1024 bytes, then KB/MB/GB/TB.
fn fmt_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{} {}", b, UNITS[i]) }
    else      { format!("{:.2} {}", v, UNITS[i]) }
}

/// Long-running monitor task. Wraps perform_check + log emission.
/// Exits when the cancellation channel closes (which happens on SIGINT
/// in main.rs's shutdown sequence).
pub async fn run(config: HealthConfig, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    info!(
        "[health] monitor started — checking {} every {}s",
        config.persistence_path.display(),
        config.interval.as_secs(),
    );

    let mut tick = interval(config.interval);
    // First tick fires immediately by default. Skip it so the heartbeat
    // doesn't double-fire alongside the startup banner.
    tick.tick().await;

    let mut consecutive_canary_failures: u32 = 0;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let snap = perform_check(&config);

                // --- Heartbeat (always logged) ---
                let disk_str = match &snap.disk {
                    Some(d) => format!(
                        "disk: {}/{} used ({:.1}%)",
                        fmt_bytes(d.total_bytes - d.free_bytes),
                        fmt_bytes(d.total_bytes),
                        d.used_pct,
                    ),
                    None => "disk: (unavailable)".to_string(),
                };
                info!("💓 [health] heartbeat | {}", disk_str);

                // --- Disk thresholds ---
                if let Some(d) = &snap.disk {
                    if d.used_pct >= DISK_ERROR_PCT {
                        error!(
                            "[health] DISK CRITICAL: {:.1}% used, only {} free — \
                             persistence will fail soon!",
                            d.used_pct, fmt_bytes(d.free_bytes),
                        );
                    } else if d.used_pct >= DISK_WARN_PCT {
                        warn!(
                            "[health] disk WARN: {:.1}% used, {} free remaining",
                            d.used_pct, fmt_bytes(d.free_bytes),
                        );
                    } else if d.used_pct >= DISK_INFO_PCT {
                        info!(
                            "[health] disk fill: {:.1}% used, {} free",
                            d.used_pct, fmt_bytes(d.free_bytes),
                        );
                    }
                }

                // --- Canary write ---
                if !snap.canary_ok {
                    consecutive_canary_failures += 1;
                    error!(
                        "[health] CANARY FAILURE #{}: {} — \
                         persistence path is not writable, ticks/books are being dropped!",
                        consecutive_canary_failures,
                        snap.canary_error.as_deref().unwrap_or("(no detail)"),
                    );
                } else {
                    if consecutive_canary_failures > 0 {
                        info!(
                            "[health] canary recovered after {} failures",
                            consecutive_canary_failures,
                        );
                    }
                    consecutive_canary_failures = 0;
                }
            }

            // Cancellation: caller signals shutdown via the watch channel.
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("[health] monitor stopping on shutdown signal");
                    break;
                }
            }
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fmt_bytes_human_readable() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.00 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.00 MB");
        assert_eq!(fmt_bytes(3_500_000_000), "3.26 GB");
    }

    #[test]
    fn disk_space_returns_some_for_existing_path() {
        let dir = tempdir().unwrap();
        let space = disk_space(dir.path());
        assert!(space.is_some(), "tmpdir should be statvfs-able");
        let s = space.unwrap();
        assert!(s.total_bytes > 0);
        // free_bytes should not exceed total_bytes
        assert!(s.free_bytes <= s.total_bytes);
        // used_pct sane
        assert!(s.used_pct >= 0.0 && s.used_pct <= 100.0);
    }

    #[test]
    fn disk_space_returns_none_for_nonexistent_path() {
        let path = PathBuf::from("/this/path/absolutely/does/not/exist/augur");
        let space = disk_space(&path);
        assert!(space.is_none());
    }

    #[test]
    fn canary_write_succeeds_in_tmpdir() {
        let dir = tempdir().unwrap();
        let result = canary_write(dir.path());
        assert!(result.is_ok(), "canary write should succeed: {:?}", result);
        // Canary file should be cleaned up.
        assert!(!dir.path().join(".augur_canary").exists(),
            "canary file should be removed after success");
    }

    #[test]
    fn canary_write_fails_for_nonexistent_dir() {
        let path = PathBuf::from("/this/path/definitely/does/not/exist");
        let result = canary_write(&path);
        assert!(result.is_err());
    }

    #[test]
    fn perform_check_aggregates_signals() {
        let dir = tempdir().unwrap();
        let cfg = HealthConfig {
            persistence_path: dir.path().to_path_buf(),
            interval: Duration::from_secs(60),
            canary_write: true,
        };
        let snap = perform_check(&cfg);
        assert!(snap.disk.is_some(), "disk space should be measurable");
        assert!(snap.canary_ok, "canary should succeed in tmpdir");
        assert!(snap.canary_error.is_none());
    }

    #[test]
    fn perform_check_canary_disabled() {
        let cfg = HealthConfig {
            persistence_path: PathBuf::from("/this/does/not/exist"),
            interval: Duration::from_secs(60),
            canary_write: false,  // disabled
        };
        let snap = perform_check(&cfg);
        // canary_ok = true even though path is bad, because canary
        // wasn't attempted.
        assert!(snap.canary_ok);
        assert!(snap.disk.is_none());
    }
}
