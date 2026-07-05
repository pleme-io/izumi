//! The parallel watcher engine — the "always-updating in-memory daemon with
//! parallel refreshing sources".
//!
//! [`Engine::start`] spawns ONE tokio task per enabled source, each ticking
//! on its own cadence, each calling the source's `poll` on the tokio blocking
//! pool (so a subprocess/HTTP poll never stalls the runtime), every tick
//! releasing its update into the shared [`Store`]. A consumer reads the
//! store; the engine never blocks the GUI.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::catalog::Catalog;
use crate::env::Environment;
use crate::payload::Payload;
use crate::source::{apply_poll, EngineConfig, PollOutcome, Source};
use crate::store::Store;

/// The parallel watcher engine — one tokio task per enabled source.
pub struct Engine {
    handles: Vec<tokio::task::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Engine {
    /// Spawn one watcher task per enabled source. Must be called from within a
    /// tokio runtime.
    ///
    /// `nudge` is the optional shared freshness [`Notify`](tokio::sync::Notify)
    /// — a consumer surface fires it (e.g. its board opening) so every watcher
    /// whose data is older than its pacing gap re-polls RIGHT NOW; the board
    /// you open onto is being re-verified at that moment. The per-watcher gap
    /// (interval/4, clamped 5s..60s) makes the pacing structural: a nudge
    /// storm cannot hammer an API. `None` runs pure interval cadence.
    ///
    /// The owned-`Arc` parameter shapes are the frozen consumer API (the
    /// watchers hold their own clones for the engine's whole lifetime).
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn start<K: Catalog, A: Payload>(
        sources: Vec<Arc<dyn Source<K, A>>>,
        env: Arc<dyn Environment>,
        store: Arc<Store<K, A>>,
        cfg: EngineConfig<K>,
        nudge: Option<Arc<tokio::sync::Notify>>,
    ) -> Self {
        let stop: crate::refresh::StopFlag = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for src in sources {
            let scfg = cfg.config_for(src.kind());
            if !scfg.enabled {
                continue;
            }
            let kind = src.kind();
            // Stage 1 — one continuous refresh loop per source, driven by the
            // reusable `refresh::spawn_interval_refresh_nudged`. Each tick
            // re-polls the source (off the async reactor thread — poll may
            // block on a subprocess/HTTP) and pushes its typed outcome into
            // the shared store, so a source's data is re-fetched continuously,
            // never once.
            //
            // The refresher captures the source+env+cfg; the sink computes a
            // fresh `now_ms` per tick and applies the outcome (rank-sort +
            // truncate + ingest on Fetched; health-only on Unavailable — the
            // last-known rows survive a blip). A panic is likewise preserved
            // rather than wiping the source's slice.
            let refresh: Arc<dyn Fn() -> PollOutcome<K, A> + Send + Sync> = {
                let s = Arc::clone(&src);
                let e = Arc::clone(&env);
                let cfg2 = scfg.clone();
                Arc::new(move || s.poll(e.as_ref(), &cfg2))
            };
            let sink = {
                let store = Arc::clone(&store);
                let env = Arc::clone(&env);
                let cfg2 = scfg.clone();
                move |outcome: PollOutcome<K, A>| {
                    let now_ms = env.now_unix().saturating_mul(1000);
                    apply_poll(kind, outcome, &store, &cfg2, now_ms);
                }
            };
            let on_panic = move || {
                // The contract says poll must not panic. If it did, LOG it
                // (don't swallow silently) and PRESERVE the last-known rows —
                // an ingest of the empty default would wipe this source's whole
                // slice every tick.
                tracing::warn!(
                    kind = ?kind,
                    "source panicked; keeping last-known rows"
                );
            };
            // Freshness nudge: the shared notify fires so every watcher whose
            // data is older than its pacing gap re-polls right now. The
            // per-watcher gap (interval/4, clamped 5s..60s) makes the pacing
            // structural: a nudge storm cannot hammer an API.
            let min_gap = (scfg.interval / 4)
                .clamp(Duration::from_secs(5), Duration::from_mins(1));
            handles.push(crate::refresh::spawn_interval_refresh_nudged(
                scfg.interval,
                Arc::clone(&stop),
                refresh,
                sink,
                on_panic,
                nudge.clone().map(|n| (n, min_gap)),
            ));
        }
        Self { handles, stop }
    }

    /// Signal every watcher to stop + abort the tasks.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in &self.handles {
            h.abort();
        }
    }

    #[must_use]
    pub fn active_watchers(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MockEnvironment;
    use crate::item::Item;
    use crate::source::SourceConfig;
    use crate::spawn::SpawnSpec;
    use crate::testkit::TestKind;

    struct OneShot;
    impl Source<TestKind, SpawnSpec> for OneShot {
        fn kind(&self) -> TestKind {
            TestKind::TendRepos
        }
        fn poll(
            &self,
            _env: &dyn Environment,
            _cfg: &SourceConfig,
        ) -> PollOutcome<TestKind, SpawnSpec> {
            PollOutcome::Fetched(vec![Item::new(
                TestKind::TendRepos,
                "r",
                "r",
                SpawnSpec::new("/code", "r").unwrap(),
            )])
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn engine_spawns_watchers_polls_into_the_store_and_stops() {
        let store = Arc::new(Store::new());
        let env: Arc<dyn Environment> = Arc::new(MockEnvironment::new().at(1));
        let engine = Engine::start(
            vec![Arc::new(OneShot) as Arc<dyn Source<TestKind, SpawnSpec>>],
            Arc::clone(&env),
            Arc::clone(&store),
            EngineConfig::default(),
            None,
        );
        assert_eq!(engine.active_watchers(), 1);
        // The first tick fires immediately; wait for the row to land.
        for _ in 0..100 {
            if !store.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(store.len(), 1, "the watcher's first tick ingested");
        engine.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_sources_spawn_no_watcher() {
        let store: Arc<Store<TestKind, SpawnSpec>> = Arc::new(Store::new());
        let env: Arc<dyn Environment> = Arc::new(MockEnvironment::new());
        let cfg = EngineConfig {
            default_enabled: false,
            ..EngineConfig::default()
        };
        let engine = Engine::start(
            vec![Arc::new(OneShot) as Arc<dyn Source<TestKind, SpawnSpec>>],
            env,
            store,
            cfg,
            None,
        );
        assert_eq!(engine.active_watchers(), 0, "disabled source is skipped");
    }
}
