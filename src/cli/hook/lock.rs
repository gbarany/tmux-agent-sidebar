//! Per-pane serialization for hook handlers.
//!
//! Every hook fire is its own short-lived process, and each handler is a
//! read-modify-write sequence over tmux pane options with no atomicity:
//! `Stop` can read an empty child list while `SubagentStart` is appending
//! to it, or a final `SubagentStop` can cache "turn settled" while
//! `UserPromptSubmit` opens the next turn. Ordering-tolerant handlers
//! cover events that merely arrive late; they cannot cover two handlers
//! interleaving their reads and writes, and tmux offers no
//! compare-and-swap to close that from inside a handler. Holding an
//! advisory lock per pane for the duration of `handle_event` makes each
//! hook observe the state its predecessor left behind.
//!
//! The wait is bounded: a handler wedged in a hung notification daemon
//! must not turn every later hook for that pane into a lost event, so
//! after `LOCK_WAIT` the hook proceeds unlocked. Degraded, but live.
//!
//! The lock file is never removed. `flock` binds to the open file
//! description, so two processes locking different inodes at the same
//! path would exclude nothing; every teardown that deletes per-pane
//! files targets the activity log by exact path, and this file lives
//! under its own name for that reason.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on how long a hook waits for its predecessor on the same pane.
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// Interval between non-blocking acquisition attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Lock file for `pane_id`, encoded like `activity::log_file_path`.
pub(super) fn lock_file_path(pane_id: &str) -> PathBuf {
    let encoded = pane_id.replace('%', "_");
    PathBuf::from(format!("/tmp/tmux-agent-hook{encoded}.lock"))
}

/// Guard for the per-pane advisory lock. Dropping it closes the file
/// description, which releases the lock in the kernel — including when
/// the hook process exits abnormally. The file is held for that `Drop`
/// alone; nothing reads it.
pub(super) struct PaneLock {
    _file: Option<File>,
}

impl PaneLock {
    /// Whether the lock was actually obtained, as opposed to the bounded
    /// wait expiring or the lock file being unopenable.
    #[cfg(test)]
    pub(super) fn held(&self) -> bool {
        self._file.is_some()
    }
}

/// Serialize with other hooks on `pane`, waiting at most `LOCK_WAIT`.
pub(super) fn acquire(pane: &str) -> PaneLock {
    try_lock_until(&lock_file_path(pane), Instant::now() + LOCK_WAIT)
}

/// Poll for an exclusive lock on `path` until `deadline`. Never blocks
/// indefinitely and never fails the hook: an unopenable path or an
/// expired deadline yield an unheld guard and the caller proceeds.
pub(super) fn try_lock_until(path: &Path, deadline: Instant) -> PaneLock {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    else {
        return PaneLock { _file: None };
    };
    loop {
        match try_flock(&file) {
            Ok(()) => return PaneLock { _file: Some(file) },
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                if Instant::now() >= deadline {
                    return PaneLock { _file: None };
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => return PaneLock { _file: None },
        }
    }
}

fn try_flock(file: &File) -> io::Result<()> {
    // `file` outlives the call, so the descriptor is valid for its duration.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_lock(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tmux-agent-hook-lock-test-{name}.lock"))
    }

    #[test]
    fn lock_file_path_encodes_pane_id_like_the_activity_log() {
        assert_eq!(
            lock_file_path("%5").to_str().unwrap(),
            "/tmp/tmux-agent-hook_5.lock"
        );
    }

    #[test]
    fn lock_is_released_on_drop_and_can_be_reacquired() {
        let path = scratch_lock("reacquire");
        let first = try_lock_until(&path, Instant::now() + Duration::from_secs(1));
        assert!(first.held());
        drop(first);
        let second = try_lock_until(&path, Instant::now() + Duration::from_secs(1));
        assert!(second.held(), "dropping the guard must release the lock");
    }

    #[test]
    fn contender_waits_out_the_deadline_then_proceeds_unlocked() {
        // `flock` binds to the open file description, so a second open of
        // the same path within one process contends for real.
        let path = scratch_lock("contend");
        let holder = try_lock_until(&path, Instant::now() + Duration::from_secs(1));
        assert!(holder.held());

        let wait = Duration::from_millis(60);
        let started = Instant::now();
        let contender = try_lock_until(&path, started + wait);
        let elapsed = started.elapsed();

        assert!(!contender.held(), "a held lock must not be granted twice");
        assert!(
            elapsed >= wait,
            "the contender must wait out its deadline before giving up, waited {elapsed:?}"
        );
        assert!(
            elapsed < wait * 10,
            "the bounded wait must not stretch far past the deadline, waited {elapsed:?}"
        );
    }

    #[test]
    fn unopenable_lock_path_yields_an_unheld_guard_without_panicking() {
        let path = scratch_lock("missing-dir").join("nested").join("pane.lock");
        let guard = try_lock_until(&path, Instant::now() + Duration::from_secs(1));
        assert!(!guard.held());
    }
}
