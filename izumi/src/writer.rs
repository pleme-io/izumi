//! Single-writer election for shared on-disk state (framed snapshots).
//! Every consumer process READS the state files at boot, but only the
//! election winner ever WRITES them — two processes otherwise race the same
//! atomic-rename target and the last writer silently clobbers the other's
//! rows.
//!
//! The election is an advisory exclusive file lock (`flock`-backed on unix,
//! via `std::fs::File::try_lock`) on a lock file in a CALLER-supplied state
//! dir, held for the election's lifetime once won (the OS releases it on any
//! exit, including a crash — no stale-pid protocol needed). Losers keep full
//! in-memory behavior (and still LOAD the snapshots at boot); they merely
//! skip writing — and they re-contest the role on every
//! [`WriterElection::check`], so the writer seat is re-filled the moment it
//! frees.
//!
//! Non-unix targets return the TYPED [`WriterStatus::Unsupported`] instead of
//! silently claiming writership — no silent wrong answers.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The held election win — dropping it releases the lock (the file closes).
pub struct WriterLock {
    _file: std::fs::File,
}

/// The typed outcome of a writer-election check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterStatus {
    /// THIS process holds the state-writer role (held for the election's
    /// lifetime once won).
    Winner,
    /// Another live process holds the lock — skip writing, re-contest later.
    Loser,
    /// This target has no advisory-lock support — writership CANNOT be
    /// claimed honestly, so persistence must stay off (typed, never silent).
    Unsupported,
}

impl WriterStatus {
    /// Whether this status authorizes writing the shared state files.
    #[must_use]
    pub fn is_writer(self) -> bool {
        matches!(self, WriterStatus::Winner)
    }
}

/// Try to become the writer for `dir` (created if absent). `None` = another
/// live process already holds the lock. Unix only — the election surface for
/// portable callers is [`WriterElection`].
#[cfg(unix)]
#[must_use]
pub fn try_acquire(dir: &Path) -> Option<WriterLock> {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("writer.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    // Advisory exclusive, non-blocking — flock(LOCK_EX | LOCK_NB) on unix.
    // WouldBlock = another open file description holds it; any other error
    // (exotic fs without lock support) also loses, never silently wins.
    match file.try_lock() {
        Ok(()) => Some(WriterLock { _file: file }),
        Err(_) => None,
    }
}

/// A re-contestable single-writer election over one state dir. A win is held
/// for the election's lifetime; a loss is RE-CONTESTED on every
/// [`check`](WriterElection::check) (a cheap non-blocking lock attempt), so
/// when the winning process exits a surviving instance picks the role up on
/// its next maintenance tick — a live system can never end up with zero
/// writers. Both a snapshot persist task and any other shared-state writer
/// gate on this per tick (compose with the store's `generation()` dirty
/// signal — see [`crate::maintain`]).
pub struct WriterElection {
    dir: PathBuf,
    held: Mutex<Option<WriterLock>>,
}

impl WriterElection {
    /// An election over `dir` (the state directory holding `writer.lock`).
    /// No lock is taken until the first [`check`](WriterElection::check).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            held: Mutex::new(None),
        }
    }

    /// The state dir this election contests.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether THIS process currently holds the state-writer role — winner
    /// held for the election lifetime, loser re-contested on every call,
    /// non-unix typed [`WriterStatus::Unsupported`].
    #[must_use]
    pub fn check(&self) -> WriterStatus {
        #[cfg(not(unix))]
        {
            WriterStatus::Unsupported
        }
        #[cfg(unix)]
        {
            let mut held = self
                .held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if held.is_some() {
                return WriterStatus::Winner;
            }
            match try_acquire(&self.dir) {
                Some(lock) => {
                    tracing::info!(
                        dir = %self.dir.display(),
                        "state-writer role acquired — this process persists the shared snapshots"
                    );
                    *held = Some(lock);
                    WriterStatus::Winner
                }
                None => WriterStatus::Loser,
            }
        }
    }

    /// Convenience: `check() == Winner`.
    #[must_use]
    pub fn is_writer(&self) -> bool {
        self.check().is_writer()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_on_the_same_dir_loses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir = dir.path().join("election");
        // flock is per open-file-description, so two opens in ONE process
        // still contend — the test needs no second process.
        let first = try_acquire(&dir);
        assert!(first.is_some(), "first acquire wins");
        assert!(try_acquire(&dir).is_none(), "second acquire loses while held");
        drop(first);
        assert!(
            try_acquire(&dir).is_some(),
            "the lock releases on drop (and on process exit)"
        );
    }

    #[test]
    fn election_wins_holds_and_recontests() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("election2");
        let a = WriterElection::new(&dir);
        let b = WriterElection::new(&dir);
        assert_eq!(a.check(), WriterStatus::Winner, "first election wins");
        assert_eq!(a.check(), WriterStatus::Winner, "a win is held, not re-fought");
        assert!(a.is_writer());
        assert_eq!(b.check(), WriterStatus::Loser, "the seat is taken");
        assert!(!b.is_writer());
        drop(a);
        // The loser RE-CONTESTS on every check and picks the freed seat up.
        assert_eq!(b.check(), WriterStatus::Winner, "seat re-filled after release");
    }
}
