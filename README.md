# izumi (泉)

**The fresh-source board substrate** — continuously-refreshing, ranked,
cache-fresh, *actionable* data sources for Rust applications.

izumi is the generalized extraction of [mado](https://github.com/pleme-io/mado)'s
Ctrl-S suggestion stream: the living board that continuously proposes "what
you could start working on right now" from dozens of external sources — PRs
awaiting review, sprint tickets, firing alerts, failing CI, dirty repos —
each row ranked by urgency and always one keypress from action.

```
sources (poll through a mockable Environment)
   │  PollOutcome::{Fetched, Unavailable}   ← the honesty border: a blip
   ▼                                          never masquerades as "resolved"
living-board Store
   │  recurrence tombstones · bounded aging escalation · lifecycle
   │  (Offered → Accepted ◐ / Snoozed / Dismissed) · per-source health
   ▼
ranked, deduplicated, always-actionable rows
```

## Crates

| Crate | Purpose |
|---|---|
| [`izumi`](./izumi) | The core algebra: `Item<K, A>`, the `Catalog` trait + `catalog!{}` macro (typed source catalogs with **compile-time slug uniqueness**), `Source`/`PollOutcome`, the living-board `Store`, the freshness-nudged watcher `Engine`, the typed `Environment` seam (argv `Cmd` + `HttpReq` — no shell, ever), per-host pacing (`samba`), BLAKE3-framed atomic snapshots, a K-erased `raw` reader |
| [`izumi-sources`](./izumi-sources) | 25 generic providers (GitHub, Jira, Confluence, Grafana, Datadog, Opsgenie, Flux, k8s, AWS, Cloudflare, Google, tend, cargo, …), each generic over your catalog — verification-matrix-tested |
| [`izumi-config`](./izumi-config) | The [shikumi](https://github.com/pleme-io/shikumi) `TieredConfig` board surface (`BoardConfig`, `SourceEntry`, the `effective_sources` prescribed⊕override merge) |
| [`izumi-lisp`](./izumi-lisp) | tatara-lisp authoring: `(defizumiboard …)` + `(defizumisource …)` |
| [`izumi-board`](./izumi-board) | A headless board daemon/CLI: run the engine on any machine, query/act over a unix socket |

## Defining your own board

```rust
izumi::catalog! {
    /// Every source my app watches.
    pub enum MyKind {
        JiraAssigned { slug: "jira-assigned", emoji: "📋", label: "Jira assigned",
                       urgency: Normal, needs_auth: true, interval_secs: 300 },
        FluxFailing  { slug: "flux-failing", emoji: "🔁", label: "Flux failing",
                       urgency: High, needs_auth: false, interval_secs: 60 },
    }
}

let store = std::sync::Arc::new(izumi::Store::<MyKind, izumi::SpawnSpec>::new());
let env: std::sync::Arc<dyn izumi::Environment> =
    std::sync::Arc::new(izumi::RealEnvironment::discover());
let sources: Vec<std::sync::Arc<dyn izumi::Source<MyKind, izumi::SpawnSpec>>> = vec![
    std::sync::Arc::new(izumi_sources::JiraAssigned::new(MyKind::JiraAssigned)),
    std::sync::Arc::new(izumi_sources::FluxFailing::new(MyKind::FluxFailing)),
];
let engine = izumi::Engine::start(sources, env, store.clone(), cfg, None);
// … read store.ranked_stored(max, now_ms) whenever you render.
```

- `K` is *your* closed catalog — the `catalog!{}` table generates the enum,
  every accessor, serde-via-slug, `Display`, a compile-time duplicate-slug
  check, and completeness tests. Adding a variant without its table row is a
  compile error, not a runtime surprise.
- `A` is your action payload. `SpawnSpec` (cwd + session name + validated
  kickoff command; control-byte injection is unconstructible) ships as the
  reference payload; any `Clone + Serialize + Deserialize` type works.
- Every source polls through `&dyn Environment` — tests inject
  `MockEnvironment` fixtures; nothing touches the network, a subprocess, or
  a cluster in a unit test.

## Invariants worth knowing

- **Honest polling.** `Fetched` means the upstream *was observed* (an empty
  set = genuine resolution). Unreachable/unauthed/unconfigured are typed
  `Unavailable` tiers — the board keeps last-known rows and shows health,
  never silently blanks.
- **Identity continuity.** Item ids are content-addressed from
  `(slug, key)`; recurrence tombstones give a re-firing issue its original
  birth time and a `×N` count; dismissals are sticky across re-ingest.
- **Urgency dominates.** Ranking is urgency-tier first; aging (+2/min,
  capped) and recurrence (+40/sighting, capped) escalate *within* a tier only.
- **Pacing is structural.** One leaky bucket per upstream host, 429/403
  cooldowns, and a freshness nudge that can never storm an API.

## Consumers

- **mado** — the Ctrl-S session picker (consumer #1; snapshots and YAML are
  wire-compatible by golden-fixture proof).
- **izumi-board** — headless boards on servers (consumer #2).

Non-goals: tend's cache gates, formigueiro's update signals, and praca's
frecency session ranking are *cousins, not duplicates* — see
[CLAUDE.md](./CLAUDE.md) for the deliberate non-unification note.

## License

MIT
