# Golden wire fixtures — mado suggest plane

**BYTE-FROZEN wire-compat proofs. These files MUST always load.** They are the
canonical on-disk artifacts of the mado suggestion plane's v1 wire format; any
izumi change that fails to parse them byte-for-byte is a wire-format break, not
a fixture problem. Never regenerate them casually — a regeneration is a
deliberate format-version event.

## Provenance

- **Produced from:** `pleme-io/mado` at commit
  `8dc28fcdfd8e66594cc1cff45f0ea48868e990ee` (mado v0.1.65, edition 2024).
- **Date:** 2026-07-05.
- **Mechanism:** a TEMPORARY `#[cfg(test)]` test `golden_fixture_dump`
  appended to `mado/src/suggest/store.rs`, run via
  `cargo test -p mado golden_fixture_dump`, then reverted
  (`git checkout -- src/suggest/store.rs`; mado left byte-clean, nothing
  committed). All timestamps are FIXED so the output is deterministic.

## Test body summary

The temporary test:

1. Built a `SuggestionStore` and, at a fixed `now_ms = 1_000_000`, ingested
   four representative suggestions (one per source, each via
   `store.ingest(source, vec![…], 1_000_000)`):
   - **`jira-sprint`** — `Suggestion::new(JiraSprint, "ASM-1234",
     "ASM-1234 fix the parser", SpawnSpec::new("/code/asm", "asm-1234"))`
     `.detail("sprint 42 · in progress")`
     `.correlated(CorrKey::jira("ASM-1234"))` `.urgent(Urgency::Normal)`
     → then `mark_accepted(id, "work")` ⇒ state `accepted { session: "work" }`.
   - **`grafana-alerts`** — key `HighCPU`, title `HighCPU firing`, spawn
     `SpawnSpec::new("/code/ops", "highcpu")`, `.urgent(Urgency::Critical)`
     → then `snooze(id, 9_999_999)` ⇒ state `snoozed { until_ms: 9999999 }`.
   - **`github-review-requested`** — key `pleme-io/mado#1`, title
     `pr#1 review requested`, spawn
     `SpawnSpec::new("/code/github/pleme-io/mado", "mado-pr-1")
     .with_command("gh pr checkout 1")` → then `dismiss(id)` ⇒ state
     `dismissed`.
   - **`tend-repos`** — key `/code/github/pleme-io/izumi`, title
     `izumi dirty`, spawn
     `SpawnSpec::new("/code/github/pleme-io/izumi", "izumi")` → left
     untouched ⇒ state `offered`.
2. Called `store.persist_file(Path::new("/tmp/izumi-golden/mado-suggest-v1.snapshot"), 1_000_000)`
   — mado's crash-safe framed persist (compact `serde_json::to_vec` of the
   `StoreSnapshot`, BLAKE3-framed, atomic temp+rename).
3. Wrote `serde_json::to_vec_pretty` of the single jira `Suggestion` (as
   built, pre-ingest, state-free) to `/tmp/izumi-golden/suggestion.json`.

Both files were then copied here verbatim.

## `mado-suggest-v1.snapshot` (1479 bytes)

The framed warm-restart snapshot, exactly as `SuggestionStore::persist_file`
writes it:

```
"mado-suggest v1\n"            ← 16-byte schema magic (SNAPSHOT_MAGIC)
<64 lowercase hex chars>"\n"   ← BLAKE3 hash of the JSON body
<compact JSON>                 ← serde_json::to_vec(&StoreSnapshot)
```

Body hash of this fixture (65-byte header, 1398-byte body):
`ad695a1aff7e0cf32d3f45a8d4f2430872cbdb00696d34e55622790726dd6013`.

The JSON body is `StoreSnapshot { entries: Vec<StoredSuggestion>,
saved_at_ms: 1000000 }`; entries are `BTreeMap`-ordered by `SuggestionId`
(FNV-1a of `"<source-slug>:<key>"`), so the byte layout is deterministic —
in this fixture the order is `grafana-alerts`, `github-review-requested`,
`jira-sprint`, `tend-repos`. Row coverage: 4 rows × 4 lifecycle states
(`snoozed { until_ms: 9999999 }` / `dismissed` / `accepted { session:
"work" }` / `offered`, kebab-case internally-tagged on `kind`), each with
`first_seen_ms = last_seen_ms = 1000000`, `times_seen = 1`. The jira row
carries `detail` + `corr` (`"jira:ASM-1234"`); the github row carries
`spawn.initial_command` (`"gh pr checkout 1"`); the grafana row carries
`urgency: "critical"`.

A conforming loader must: verify the magic, verify the BLAKE3 hex against the
body, parse the body, and recover all 4 rows with their exact states — and
must treat wrong-magic / hash-mismatch as start-empty (never an error crash,
never garbage rows).

## `suggestion.json` (296 bytes)

`serde_json::to_vec_pretty` of one `Suggestion` (the jira row as constructed,
before ingest — no store bookkeeping). Exercises the full field surface:
`id` (u64), `source` (kebab-case slug), `title`, `detail`, `urgency`
(kebab-case), `spawn` (`cwd`/`name`/`initial_command: null`), `score`, `corr`
(transparent string `"jira:ASM-1234"`). A conforming `Suggestion` deserializer
must load it; note `spawn` deserializes through the validated
`SpawnSpecWire` `try_from` border (non-empty cwd/name).
