//! The shared maintenance tick — the one periodic pass every izumi consumer
//! runs against its [`Store`]: per-source decay + the hard gc cap, in one
//! call.
//!
//! Persistence discipline for the same tick: gate the snapshot write on BOTH
//! the store's [`generation`](Store::generation) (only persist when it
//! advanced since the last write — the dirty signal; a heartbeat tick never
//! touches disk) AND the [`WriterElection`](crate::writer::WriterElection)
//! (only the elected single writer persists — see [`crate::writer`]); then
//! write through [`Store::persist_file`] under the consumer's schema magic.

use crate::catalog::Catalog;
use crate::payload::Payload;
use crate::store::Store;

/// One maintenance tick: per-source decay (each source's rows age out against
/// `ttl_for(source)`; 0 = that source never ages out) followed by the hard
/// [`gc`](Store::gc) cap (`max_entries == 0` = unbounded). Both halves
/// tombstone what they drop, so a tick can never launder a dismissal or
/// forget a recurrence count.
pub fn maintenance_tick<K: Catalog, A: Payload>(
    store: &Store<K, A>,
    ttl_for: impl Fn(K) -> u64,
    max_entries: usize,
    now_ms: u64,
) {
    store.decay_per_source(now_ms, ttl_for);
    store.gc(max_entries, now_ms);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Item, ItemId, Urgency};
    use crate::spawn::SpawnSpec;
    use crate::testkit::TestKind;

    fn sug(source: TestKind, key: &str) -> Item<TestKind, SpawnSpec> {
        Item::new(source, key, key, SpawnSpec::new("/code", key).unwrap())
    }

    #[test]
    fn tick_decays_per_source_then_caps() {
        let store = Store::new();
        // A stale tend row (short TTL), a fresh grafana Critical, and a pile
        // of fresh tend Lows to trip the cap.
        store.ingest(
            TestKind::TendRepos,
            vec![sug(TestKind::TendRepos, "stale").urgent(Urgency::Low)],
            1_000,
        );
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "fire").urgent(Urgency::Critical)],
            9_000,
        );
        let fresh: Vec<_> = ["a", "b", "c"].iter().map(|k| sug(TestKind::TendRepos, k)).collect();
        store.ingest(TestKind::TendRepos, fresh, 9_000);
        // Wait — the second tend ingest replaced the source slice, dropping
        // "stale" already; re-add it via upsert to exercise decay.
        store.upsert(sug(TestKind::TendRepos, "stale"), 1_000);
        assert_eq!(store.len(), 5);

        maintenance_tick(
            &store,
            |k| match k {
                TestKind::TendRepos => 5_000,
                _ => 0, // never ages out
            },
            2,
            9_500,
        );
        // Decay dropped the stale tend row (last_seen 1000, ttl 5000 at 9500);
        // gc then capped to 2 keeping the highest effective-ranked.
        assert_eq!(store.len(), 2, "decay then cap");
        assert!(store.get(ItemId::derive(TestKind::TendRepos, "stale")).is_none());
        assert!(
            store.get(ItemId::derive(TestKind::GrafanaAlerts, "fire")).is_some(),
            "the Critical row survives the cap"
        );
    }
}
