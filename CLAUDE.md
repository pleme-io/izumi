skip-urdume: not-a-service — pure library workspace + a thin headless CLI/daemon (izumi-board); no data spine, no API transports.
skip-breathe: build-time library — no runtime workload to band; izumi-board consumers declare their own bands if clusterized.
skip-shigoto: continuous-watcher-runtime-not-a-work-DAG — the Engine is an unbounded per-source watcher plane (one tokio task per source, no completion), not a dependency-ordered finite work graph.
skip-tela: not-a-frontend.
skip-emitter-substrate: enum-generation-requires-decl-macro — catalog!{} must generate the enum itself from one table, which no proc-macro derive can do; the per-variant string-table impls it also emits (slug/label/emoji/urgency/auth/interval + Display) are tracked as macro-farm backlog (`pleme-catalog-derive` via PerVariantDeriveSpec) so catalog!{} can shrink to sugar over the derive.
skip-typed-spec-triplet: config-surface-only-in-v0.1 — the Environment trait + MockEnvironment IS the triplet's testability seam and izumi-lisp ships the config authoring forms; the snapshot wire-format spec ((defizumisnapshot …)) and the store lifecycle FSM spec are named M-next, not shipped.

# izumi (泉 — spring) — the fresh-source board substrate

Continuously-refreshing, ranked, cache-fresh, **actionable** data sources —
the generalized extraction of mado's Ctrl-S suggestion plane. One typed
algebra: sources poll external state through a mockable `Environment`,
honest `PollOutcome`s feed a living-board `Store` (recurrence tombstones,
bounded aging escalation, lifecycle soft-ack, per-source health), a
freshness-nudged watcher `Engine` keeps everything current, and every item
carries a valid-by-construction action payload.

## Workspace

| Crate | What it is |
|---|---|
| `izumi` | Core algebra: `Item<K, A>`, `Catalog` trait + `catalog!{}` (compile-time slug uniqueness), `Source`/`PollOutcome`, `Store` living-board, `Engine` + freshness nudge, `Environment` seam (typed Cmd/HTTP argv — NO SHELL), `HostPacer` (samba), BLAKE3-framed snapshot `persist` (magic-parameterized), K-erased `raw` reader, `writer` flock election, `maintain` loop |
| `izumi-sources` | Generic provider catalog (github/jira/grafana/k8s/flux/tend/cargo/…), each generic over K (kind at construction), payload = `SpawnSpec`; verification-matrix-tested |
| `izumi-config` | shikumi `TieredConfig` board surface — `BoardConfig` incl `sources_replace` + `per_source_cap` + the `effective_sources()` merge |
| `izumi-lisp` | tatara-lisp authoring: `(defizumiboard …)` + `(defizumisource …)` |
| `izumi-board` | Headless board daemon/CLI (consumer #2): shikumi config, engine over izumi-sources, unix-socket lifecycle ingress (list/dismiss/snooze/accept), snapshot persist |

## Consumers + non-goals

- **Consumer #1: mado** (Ctrl-S picker) — byte-compatible snapshots (magic
  `b"mado-suggest v1\n"` passed by mado) + YAML config semantics preserved.
- **Consumer #2: izumi-board** (headless).
- **Deliberately NOT unified** (cousins, not duplicates — three-site rule):
  tend's `CacheFreshGate` (shigoto gate), formigueiro's `SignalSource` →
  promotion-FSM mutation plans, praca's frecency+fuzzy session ranking.
  The **shared shape that IS at third site** is the `Environment`/`Cmd`/
  `HttpReq`/pacer seam (mado/izumi ⊕ formigueiro `UpdateEnv`+`Pacer` ⊕
  sentinela `GitopsEnv`) — adopting `izumi::env` in those repos is the named
  extraction trigger.
- praca adapter is TWO gates away (tear-side `SessionOrigin::Suggested` +
  a cwd/env/args-bearing backend spawn op) — see mado
  `docs/SUGGESTION-STREAM.md`.

## Wire-compat contracts (do not break)

- `StoredItem` serializes its item under the field name `"suggestion"`
  (serde rename); `ItemState` keeps `tag = "kind"`, kebab-case, variant
  fields `Accepted{session}` / `Snoozed{until_ms}`; `ItemId` is a
  transparent `u64` newtype (JSON number); `StoreSnapshot` keeps
  `entries`/`saved_at_ms`. Golden fixtures under `izumi/tests/fixtures/`
  are byte-frozen from mado's pre-extraction build — they must always load.
- `Item<K, A>` serializes its payload under the wire name `"spawn"` for ANY
  payload (v1-codec legacy field, forced by mado snapshot continuity).
- The snapshot frame is `MAGIC || blake3-hex || '\n' || json`; magic is a
  caller parameter INCLUDING any trailing newline.

## Double-polling hazard

`HostPacer` is per-process. Arming izumi-board beside mado on a
workstation doubles upstream QPS against github/atlassian/grafana. The nix
module defaults to **disabled**; workstation profiles must not arm it
while mado's suggestion engine runs with overlapping sources.
