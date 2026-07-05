//! The catalog-erased, row-lenient snapshot reader — for CROSS-PROCESS
//! consumers that do not know the producer's catalog type.
//!
//! A typed [`Store`](crate::Store) load requires the exact `K: Catalog` the
//! snapshot was written with (an unknown slug fails the whole deserialize —
//! correct for the producer, useless for a foreign reader). This module reads
//! the SAME framed snapshot with `serde_json::Value` walking instead:
//!
//! * kinds stay slugs (`source: String`) — UNKNOWN SLUGS ARE KEPT, so a
//!   reader built against an older catalog still sees every row;
//! * the payload stays an opaque [`serde_json::Value`];
//! * a malformed ROW is dropped, never rejects the whole snapshot
//!   (row-lenient: one corrupt row must not blind the reader to the rest).

use std::path::Path;

use serde_json::Value;

/// One catalog-erased snapshot row — the lenient projection of a
/// [`StoredItem`](crate::StoredItem) as written on the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct RawStoredItem {
    /// The item id as a decimal `u64` string (the wire form is a bare number;
    /// the string form survives readers whose number type would truncate).
    pub id: String,
    /// The source slug — unknown slugs are KEPT verbatim.
    pub source: String,
    pub title: String,
    pub detail: Option<String>,
    /// The urgency slug (kebab-case wire form, e.g. `normal`).
    pub urgency: String,
    pub score: u32,
    /// The lifecycle state's `kind` tag (kebab-case, e.g. `offered`).
    pub state_kind: String,
    pub times_seen: u32,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// The opaque action payload (the wire field `spawn`), untyped.
    pub payload: Value,
}

/// The catalog-erased snapshot: every parseable row + the save stamp.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawSnapshot {
    pub entries: Vec<RawStoredItem>,
    pub saved_at_ms: u64,
}

/// Parse a snapshot's JSON body leniently. `None` only when the body isn't a
/// JSON object with an `entries` array at all; a malformed ROW inside is
/// dropped, never rejects the whole snapshot.
#[must_use]
pub fn parse_raw_snapshot(json: &[u8]) -> Option<RawSnapshot> {
    let v: Value = serde_json::from_slice(json).ok()?;
    let obj = v.as_object()?;
    let saved_at_ms = obj.get("saved_at_ms").and_then(Value::as_u64).unwrap_or(0);
    let rows = obj.get("entries").and_then(Value::as_array)?;
    let entries = rows.iter().filter_map(parse_raw_row).collect();
    Some(RawSnapshot {
        entries,
        saved_at_ms,
    })
}

/// One row, leniently: id + source + title are load-bearing (a row without an
/// identity is meaningless — dropped); everything else defaults like the
/// typed codec would.
fn parse_raw_row(row: &Value) -> Option<RawStoredItem> {
    let row = row.as_object()?;
    let item = row.get("suggestion")?.as_object()?;
    let id = item.get("id")?.as_u64()?;
    let source = item.get("source")?.as_str()?.to_owned();
    let title = item.get("title")?.as_str()?.to_owned();
    let detail = item.get("detail").and_then(Value::as_str).map(str::to_owned);
    let urgency = item
        .get("urgency")
        .and_then(Value::as_str)
        .unwrap_or("normal")
        .to_owned();
    let score = item
        .get("score")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(500);
    let payload = item.get("spawn").cloned().unwrap_or(Value::Null);
    let state_kind = row
        .get("state")
        .and_then(Value::as_object)
        .and_then(|s| s.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("offered")
        .to_owned();
    let times_seen = row
        .get("times_seen")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(1);
    let first_seen_ms = row.get("first_seen_ms").and_then(Value::as_u64).unwrap_or(0);
    let last_seen_ms = row.get("last_seen_ms").and_then(Value::as_u64).unwrap_or(0);
    Some(RawStoredItem {
        id: id.to_string(),
        source,
        title,
        detail,
        urgency,
        score,
        state_kind,
        times_seen,
        first_seen_ms,
        last_seen_ms,
        payload,
    })
}

/// Read + unframe (see [`crate::persist::unframe_snapshot`]) + leniently
/// parse a snapshot file. `None` on a missing file, wrong magic, torn body,
/// or a non-snapshot JSON shape.
#[must_use]
pub fn read_raw_snapshot_file(path: &Path, magic: &[u8]) -> Option<RawSnapshot> {
    let bytes = std::fs::read(path).ok()?;
    let json = crate::persist::unframe_snapshot(magic, &bytes)?;
    parse_raw_snapshot(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built v1-wire snapshot body: one known row, one row from a slug
    /// this reader has never heard of, one malformed row (no title).
    const BODY: &str = r#"{
        "saved_at_ms": 777,
        "entries": [
            {
                "suggestion": {
                    "id": 42,
                    "source": "tend-repos",
                    "title": "mado",
                    "detail": "dirty",
                    "urgency": "low",
                    "spawn": {"cwd": "/code/mado", "name": "mado", "initial_command": null},
                    "score": 500
                },
                "first_seen_ms": 100,
                "last_seen_ms": 200,
                "times_seen": 3,
                "state": {"kind": "accepted", "session": "s"}
            },
            {
                "suggestion": {
                    "id": 43,
                    "source": "some-future-source",
                    "title": "unknown slug survives",
                    "urgency": "critical",
                    "spawn": {"cwd": "/x", "name": "n"},
                    "score": 900
                },
                "first_seen_ms": 1,
                "last_seen_ms": 2,
                "state": {"kind": "offered"}
            },
            {
                "suggestion": {"id": 44, "source": "tend-repos"}
            }
        ]
    }"#;

    #[test]
    fn parse_is_row_lenient_and_keeps_unknown_slugs() {
        let snap = parse_raw_snapshot(BODY.as_bytes()).expect("snapshot parses");
        assert_eq!(snap.saved_at_ms, 777);
        // The malformed row (no title) is DROPPED; the other two survive —
        // including the unknown slug.
        assert_eq!(snap.entries.len(), 2);
        let known = &snap.entries[0];
        assert_eq!(known.id, "42");
        assert_eq!(known.source, "tend-repos");
        assert_eq!(known.title, "mado");
        assert_eq!(known.detail.as_deref(), Some("dirty"));
        assert_eq!(known.urgency, "low");
        assert_eq!(known.score, 500);
        assert_eq!(known.state_kind, "accepted");
        assert_eq!(known.times_seen, 3);
        assert_eq!(known.first_seen_ms, 100);
        assert_eq!(known.last_seen_ms, 200);
        assert_eq!(known.payload["cwd"], "/code/mado");
        let alien = &snap.entries[1];
        assert_eq!(alien.source, "some-future-source", "unknown slug kept");
        assert_eq!(alien.state_kind, "offered");
        assert_eq!(alien.times_seen, 1, "missing times_seen defaults like the typed codec");
    }

    #[test]
    fn parse_rejects_only_a_non_snapshot_shape() {
        assert_eq!(parse_raw_snapshot(b"not json"), None);
        assert_eq!(parse_raw_snapshot(b"[1,2,3]"), None);
        assert_eq!(parse_raw_snapshot(br#"{"no_entries": true}"#), None);
        // An empty entries array is a valid (empty) snapshot.
        let empty = parse_raw_snapshot(br#"{"entries": []}"#).unwrap();
        assert!(empty.entries.is_empty());
        assert_eq!(empty.saved_at_ms, 0);
    }

    #[test]
    fn read_raw_snapshot_file_unframes_then_parses() {
        const MAGIC: &[u8] = b"izumi-raw-test v1\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snap.json");
        crate::persist::atomic_write_framed(MAGIC, &path, BODY.as_bytes());
        let snap = read_raw_snapshot_file(&path, MAGIC).expect("framed file reads");
        assert_eq!(snap.entries.len(), 2);
        // Wrong magic → None (the frame gate, not the lenient row gate).
        assert_eq!(read_raw_snapshot_file(&path, b"wrong\n"), None);
        // Missing file → None.
        assert_eq!(read_raw_snapshot_file(&dir.path().join("nope"), MAGIC), None);
    }
}
