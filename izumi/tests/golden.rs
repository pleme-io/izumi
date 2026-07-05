//! Golden wire-compat gate: a REAL mado `suggest v1` snapshot fixture loads
//! through the generic [`izumi::Store`] byte-for-byte — proving the
//! generalization kept the v1 codec (field names, slug wire form, framed
//! magic, `ItemId` numbers) intact.
//!
//! The fixtures under `tests/fixtures/` are byte-frozen from mado's
//! pre-extraction build (see the fixtures README for provenance); a missing
//! fixture is a hard failure — these assertions must always run.

use std::path::PathBuf;

use izumi::{Catalog as _, CorrKey, Item, ItemId, ItemState, SpawnSpec, Store, Urgency};

izumi::catalog! {
    /// The four mado kinds the golden fixture covers — table values match the
    /// mado `SourceKind` catalog rows exactly.
    pub enum GoldenKind {
        TendRepos { slug: "tend-repos", emoji: "\u{1F9F9}", label: "tend dirty repos", urgency: Low, needs_auth: false, interval_secs: 30 },
        JiraSprint { slug: "jira-sprint", emoji: "\u{1F3AB}", label: "Jira sprint", urgency: Normal, needs_auth: true, interval_secs: 300 },
        GithubReviewRequested { slug: "github-review-requested", emoji: "\u{1F50D}", label: "GitHub review-requested", urgency: High, needs_auth: true, interval_secs: 180 },
        GrafanaAlerts { slug: "grafana-alerts", emoji: "\u{1F525}", label: "grafana alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
    }
}

/// The EXACT mado v1 snapshot magic (includes its trailing newline).
const MADO_MAGIC: &[u8] = b"mado-suggest v1\n";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mado-suggest-v1.snapshot")
}

#[test]
fn golden_mado_snapshot_loads_typed_and_raw() {
    let path = fixture_path();
    assert!(
        path.exists(),
        "golden fixture {} is missing — it is byte-frozen wire-compat evidence and must be present",
        path.display()
    );

    // Typed load through the generic store with the mado magic. now_ms = 0 <
    // saved_at_ms → no rebase, so every on-disk ms value survives verbatim.
    let store: Store<GoldenKind, SpawnSpec> = Store::new();
    store.load_file(&path, MADO_MAGIC, 0);
    let snap = store.to_snapshot(0);
    assert!(
        !snap.entries.is_empty(),
        "the golden snapshot loads a non-empty entry set"
    );

    // One row of each ItemState variant round-trips through the v1 codec.
    let has = |name: &str, pred: &dyn Fn(&ItemState) -> bool| {
        assert!(
            snap.entries.iter().any(|e| pred(&e.state)),
            "fixture carries a {name} row"
        );
    };
    has("Offered", &|s| matches!(s, ItemState::Offered));
    has("Accepted", &|s| matches!(s, ItemState::Accepted { .. }));
    has("Snoozed", &|s| matches!(s, ItemState::Snoozed { .. }));
    has("Dismissed", &|s| matches!(s, ItemState::Dismissed));

    // Ids are preserved: the raw (catalog-erased) reader on the SAME file
    // sees the same row count and the same id set — the typed load dropped
    // and renumbered nothing.
    let raw = izumi::raw::read_raw_snapshot_file(&path, MADO_MAGIC)
        .expect("raw reader unframes + parses the same fixture");
    assert_eq!(
        raw.entries.len(),
        snap.entries.len(),
        "typed and raw loads agree on the row count"
    );
    let mut typed_ids: Vec<String> = snap.entries.iter().map(|e| e.item.id.0.to_string()).collect();
    let mut raw_ids: Vec<String> = raw.entries.iter().map(|e| e.id.clone()).collect();
    typed_ids.sort_unstable();
    raw_ids.sort_unstable();
    assert_eq!(typed_ids, raw_ids, "every id is preserved bit-for-bit");

    // Every row's slug resolves in the golden catalog (the fixture covers
    // exactly the four declared kinds) and matches its typed twin.
    for e in &raw.entries {
        assert!(
            GoldenKind::from_slug(&e.source).is_some(),
            "fixture slug {} is in the golden catalog",
            e.source
        );
    }
}

#[test]
fn golden_mado_suggestion_json_deserializes_full_field_surface() {
    // `suggestion.json` is the pretty-printed single-`Suggestion` fixture (the
    // jira row as constructed, pre-ingest) — it exercises the FULL item field
    // surface: `id` (u64 number), `source` (kebab-case slug), `title`,
    // `detail`, `urgency` (kebab-case), `spawn` (validated `SpawnSpecWire`
    // border), `score`, `corr` (transparent string).
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("suggestion.json");
    assert!(
        path.exists(),
        "golden fixture {} is missing — it is byte-frozen wire-compat evidence and must be present",
        path.display()
    );

    let bytes = std::fs::read(&path).expect("golden suggestion.json is readable");
    let item: Item<GoldenKind, SpawnSpec> =
        serde_json::from_slice(&bytes).expect("a conforming Item deserializer loads the fixture");

    assert_eq!(item.source, GoldenKind::JiraSprint);
    assert_eq!(item.title, "ASM-1234 fix the parser");
    assert_eq!(item.detail.as_deref(), Some("sprint 42 · in progress"));
    assert_eq!(item.urgency, Urgency::Normal);
    assert_eq!(item.spawn.cwd(), std::path::Path::new("/code/asm"));
    assert_eq!(item.spawn.name(), "asm-1234");
    assert_eq!(item.spawn.initial_command(), None);
    assert_eq!(item.score, 500);
    assert_eq!(item.corr, CorrKey::jira("ASM-1234"));

    // The persisted id is EXACTLY the fnv1a derivation over `slug ':' key` —
    // identity is preserved bit-for-bit across the extraction.
    assert_eq!(item.id, ItemId::derive_slug("jira-sprint", "ASM-1234"));
    assert_eq!(item.id, ItemId::derive(GoldenKind::JiraSprint, "ASM-1234"));

    // Round-trip: re-serializing keeps every v1 field name (in particular the
    // payload's legacy wire name `spawn`).
    let round: serde_json::Value = serde_json::to_value(&item).expect("item re-serializes");
    let orig: serde_json::Value =
        serde_json::from_slice(&bytes).expect("fixture parses as JSON");
    assert_eq!(round, orig, "v1 codec round-trips the fixture value-identically");
}
