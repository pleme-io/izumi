//! State-dir plumbing — the snapshot, the control socket, and the
//! writer-election lock all live under ONE `izumi/` state directory, so a
//! second board instance on the same machine contends the same election and
//! reads the same warm-restart snapshot.

use std::path::PathBuf;

/// The izumi-board snapshot schema magic. Per the [`izumi::persist`] frame
/// contract the magic INCLUDES its trailing newline (frame layout:
/// `MAGIC || blake3-hex || '\n' || json`). Distinct from mado's
/// `b"mado-suggest v1\n"` on purpose: the two boards carry different
/// catalogs, and a wrong-magic read is a typed start-empty, never a
/// garbage-row load.
pub const SNAPSHOT_MAGIC: &[u8] = b"izumi-board v1\n";

/// Env override for the state BASE directory (tests + custom deployments
/// inject a temp dir) — the izumi twin of mado's `MADO_STATE_DIR`.
pub const STATE_DIR_ENV: &str = "IZUMI_STATE_DIR";

/// Resolve the board's state directory: `$IZUMI_STATE_DIR` is an explicit
/// base override; else the OS state dir (`~/.local/state` on Linux —
/// warm-restart data is operator-meaningful *state*, not throwaway cache),
/// falling back to the data dir, then the temp dir. Everything lives under
/// an `izumi/` subdir.
#[must_use]
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os(STATE_DIR_ENV)
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("izumi")
}

/// The warm-restart snapshot path (`<state_dir>/izumi/board.snapshot`) —
/// written by the elected single writer, read by every instance at boot and
/// by the CLI's read-only degraded path.
#[must_use]
pub fn snapshot_path() -> PathBuf {
    state_dir().join("board.snapshot")
}

/// The unix control-socket path (`<state_dir>/izumi/board.sock`) the daemon
/// serves and the CLI verbs dial.
#[must_use]
pub fn socket_path() -> PathBuf {
    state_dir().join("board.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test (not several) so the process-global env mutation can't race
    /// in the parallel test runner — nothing else in this suite reads
    /// `IZUMI_STATE_DIR` (the socket + fallback tests take explicit paths).
    #[test]
    fn state_paths_resolve_the_env_override_then_the_os_state_dir() {
        // SAFETY: env mutation is `unsafe` in edition 2024; this is the sole
        // mutator of this var and runs its set/clear sequentially.
        unsafe {
            std::env::set_var(STATE_DIR_ENV, "/tmp/izumi-state-test");
            assert_eq!(
                snapshot_path(),
                PathBuf::from("/tmp/izumi-state-test/izumi/board.snapshot")
            );
            assert_eq!(
                socket_path(),
                PathBuf::from("/tmp/izumi-state-test/izumi/board.sock")
            );
            std::env::remove_var(STATE_DIR_ENV);
        }
        // Without the override, whatever base resolves, everything sits under
        // the `izumi/` subdir and the two files share one state dir.
        assert_eq!(state_dir().file_name().and_then(|n| n.to_str()), Some("izumi"));
        assert_eq!(snapshot_path().parent(), Some(state_dir().as_path()));
        assert_eq!(socket_path().parent(), Some(state_dir().as_path()));
    }

    /// The frame contract: the schema magic carries its trailing newline (the
    /// caller-supplied magic IS the whole prefix before the blake3 hex).
    #[test]
    fn snapshot_magic_ends_with_a_newline() {
        assert_eq!(SNAPSHOT_MAGIC.last(), Some(&b'\n'));
    }
}
