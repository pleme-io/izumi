//! Snapshot framing + crash-safe atomic persistence — the shared on-disk
//! contract every izumi snapshot (and any other framed state file) writes
//! through.
//!
//! Frame layout: `MAGIC || blake3-hex || '\n' || json`. The magic is a
//! CALLER-supplied schema tag (the caller includes any trailing newline in
//! the magic bytes — mado's `b"mado-suggest v1\n"` includes its `\n`, so the
//! parameterized frame is byte-identical to the mado original); the embedded
//! BLAKE3 hash makes a torn file detectable on load. A wrong magic (schema
//! bump / foreign file) or a hash mismatch both mean start-empty — never feed
//! garbage rows to a consumer.

use std::path::Path;

/// Frame a JSON snapshot: `magic || blake3-hex || '\n' || json`. The magic is
/// a schema tag; the embedded hash makes a torn file detectable on load.
#[must_use]
pub fn frame_snapshot(magic: &[u8], json: &[u8]) -> Vec<u8> {
    let hex = blake3::hash(json).to_hex();
    let mut out = Vec::with_capacity(magic.len() + hex.len() + 1 + json.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(hex.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(json);
    out
}

/// Inverse of [`frame_snapshot`]: `None` on wrong magic (schema bump) or a
/// hash mismatch (torn/corrupt) — both mean start-empty.
#[must_use]
pub fn unframe_snapshot(magic: &[u8], bytes: &[u8]) -> Option<Vec<u8>> {
    let rest = bytes.strip_prefix(magic)?;
    let nl = rest.iter().position(|&b| b == b'\n')?;
    let (hex, after) = rest.split_at(nl);
    let json = &after[1..]; // skip the newline
    if blake3::hash(json).to_hex().as_bytes() != hex {
        return None; // torn / corrupt
    }
    Some(json.to_vec())
}

/// Frame `json` (magic + BLAKE3) and atomically write it to `path`:
/// `create_dir_all` + a pid-tagged temp (two processes never race one temp
/// name) + `sync_all` + rename. The shared persistence primitive every framed
/// snapshot writes through. Best-effort — a failure never blocks the caller.
pub fn atomic_write_framed(magic: &[u8], path: &Path, json: &[u8]) {
    use std::io::Write;
    let framed = frame_snapshot(magic, json);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp.");
    tmp.push(std::process::id().to_string());
    let tmp = std::path::PathBuf::from(tmp);
    let Ok(mut f) = std::fs::File::create(&tmp) else {
        return;
    };
    if f.write_all(&framed).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    let _ = f.sync_all(); // durable before the rename
    drop(f);
    let _ = std::fs::rename(&tmp, path);
}

/// Reclaim sibling temp files (`<file>.tmp.<pid>`) left by a crashed prior
/// persist. Default staleness floor is 5 minutes so a CONCURRENT process's
/// in-flight temp — always seconds old — is never deleted out from under it.
/// Best-effort; any I/O error is ignored.
pub fn sweep_orphan_temps(path: &Path) {
    sweep_orphan_temps_with(path, std::time::SystemTime::now(), 300);
}

/// Inner, testable form of [`sweep_orphan_temps`]: `now` + the staleness floor
/// are injected so a test can prove both directions (fresh temp kept, stale
/// temp reclaimed) without touching a file's real mtime.
fn sweep_orphan_temps_with(path: &Path, now: std::time::SystemTime, max_age_secs: u64) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let mut prefix = String::from(fname);
    prefix.push_str(".tmp.");
    let Ok(rd) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Only `<file>.tmp.<digits>` — our own pid-tagged temp shape.
        let Some(suffix) = name.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age.as_secs() >= max_age_secs);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGIC: &[u8] = b"izumi-test v1\n";

    #[test]
    fn frame_unframe_round_trips_pure() {
        // The pure pair round-trips without touching the filesystem.
        let json = br#"{"entries":[],"saved_at_ms":42}"#;
        let framed = frame_snapshot(MAGIC, json);
        assert!(framed.starts_with(MAGIC), "frame leads with the magic");
        assert_eq!(unframe_snapshot(MAGIC, &framed).as_deref(), Some(&json[..]));
    }

    #[test]
    fn frame_layout_is_magic_hex_newline_json() {
        // Byte-exact layout contract: magic || blake3-hex || '\n' || json —
        // identical to the mado frame when magic carries its trailing newline.
        let json = b"{}";
        let framed = frame_snapshot(MAGIC, json);
        let hex = blake3::hash(json).to_hex();
        let mut expect = Vec::new();
        expect.extend_from_slice(MAGIC);
        expect.extend_from_slice(hex.as_bytes());
        expect.push(b'\n');
        expect.extend_from_slice(json);
        assert_eq!(framed, expect);
    }

    #[test]
    fn unframe_rejects_wrong_magic_and_torn_body() {
        let json = b"{\"k\":1}";
        let mut framed = frame_snapshot(MAGIC, json);
        // Wrong magic (a foreign/old file) → None.
        assert_eq!(unframe_snapshot(b"other-magic v9\n", &framed), None);
        // A torn body (flip a json byte) → hash mismatch → None.
        let last = framed.len() - 1;
        framed[last] ^= 0xff;
        assert_eq!(unframe_snapshot(MAGIC, &framed), None);
    }

    #[test]
    fn orphan_temp_sweep_reclaims_stale_keeps_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snap.json");

        // A crashed run's leftover temp + a non-matching sibling + a "live"
        // concurrent temp (created fresh now).
        let orphan = dir.path().join("snap.json.tmp.4242");
        let unrelated = dir.path().join("snap.json.bak");
        let live = dir.path().join("snap.json.tmp.9999");
        std::fs::write(&orphan, b"x").unwrap();
        std::fs::write(&unrelated, b"x").unwrap();
        std::fs::write(&live, b"x").unwrap();

        // With max_age 0, every matching temp counts as stale → orphan + live
        // both reclaimed; the unrelated sibling is untouched.
        sweep_orphan_temps_with(&path, std::time::SystemTime::now(), 0);
        assert!(!orphan.exists(), "a pid-tagged temp is reclaimed");
        assert!(!live.exists(), "max_age 0 reclaims even a fresh temp");
        assert!(unrelated.exists(), "a non-temp sibling is never touched");

        // Safety direction: a fresh temp under the real 5-min floor SURVIVES,
        // so a concurrent process's in-flight write is never deleted.
        std::fs::write(&orphan, b"x").unwrap();
        sweep_orphan_temps_with(&path, std::time::SystemTime::now(), 300);
        assert!(orphan.exists(), "a fresh temp is kept under the staleness floor");
    }

    #[test]
    fn atomic_write_framed_lands_a_loadable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("snap.json");
        atomic_write_framed(MAGIC, &path, b"{\"ok\":true}");
        let bytes = std::fs::read(&path).expect("file written through the temp+rename");
        assert_eq!(
            unframe_snapshot(MAGIC, &bytes).as_deref(),
            Some(&b"{\"ok\":true}"[..])
        );
    }
}
