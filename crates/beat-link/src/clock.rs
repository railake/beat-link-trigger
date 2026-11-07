//! Time sources.
//!
//! The Java implementation stamps packets with `System.nanoTime()`, and Beat Link
//! Trigger hands those timestamps (converted to microseconds) to Carabiner, which
//! interprets them on the Ableton Link clock. Link uses the platform's raw monotonic
//! clock, so we read the same clock here rather than an opaque `Instant`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds on the platform monotonic clock shared with Ableton Link.
#[cfg(target_os = "linux")]
pub fn monotonic_nanos() -> u64 {
    read_clock(libc::CLOCK_MONOTONIC)
}

/// Nanoseconds on the platform monotonic clock shared with Ableton Link.
#[cfg(target_os = "macos")]
pub fn monotonic_nanos() -> u64 {
    // Ableton Link on macOS uses mach_absolute_time, which is CLOCK_UPTIME_RAW.
    read_clock(libc::CLOCK_UPTIME_RAW)
}

/// Nanoseconds on the platform monotonic clock shared with Ableton Link.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn monotonic_nanos() -> u64 {
    read_clock(libc::CLOCK_MONOTONIC)
}

#[cfg(unix)]
fn read_clock(id: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec and `id` is a clock the platform defines.
    let rc = unsafe { libc::clock_gettime(id, &mut ts) };
    if rc != 0 {
        return fallback_nanos();
    }
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

/// Nanoseconds on a process-local monotonic clock. On Windows this does not share
/// an epoch with Ableton Link, so Carabiner beat alignment is approximate there.
#[cfg(not(unix))]
pub fn monotonic_nanos() -> u64 {
    fallback_nanos()
}

fn fallback_nanos() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Microseconds on the monotonic clock (the unit Carabiner expects).
pub fn monotonic_micros() -> u64 {
    monotonic_nanos() / 1000
}

/// Milliseconds on the monotonic clock.
pub fn monotonic_millis() -> u64 {
    monotonic_nanos() / 1_000_000
}

/// Wall-clock milliseconds since the Unix epoch (Java's `System.currentTimeMillis`).
pub fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_advances() {
        let a = monotonic_nanos();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = monotonic_nanos();
        assert!(b > a);
        assert!(b - a >= 1_000_000);
    }
}
