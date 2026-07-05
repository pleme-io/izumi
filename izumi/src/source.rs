//! The source provider plane.
//!
//! A [`Source`] turns external state (read through the
//! [`Environment`]) into a set of [`Item`]s. Every
//! source is pure w.r.t. the environment, so [`refresh_once`] — `poll` →
//! `store.ingest` — is the single unit each source is tested through (with a
//! [`MockEnvironment`](crate::env::MockEnvironment)).
//!
//! The parallel watcher engine that drives sources on their cadences lives in
//! [`crate::engine`].

use std::collections::BTreeMap;
use std::time::Duration;

use crate::catalog::Catalog;
use crate::env::Environment;
use crate::item::{Item, SourceStatus};
use crate::payload::Payload;
use crate::store::Store;

/// Per-source runtime config — the typed knobs a consumer's config block
/// resolves into. `params` is an open per-kind map (token env override,
/// jira JQL, grafana folder, …); typed-per-kind config is a later refinement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub max_items: usize,
    pub params: BTreeMap<String, String>,
}

impl SourceConfig {
    /// Sensible defaults derived from the source kind.
    #[must_use]
    pub fn for_kind<K: Catalog>(kind: K) -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(kind.default_interval_secs()),
            max_items: 5,
            params: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }
}

/// The typed outcome of one poll — the honest border between a provider and
/// the store. `Fetched` means the upstream WAS observed and the carried set
/// is the complete current truth for this source (empty = genuinely nothing —
/// resolved items decay off). `Unavailable` means the upstream COULD NOT be
/// observed (missing param, missing credential, network/tool failure): the
/// store keeps the source's last-known rows (TTL ages them out) and records
/// the health state, so an infrastructure blip never wipes the board or
/// masquerades as "everything resolved".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollOutcome<K, A> {
    Fetched(Vec<Item<K, A>>),
    Unavailable(SourceStatus),
}

impl<K, A> PollOutcome<K, A> {
    /// The source needs a param (site/`base_url`/…) the config doesn't supply.
    #[must_use]
    pub const fn unconfigured() -> Self {
        Self::Unavailable(SourceStatus::Unconfigured)
    }
    /// The source's credential/secret is absent.
    #[must_use]
    pub const fn auth_missing() -> Self {
        Self::Unavailable(SourceStatus::AuthMissing)
    }
    /// The fetch/parse failed (network, timeout, tool exit, bad shape).
    #[must_use]
    pub const fn error() -> Self {
        Self::Unavailable(SourceStatus::Error)
    }
}

/// A typed item provider. One impl per catalog kind.
pub trait Source<K: Catalog, A: Payload>: Send + Sync {
    /// Which source this is.
    fn kind(&self) -> K;

    /// Produce the current item set by reading external state through `env`.
    /// MUST be best-effort + pure w.r.t. `env` and MUST NOT panic.
    /// Honesty contract: return [`PollOutcome::Fetched`] ONLY when the
    /// upstream was actually observed (an observed-empty set is `Fetched` of
    /// an empty `Vec`); a missing param / credential / tool / failed fetch is
    /// the matching [`PollOutcome::Unavailable`] tier. `cfg` carries the
    /// per-source knobs (`max_items`, params).
    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, A>;
}

/// Apply one poll's outcome to the store — the shared sink the engine's
/// watcher tasks and [`refresh_once`] both flow through. A `Fetched` set is
/// rank-sorted BEFORE the `max_items` truncation (so a provider emitting its
/// worst item last never loses its best), then ingested; an `Unavailable`
/// records health and leaves the last-known rows in place.
///
/// Dismissed rows are exempt from the `max_items` budget: they are invisible
/// on the board, so a dismissed top-ranked issue must not crowd offerable
/// rows out of the store — but they still ride along in the ingest so their
/// Dismissed state persists on the live entry (stickiness).
pub fn apply_poll<K: Catalog, A: Payload>(
    kind: K,
    outcome: PollOutcome<K, A>,
    store: &Store<K, A>,
    cfg: &SourceConfig,
    now_ms: u64,
) {
    match outcome {
        PollOutcome::Fetched(items) => {
            let (dismissed, mut live): (Vec<_>, Vec<_>) =
                items.into_iter().partition(|s| store.is_dismissed(s.id));
            live.sort_by_key(|s| std::cmp::Reverse(s.rank_key()));
            live.truncate(cfg.max_items);
            live.extend(dismissed);
            store.ingest(kind, live, now_ms);
            store.record_poll(kind, SourceStatus::Ok, now_ms);
        }
        PollOutcome::Unavailable(status) => {
            store.record_poll(kind, status, now_ms);
        }
    }
}

/// The single tested unit: poll a source through the environment and apply
/// the outcome to the store. Pure given `(source, env, store, cfg, now_ms)`.
pub fn refresh_once<K: Catalog, A: Payload>(
    source: &dyn Source<K, A>,
    env: &dyn Environment,
    store: &Store<K, A>,
    cfg: &SourceConfig,
    now_ms: u64,
) {
    let outcome = source.poll(env, cfg);
    apply_poll(source.kind(), outcome, store, cfg, now_ms);
}

/// Engine-wide config: per-source overrides + global decay/visibility knobs.
#[derive(Clone, Debug)]
pub struct EngineConfig<K> {
    pub per_source: BTreeMap<K, SourceConfig>,
    /// Whether a source with no explicit override runs by default.
    pub default_enabled: bool,
}

impl<K> Default for EngineConfig<K> {
    fn default() -> Self {
        Self {
            per_source: BTreeMap::new(),
            default_enabled: true,
        }
    }
}

impl<K: Catalog> EngineConfig<K> {
    /// The effective config for a kind: an explicit override wins, else the
    /// kind's defaults with `enabled = default_enabled`.
    #[must_use]
    pub fn config_for(&self, kind: K) -> SourceConfig {
        self.per_source.get(&kind).cloned().unwrap_or_else(|| {
            let mut c = SourceConfig::for_kind(kind);
            c.enabled = self.default_enabled;
            c
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MockEnvironment;
    use crate::spawn::SpawnSpec;
    use crate::store::Store;
    use crate::testkit::TestKind;

    /// A source that echoes one item per line of a fixture command, honoring
    /// the honesty contract (a failed run = `Unavailable`).
    struct FixtureSource;
    impl Source<TestKind, SpawnSpec> for FixtureSource {
        fn kind(&self) -> TestKind {
            TestKind::TendRepos
        }
        fn poll(
            &self,
            env: &dyn Environment,
            _cfg: &SourceConfig,
        ) -> PollOutcome<TestKind, SpawnSpec> {
            let Some(out) = env.run(&crate::env::Cmd::new("tend").arg("status")) else {
                return PollOutcome::error();
            };
            PollOutcome::Fetched(
                out.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|repo| {
                        Item::new(
                            TestKind::TendRepos,
                            repo,
                            repo,
                            SpawnSpec::new("/code", repo).unwrap(),
                        )
                    })
                    .collect(),
            )
        }
    }

    #[test]
    fn refresh_once_polls_and_ingests() {
        let env = MockEnvironment::new().cmd("tend status", "mado\ntear\n");
        let store = Store::new();
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        refresh_once(&FixtureSource, &env, &store, &cfg, 1000);
        assert_eq!(store.len(), 2);
        assert!(store.ranked(10, 1000).iter().any(|s| s.title == "mado"));
        let health = store.health();
        assert!(
            health
                .iter()
                .any(|(k, h)| *k == TestKind::TendRepos
                    && h.status == crate::item::SourceStatus::Ok),
            "an observed poll records Ok health"
        );
    }

    #[test]
    fn refresh_once_truncates_to_max_items() {
        let env = MockEnvironment::new().cmd("tend status", "a\nb\nc\nd\ne\n");
        let store = Store::new();
        let mut cfg = SourceConfig::for_kind(TestKind::TendRepos);
        cfg.max_items = 2;
        refresh_once(&FixtureSource, &env, &store, &cfg, 1000);
        assert_eq!(store.len(), 2, "capped to max_items");
    }

    #[test]
    fn unavailable_keeps_last_known_rows_and_records_health() {
        // First poll observes two repos; the second poll's upstream is gone
        // (no cmd fixture → run returns None → Unavailable). The board keeps
        // the last-known rows instead of flickering empty, and health flips.
        let store = Store::new();
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let ok_env = MockEnvironment::new().cmd("tend status", "mado\ntear\n");
        refresh_once(&FixtureSource, &ok_env, &store, &cfg, 1000);
        assert_eq!(store.len(), 2);

        let dead_env = MockEnvironment::new();
        refresh_once(&FixtureSource, &dead_env, &store, &cfg, 2000);
        assert_eq!(store.len(), 2, "a fetch failure never wipes the board");
        let health = store.health();
        let (_, h) = health
            .iter()
            .find(|(k, _)| *k == TestKind::TendRepos)
            .expect("health recorded");
        assert_eq!(h.status, crate::item::SourceStatus::Error);
        assert_eq!(h.last_ok_ms, 1000, "last good observation remembered");

        // A later OBSERVED-empty poll is the truth → rows genuinely resolve.
        let empty_env = MockEnvironment::new().cmd("tend status", "");
        refresh_once(&FixtureSource, &empty_env, &store, &cfg, 3000);
        assert_eq!(store.len(), 0, "observed-empty means resolved");
    }

    #[test]
    fn dismissed_rows_do_not_consume_the_max_items_budget() {
        use crate::item::{ItemId, Urgency};
        let store = Store::new();
        let mut cfg = SourceConfig::for_kind(TestKind::TendRepos);
        cfg.max_items = 2;
        let mk = |k: &str, u: Urgency| {
            Item::new(TestKind::TendRepos, k, k, SpawnSpec::new("/code", k).unwrap())
                .urgent(u)
        };
        // Round 1: two hot rows fill the budget; the operator dismisses both.
        apply_poll(
            TestKind::TendRepos,
            PollOutcome::Fetched(vec![
                mk("hot1", Urgency::Critical),
                mk("hot2", Urgency::Critical),
            ]),
            &store,
            &cfg,
            1000,
        );
        assert!(store.dismiss(ItemId::derive(TestKind::TendRepos, "hot1")));
        assert!(store.dismiss(ItemId::derive(TestKind::TendRepos, "hot2")));
        // Round 2: upstream still reports the dismissed pair PLUS two calmer
        // offerable rows. The dismissed pair must not eat the budget — both
        // offerable rows reach the board, and the dismissals stay sticky.
        apply_poll(
            TestKind::TendRepos,
            PollOutcome::Fetched(vec![
                mk("hot1", Urgency::Critical),
                mk("hot2", Urgency::Critical),
                mk("calm1", Urgency::Normal),
                mk("calm2", Urgency::Normal),
            ]),
            &store,
            &cfg,
            2000,
        );
        let board = store.ranked(10, 2000);
        assert_eq!(board.len(), 2, "both offerable rows surfaced");
        assert!(board.iter().all(|s| s.title.starts_with("calm")));
        assert!(
            store.is_dismissed(ItemId::derive(TestKind::TendRepos, "hot1")),
            "dismissal survives riding along outside the budget"
        );
    }

    #[test]
    fn apply_poll_rank_sorts_before_truncation() {
        // A provider emitting its BEST item last must not lose it to the
        // max_items cut — apply_poll sorts by rank first.
        let store = Store::new();
        let mut cfg = SourceConfig::for_kind(TestKind::TendRepos);
        cfg.max_items = 1;
        let low = Item::new(
            TestKind::TendRepos,
            "low",
            "low",
            SpawnSpec::new("/code", "low").unwrap(),
        )
        .urgent(crate::item::Urgency::Low);
        let hot = Item::new(
            TestKind::TendRepos,
            "hot",
            "hot",
            SpawnSpec::new("/code", "hot").unwrap(),
        )
        .urgent(crate::item::Urgency::Critical);
        apply_poll(
            TestKind::TendRepos,
            PollOutcome::Fetched(vec![low, hot]),
            &store,
            &cfg,
            1000,
        );
        let ranked = store.ranked(10, 1000);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].title, "hot", "the best item survives the cap");
    }

    #[test]
    fn disabled_source_is_skipped_by_config_for() {
        let mut cfg = EngineConfig::default();
        let mut sc = SourceConfig::for_kind(TestKind::TendRepos);
        sc.enabled = false;
        cfg.per_source.insert(TestKind::TendRepos, sc);
        assert!(!cfg.config_for(TestKind::TendRepos).enabled);
        // An unconfigured source falls back to enabled defaults.
        assert!(cfg.config_for(TestKind::GitBranchPr).enabled);
    }
}
