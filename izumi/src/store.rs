//! The ephemeral, ranked, in-memory item store — the live cache the
//! parallel source watchers feed and a board consumer reads.
//!
//! It is deliberately SEPARATE from any persisted authored catalog: items are
//! transient external signals, not authored records, and must never pollute a
//! consumer's saved state. Each source OWNS its slice of the store — an
//! `ingest` replaces exactly that source's set, preserving `first_seen_ms`
//! for ids that persist (so a shade-in animation rides on a stable birth
//! time) and dropping vanished ones. Other sources' entries are untouched.
//!
//! Beyond the raw set, the store carries the LIVING-BOARD algebra:
//!
//! * **Recurrence** — a vanished id leaves a bounded tombstone; if the same
//!   issue re-fires within the tombstone window it comes back with its
//!   original birth time and a bumped `times_seen` (the anomaly-recurrence
//!   pattern), so a repeat offender ranks above a first-timer and the board
//!   can stamp `×N`.
//! * **Aging** — the ranked read adds a bounded waiting-time bonus, so an
//!   item that has sat unhandled slowly escalates within its urgency tier
//!   (never across tiers: urgency stays the dominant axis).
//! * **Lifecycle** — each row is `Offered → Accepted{session} /
//!   Snoozed{until} / Dismissed`. Accepted rows are soft-acked (demoted to
//!   the Idle tier — in progress, don't crowd new work); snoozed rows hide
//!   until their deadline; dismissed rows never surface again (their state
//!   survives re-ingest AND the tombstone window).
//! * **Source health** — the typed `PollOutcome` border records whether a
//!   source was actually observed (`Ok`) or could not be
//!   (`Unconfigured`/`AuthMissing`/`Error`), so "the board is calm" and "the
//!   board is blind" are distinguishable at a glance.
//!
//! A best-effort JSON snapshot lets a warm restart re-surface the last-known
//! set instantly while the watchers re-poll; a torn/absent snapshot simply
//! starts empty. The snapshot stamps its save time so ages rebase on load —
//! rows fresh at save stay fresh, rows stale at save still decay.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use tokio::sync::watch;

use crate::catalog::Catalog;
use crate::item::{CorrKey, Item, ItemId, SourceStatus, Urgency};
use crate::payload::Payload;
use crate::refresh::Reactive;

/// How long a vanished id's tombstone (recurrence memory) is kept, ms. A
/// re-fire inside this window restores birth + bumps `times_seen`; outside it
/// the issue counts as brand-new. Six hours: long enough to catch a flapping
/// alert across a workday, short enough that yesterday's noise doesn't haunt.
const TOMBSTONE_TTL_MS: u64 = 6 * 60 * 60 * 1000;

/// Hard cap on remembered tombstones (memory insurance) — oldest-vanished
/// evicted first.
const TOMBSTONE_CAP: usize = 512;

/// Aging escalation: +this much effective score per minute waited…
const AGE_BONUS_PER_MIN: u64 = 2;
/// …capped at this many minutes (3h → +360), so aging nudges within a tier
/// but can never dominate genuine urgency/score differences alone.
const AGE_BONUS_CAP_MIN: u64 = 180;

/// Recurrence escalation: +this much effective score per re-sighting…
const RECUR_BONUS_PER_SEEN: u64 = 40;
/// …counting at most this many re-sightings (10 → +400).
const RECUR_BONUS_CAP: u32 = 10;

/// The low-bit budget of the composite rank key (urgency sits above bit 20).
/// score (≤1000) + aging (≤360) + recurrence (≤400) ≤ 1760 fits comfortably;
/// the mask is defense-in-depth so a future bonus can never leak into the
/// urgency bits.
const RANK_LOW_MASK: u64 = 0xF_FFFF;

/// Where an item sits in its worked-on lifecycle. Serialized in the snapshot
/// so a dismissal survives a restart. Wire form (kebab-case, `kind`-tagged)
/// is identical to the mado `SuggestionState`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum ItemState {
    /// On the board, unworked.
    #[default]
    Offered,
    /// The operator opened a session for it — in progress. Soft-acked: the
    /// ranked read demotes it to the Idle tier so it stops crowding new work,
    /// and the board badges it ◐ instead of ○.
    Accepted {
        /// The session name the accept spawned/switched to.
        session: String,
    },
    /// Hidden until the deadline passes, then Offered again.
    Snoozed { until_ms: u64 },
    /// Never offered again (until the source itself stops reporting it AND
    /// the tombstone window lapses).
    Dismissed,
}

/// An item plus the store-side bookkeeping the renderer needs. The serde
/// RENAME keeps the mado v1 wire field `suggestion` while the Rust field is
/// `item`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredItem<K, A> {
    #[serde(rename = "suggestion")]
    pub item: Item<K, A>,
    /// When this id first appeared (ms) — the shade-in ramp anchor + the
    /// aging-escalation clock.
    #[serde(default)]
    pub first_seen_ms: u64,
    /// When this id was last confirmed present (ms) — decay/freshness.
    #[serde(default)]
    pub last_seen_ms: u64,
    /// How many times this issue has (re)appeared — 1 on first sighting,
    /// bumped when a tombstoned id re-fires (anomaly-recurrence).
    #[serde(default = "default_times_seen")]
    pub times_seen: u32,
    /// Worked-on lifecycle state.
    #[serde(default)]
    pub state: ItemState,
}

fn default_times_seen() -> u32 {
    1
}

/// Recurrence memory for a vanished id — enough to restore identity
/// continuity (birth, count, a sticky Dismissed) if the issue re-fires.
#[derive(Clone, Debug)]
struct Tombstone {
    first_seen_ms: u64,
    times_seen: u32,
    state: ItemState,
    vanished_ms: u64,
}

/// A source's last observed poll health — fed by the typed `PollOutcome`
/// border so an unobservable upstream is visible instead of silently empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceHealth {
    pub status: SourceStatus,
    /// When the source last completed a poll attempt (ms).
    pub last_poll_ms: u64,
    /// When the source last observed its upstream successfully (ms). 0 =
    /// never this process lifetime.
    pub last_ok_ms: u64,
}

/// Everything behind the ONE mutex — entries + recurrence tombstones +
/// per-source health share a lock so no two-lock ordering can deadlock.
struct StoreInner<K, A> {
    entries: BTreeMap<ItemId, StoredItem<K, A>>,
    tombstones: BTreeMap<ItemId, Tombstone>,
    health: BTreeMap<K, SourceHealth>,
}

impl<K, A> Default for StoreInner<K, A> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            health: BTreeMap::new(),
        }
    }
}

/// Thread-safe ranked item cache. Cloneable handle pattern is not used here
/// (callers hold an `Arc<Store<K, A>>`); the inner state is mutexed.
///
/// A monotonic `generation` is bumped only on a *meaningful* change (an id
/// added/removed, a row's displayed/ranked fields or lifecycle state changed,
/// a source's health status transitioned — NOT a mere `last_seen_ms`
/// heartbeat). A board memoizes its ranked read by it (O(1) reads while the
/// watchers idle) and a persist task uses it as the dirty signal — one
/// counter, the shikumi swap-then-observe contract.
///
/// The generation + the change-broadcast are the shared
/// [`Reactive`] core (stage 2 of the live-stream
/// substrate): every meaningful mutation bumps AND notifies every
/// [`subscribe`](Store::subscribe)r, so a board (and any other surface)
/// re-renders on the fact of a change instead of a fixed timer.
pub struct Store<K, A> {
    inner: Mutex<StoreInner<K, A>>,
    reactive: Reactive,
}

impl<K, A> Default for Store<K, A> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(StoreInner::default()),
            reactive: Reactive::new(),
        }
    }
}

/// Serializable view for the warm-restart snapshot.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct StoreSnapshot<K, A> {
    pub entries: Vec<StoredItem<K, A>>,
    /// When this snapshot was written (ms). On load, each entry's freshness
    /// rebases by `now - saved_at` so age-at-save is preserved — a row fresh
    /// at save doesn't get decayed as stale just because the restart came
    /// hours later. 0 (legacy snapshots) = no rebase.
    #[serde(default)]
    pub saved_at_ms: u64,
}

impl<K, A> Default for StoreSnapshot<K, A> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            saved_at_ms: 0,
        }
    }
}

/// The effective (living-board) rank key of a stored row at `now_ms`:
/// urgency dominates in the high bits — with an Accepted row demoted to the
/// Idle tier (soft-ack) — while the low bits carry score + the bounded
/// aging + recurrence escalations. Pure; the one ordering every read uses.
#[must_use]
pub fn effective_rank_key<K: Catalog, A: Payload>(st: &StoredItem<K, A>, now_ms: u64) -> u64 {
    let urgency = match st.state {
        ItemState::Accepted { .. } => Urgency::Idle,
        _ => st.item.urgency,
    };
    let waited_min = now_ms.saturating_sub(st.first_seen_ms) / 60_000;
    let bonus = u64::from(st.item.score.min(1000))
        + waited_min.min(AGE_BONUS_CAP_MIN) * AGE_BONUS_PER_MIN
        + u64::from(st.times_seen.saturating_sub(1).min(RECUR_BONUS_CAP))
            * RECUR_BONUS_PER_SEEN;
    (u64::from(urgency.weight()) << 20) | (bonus & RANK_LOW_MASK)
}

/// Whether a row is offerable on the board at `now_ms` — filters the
/// dismissed and the still-snoozed. Accepted rows remain offerable (demoted +
/// badged) so "in progress" stays legible instead of vanishing.
fn offerable<K, A>(st: &StoredItem<K, A>, now_ms: u64) -> bool {
    match st.state {
        ItemState::Dismissed => false,
        ItemState::Snoozed { until_ms } => now_ms >= until_ms,
        ItemState::Offered | ItemState::Accepted { .. } => true,
    }
}

impl<K: Catalog, A: Payload> Store<K, A> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreInner<K, A>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current change-generation (Acquire). Bumped only on a meaningful change;
    /// a board memoizes its ranked read by it + a persist task uses it as the
    /// dirty signal.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.reactive.generation()
    }

    /// A change-notification subscription (stage 3): a [`watch::Receiver`] that
    /// fires on every meaningful mutation. Async consumers `.changed().await`;
    /// a synchronous board polls `.has_changed()` per frame and re-lists on
    /// the fact of a change — no fixed timer, no missed update.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.reactive.subscribe()
    }

    /// Bump the change-generation (Release) AND broadcast — called after a
    /// meaningful mutation.
    fn bump(&self) {
        self.reactive.bump();
    }

    /// Insert-or-update ONE row under an already-held lock, honoring the
    /// tombstone window (recurrence restore) and preserving lifecycle state on
    /// update. Returns whether the mutation was meaningful.
    fn upsert_locked(g: &mut StoreInner<K, A>, s: Item<K, A>, now_ms: u64) -> bool {
        if let Some(existing) = g.entries.get_mut(&s.id) {
            // Bump only on a DISPLAYED/RANKED change, not a mere
            // last_seen heartbeat — else every 30s poll would force a
            // re-render + a disk write for an unchanged row.
            let meaningful = existing.item != s;
            existing.last_seen_ms = now_ms;
            existing.item = s;
            meaningful
        } else {
            // A re-fire inside the tombstone window keeps its identity:
            // original birth (aging keeps counting), bumped times_seen
            // (recurrence escalation), and a sticky Dismissed.
            let tomb = g.tombstones.remove(&s.id).filter(|t| {
                now_ms.saturating_sub(t.vanished_ms) <= TOMBSTONE_TTL_MS
            });
            let (first_seen_ms, times_seen, state) = match tomb {
                Some(t) => (t.first_seen_ms, t.times_seen.saturating_add(1), t.state),
                None => (now_ms, 1, ItemState::Offered),
            };
            g.entries.insert(
                s.id,
                StoredItem {
                    first_seen_ms,
                    last_seen_ms: now_ms,
                    times_seen,
                    state,
                    item: s,
                },
            );
            true
        }
    }

    /// Move a dropped entry into the recurrence tombstones (bounded).
    fn tombstone_locked(g: &mut StoreInner<K, A>, st: &StoredItem<K, A>, now_ms: u64) {
        g.tombstones.insert(
            st.item.id,
            Tombstone {
                first_seen_ms: st.first_seen_ms,
                times_seen: st.times_seen,
                state: st.state.clone(),
                vanished_ms: now_ms,
            },
        );
        if g.tombstones.len() > TOMBSTONE_CAP {
            // Evict oldest-vanished first.
            let mut by_age: Vec<(ItemId, u64)> = g
                .tombstones
                .iter()
                .map(|(id, t)| (*id, t.vanished_ms))
                .collect();
            by_age.sort_by_key(|t| t.1);
            let drop_n = g.tombstones.len() - TOMBSTONE_CAP;
            for (id, _) in by_age.into_iter().take(drop_n) {
                g.tombstones.remove(&id);
            }
        }
    }

    /// Replace the item set contributed by `source`. Ids that persist keep
    /// their `first_seen_ms` + lifecycle state; ids of this source that
    /// vanished are tombstoned (recurrence memory) and dropped; entries from
    /// other sources are untouched.
    pub fn ingest(&self, source: K, items: Vec<Item<K, A>>, now_ms: u64) {
        let mut changed = false;
        {
            let mut g = self.lock();
            let incoming: BTreeSet<ItemId> = items.iter().map(|s| s.id).collect();
            // Tombstone + drop this source's vanished ids (keep all other
            // sources'). Two passes because the borrow of the drop set ends
            // before mutation of the tombstone map.
            let dropped: Vec<StoredItem<K, A>> = g
                .entries
                .values()
                .filter(|st| st.item.source == source && !incoming.contains(&st.item.id))
                .cloned()
                .collect();
            for st in &dropped {
                Self::tombstone_locked(&mut g, st, now_ms);
                g.entries.remove(&st.item.id);
                changed = true;
            }
            for s in items {
                if Self::upsert_locked(&mut g, s, now_ms) {
                    changed = true;
                }
            }
        }
        if changed {
            self.bump();
        }
    }

    /// Additive single-row upsert — the agent/MCP inject path. Unlike
    /// [`Store::ingest`] it never drops the source's other rows, so repeated
    /// injections accumulate (and decay by TTL like everything else).
    pub fn upsert(&self, s: Item<K, A>, now_ms: u64) {
        let changed = {
            let mut g = self.lock();
            Self::upsert_locked(&mut g, s, now_ms)
        };
        if changed {
            self.bump();
        }
    }

    /// Record a source's poll health from the typed `PollOutcome` border.
    /// Bumps/broadcasts only on a STATUS transition (Ok→Error, …) — the
    /// timestamps alone are heartbeats.
    pub fn record_poll(&self, source: K, status: SourceStatus, now_ms: u64) {
        let changed = {
            let mut g = self.lock();
            let entry = g.health.entry(source).or_insert(SourceHealth {
                status,
                last_poll_ms: 0,
                last_ok_ms: 0,
            });
            let transitioned = entry.status != status || entry.last_poll_ms == 0;
            entry.status = status;
            entry.last_poll_ms = now_ms;
            if status == SourceStatus::Ok {
                entry.last_ok_ms = now_ms;
            }
            transitioned
        };
        if changed {
            self.bump();
        }
    }

    /// Every source's last-known poll health, catalog-ordered.
    #[must_use]
    pub fn health(&self) -> Vec<(K, SourceHealth)> {
        let g = self.lock();
        g.health.iter().map(|(k, h)| (*k, *h)).collect()
    }

    /// Mark a row Accepted (in progress) — a board's accept path calls this
    /// with the session it spawned/switched to. `false` if the id is gone.
    pub fn mark_accepted(&self, id: ItemId, session: &str) -> bool {
        self.set_state(
            id,
            ItemState::Accepted {
                session: session.to_owned(),
            },
        )
    }

    /// Dismiss a row — never offered again (survives re-ingest + tombstone).
    pub fn dismiss(&self, id: ItemId) -> bool {
        self.set_state(id, ItemState::Dismissed)
    }

    /// Snooze a row until `until_ms`.
    pub fn snooze(&self, id: ItemId, until_ms: u64) -> bool {
        self.set_state(id, ItemState::Snoozed { until_ms })
    }

    /// Whether this id is Dismissed — on its live entry OR its tombstone
    /// (a dismissed row that decayed still counts, so the exemption in
    /// `apply_poll`'s budget survives the gap between decay and re-fire).
    #[must_use]
    pub fn is_dismissed(&self, id: ItemId) -> bool {
        let g = self.lock();
        match g.entries.get(&id) {
            Some(st) => matches!(st.state, ItemState::Dismissed),
            None => g
                .tombstones
                .get(&id)
                .is_some_and(|t| matches!(t.state, ItemState::Dismissed)),
        }
    }

    fn set_state(&self, id: ItemId, state: ItemState) -> bool {
        let changed = {
            let mut g = self.lock();
            match g.entries.get_mut(&id) {
                Some(st) if st.state != state => {
                    st.state = state;
                    true
                }
                Some(_) => return true, // already in that state — success, no bump
                None => return false,
            }
        };
        if changed {
            self.bump();
        }
        true
    }

    /// Drop every entry whose `last_seen_ms` is older than `ttl_ms` — the
    /// decay pass (a source that stops reporting an item lets it age out even
    /// if the source itself never re-polls). Decayed ids leave tombstones so a
    /// slow flap still counts as recurrence.
    pub fn decay(&self, now_ms: u64, ttl_ms: u64) {
        self.decay_per_source(now_ms, |_| ttl_ms);
    }

    /// Per-source decay: each entry ages out against `ttl_for(its source)`, so a
    /// slow source (e.g. a 1h poll) doesn't flicker under a fast global TTL. A
    /// `ttl_for` of 0 means that source's entries never age out. Also purges
    /// tombstones past their recurrence window.
    pub fn decay_per_source(&self, now_ms: u64, ttl_for: impl Fn(K) -> u64) {
        let removed = {
            let mut g = self.lock();
            let stale: Vec<StoredItem<K, A>> = g
                .entries
                .values()
                .filter(|st| {
                    let ttl = ttl_for(st.item.source);
                    ttl != 0 && now_ms.saturating_sub(st.last_seen_ms) > ttl
                })
                .cloned()
                .collect();
            for st in &stale {
                Self::tombstone_locked(&mut g, st, now_ms);
                g.entries.remove(&st.item.id);
            }
            g.tombstones
                .retain(|_, t| now_ms.saturating_sub(t.vanished_ms) <= TOMBSTONE_TTL_MS);
            !stale.is_empty()
        };
        if removed {
            self.bump();
        }
    }

    /// Hard memory cap: if the store exceeds `max_entries`, evict until it
    /// fits — non-offerable rows (Dismissed, still-Snoozed) go first (they
    /// are invisible; their lifecycle state survives in the tombstone), then
    /// the lowest EFFECTIVE-ranked / stalest (the same living-board axis the
    /// board orders by, so an Accepted row's demotion counts here too).
    /// Every eviction is tombstoned, so a gc under pressure can never
    /// resurrect a dismissal or forget a recurrence count. `max_entries == 0`
    /// is unbounded. Insurance against a source that stops polling with no TTL.
    pub fn gc(&self, max_entries: usize, now_ms: u64) {
        if max_entries == 0 {
            return;
        }
        let removed = {
            let mut g = self.lock();
            if g.entries.len() <= max_entries {
                return;
            }
            let mut ranked: Vec<(ItemId, bool, u64, u64)> = g
                .entries
                .values()
                .map(|st| {
                    (
                        st.item.id,
                        offerable(st, now_ms),
                        effective_rank_key(st, now_ms),
                        st.last_seen_ms,
                    )
                })
                .collect();
            // Keep the top `max_entries`: offerable first, then effective
            // rank desc, then fresher, then id.
            ranked.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then(b.2.cmp(&a.2))
                    .then(b.3.cmp(&a.3))
                    .then(a.0.cmp(&b.0))
            });
            let keep: BTreeSet<ItemId> =
                ranked.into_iter().take(max_entries).map(|(id, ..)| id).collect();
            let evicted: Vec<StoredItem<K, A>> = g
                .entries
                .values()
                .filter(|st| !keep.contains(&st.item.id))
                .cloned()
                .collect();
            for st in &evicted {
                Self::tombstone_locked(&mut g, st, now_ms);
                g.entries.remove(&st.item.id);
            }
            !evicted.is_empty()
        };
        if removed {
            self.bump();
        }
    }

    /// The top `max` offerable items at `now_ms`, in living-board order
    /// ([`effective_rank_key`]: urgency → score+aging+recurrence → birth → id).
    #[must_use]
    pub fn ranked(&self, max: usize, now_ms: u64) -> Vec<Item<K, A>> {
        self.ranked_stored(max, now_ms)
            .into_iter()
            .map(|st| st.item)
            .collect()
    }

    /// Like [`Store::ranked`] but carries the store bookkeeping (birth for
    /// shade-in, `times_seen` for ×N, `state` for the ◐ badge). Dismissed and
    /// still-snoozed rows are filtered out here — the one offerability gate
    /// every read shares.
    #[must_use]
    pub fn ranked_stored(&self, max: usize, now_ms: u64) -> Vec<StoredItem<K, A>> {
        let g = self.lock();
        let mut v: Vec<StoredItem<K, A>> = g
            .entries
            .values()
            .filter(|st| offerable(st, now_ms))
            .cloned()
            .collect();
        drop(g);
        v.sort_by(|a, b| {
            effective_rank_key(b, now_ms)
                .cmp(&effective_rank_key(a, now_ms))
                .then(a.first_seen_ms.cmp(&b.first_seen_ms))
                .then(a.item.id.cmp(&b.item.id))
        });
        v.truncate(max);
        v
    }

    /// Resolve an item by id (an accept path looks up the action payload).
    #[must_use]
    pub fn get(&self, id: ItemId) -> Option<Item<K, A>> {
        self.lock().entries.get(&id).map(|st| st.item.clone())
    }

    /// Resolve the full stored row (lifecycle + recurrence bookkeeping).
    #[must_use]
    pub fn get_stored(&self, id: ItemId) -> Option<StoredItem<K, A>> {
        self.lock().entries.get(&id).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().entries.is_empty()
    }
    pub fn clear(&self) {
        let had = {
            let mut g = self.lock();
            let had = !g.entries.is_empty();
            g.entries.clear();
            g.tombstones.clear();
            had
        };
        if had {
            self.bump();
        }
    }

    /// Alpha (0..=255) for an id's shade-in ramp: 0 at birth, 255 after
    /// `shade_in_ms`. Used by a renderer to gently fade new rows in.
    #[must_use]
    pub fn shade_alpha(&self, id: ItemId, now_ms: u64, shade_in_ms: u64) -> u8 {
        let first = match self.lock().entries.get(&id) {
            Some(st) => st.first_seen_ms,
            None => return 255,
        };
        shade_ramp(first, now_ms, shade_in_ms)
    }

    /// Snapshot the current set for a warm restart, stamped with the save time
    /// so ages rebase on load.
    #[must_use]
    pub fn to_snapshot(&self, now_ms: u64) -> StoreSnapshot<K, A> {
        StoreSnapshot {
            entries: self.lock().entries.values().cloned().collect(),
            saved_at_ms: now_ms,
        }
    }

    /// Replace the store from a snapshot (warm restart). Each entry's
    /// freshness is REBASED by `now - saved_at` so age-at-save is preserved:
    /// a row fresh at save is fresh now (survives the post-load decay), a row
    /// already stale at save still decays. Birth times stay absolute — aging
    /// keeps counting from the true first sighting. Legacy snapshots
    /// (`saved_at_ms == 0`, or an implausible future stamp) load unrebased.
    pub fn load_snapshot(&self, snap: StoreSnapshot<K, A>, now_ms: u64) {
        let shift = if snap.saved_at_ms > 0 && now_ms >= snap.saved_at_ms {
            now_ms - snap.saved_at_ms
        } else {
            0
        };
        {
            let mut g = self.lock();
            g.entries.clear();
            for mut st in snap.entries {
                st.last_seen_ms = st.last_seen_ms.saturating_add(shift);
                g.entries.insert(st.item.id, st);
            }
        }
        self.bump();
    }

    /// Warm-restart load: a magic-framed, BLAKE3-verified snapshot file (the
    /// caller supplies its schema magic — see [`crate::persist`]). A missing
    /// file, a wrong schema magic (format bump), or a torn body (hash
    /// mismatch) all start empty — never feed garbage rows to a board.
    pub fn load_file(&self, path: &Path, magic: &'static [u8], now_ms: u64) {
        // Reclaim `.tmp.<pid>` temps a crashed prior run left behind, before we
        // read. The atomic persist renames its own temp away on success, so the
        // only way one lingers is a crash between create and rename.
        crate::persist::sweep_orphan_temps(path);
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Some(json) = crate::persist::unframe_snapshot(magic, &bytes) else {
            return; // bad magic / hash mismatch → start empty
        };
        if let Ok(snap) = serde_json::from_slice::<StoreSnapshot<K, A>>(&json) {
            self.load_snapshot(snap, now_ms);
        }
    }

    /// Crash-safe atomic persist: serialize → BLAKE3-frame (under the caller's
    /// schema magic) → write a pid-tagged temp (after `create_dir_all`) →
    /// `sync_all` → rename (see [`crate::persist::atomic_write_framed`]).
    /// Snapshotting clones under the lock then drops it, so the disk I/O is
    /// lock-free. Best-effort — a write failure never blocks the caller.
    pub fn persist_file(&self, path: &Path, magic: &'static [u8], now_ms: u64) {
        let snap = self.to_snapshot(now_ms);
        let Ok(json) = serde_json::to_vec(&snap) else {
            return;
        };
        crate::persist::atomic_write_framed(magic, path, &json);
    }
}

/// Diversify a rank-ordered item list: keep the order, but cap how many rows
/// any single source may contribute, so one noisy source (20 `CrashLoop` pods)
/// can't drown your PRs / tickets / incidents. `cap == 0` disables the cap.
/// Pure — the unit a board's balanced band is tested through.
#[must_use]
pub fn balance_per_source<K: Catalog, A: Payload>(
    items: Vec<StoredItem<K, A>>,
    max: usize,
    cap: usize,
) -> Vec<StoredItem<K, A>> {
    let mut counts: BTreeMap<K, usize> = BTreeMap::new();
    let mut out: Vec<StoredItem<K, A>> = Vec::with_capacity(max.min(items.len()));
    for st in items {
        if out.len() >= max {
            break;
        }
        if cap > 0 {
            let c = counts.entry(st.item.source).or_insert(0);
            if *c >= cap {
                continue;
            }
            *c += 1;
        }
        out.push(st);
    }
    out
}

/// Collapse rows sharing a [`CorrKey`] — the same real-world issue via two
/// sources becomes ONE board row. Pure + order-preserving (a view concern:
/// the store's per-source slices, tombstones, and health are untouched):
///
/// * `corr == None` → passes through untouched, always.
/// * corr ∈ `live_corrs` (the keys of rows a live-session dedup already
///   suppressed) → DROPPED: the real-world task has a live session via its
///   twin, so offering the other spelling would offer work in progress.
/// * first occurrence of a corr → the survivor (input is display-ordered,
///   so keep-first IS highest-rank on the empty query / best match on a
///   query).
/// * later occurrences → absorbed: an absorbed `Accepted` state is copied
///   onto the survivor (badge honesty — the row renders ◐), and the
///   absorbed source's emoji joins the survivor's detail.
#[must_use]
pub fn collapse_correlated<K: Catalog, A: Payload, S: std::hash::BuildHasher>(
    items: Vec<StoredItem<K, A>>,
    live_corrs: &std::collections::HashSet<CorrKey, S>,
) -> Vec<StoredItem<K, A>> {
    let mut out: Vec<StoredItem<K, A>> = Vec::with_capacity(items.len());
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut absorbed: BTreeMap<usize, Vec<K>> = BTreeMap::new();
    for st in items {
        let Some(corr) = st.item.corr.clone() else {
            out.push(st);
            continue;
        };
        if live_corrs.contains(&corr) {
            continue;
        }
        match seen.get(corr.as_str()) {
            None => {
                seen.insert(corr.as_str().to_owned(), out.len());
                out.push(st);
            }
            Some(&idx) => {
                let survivor = &mut out[idx];
                if !matches!(survivor.state, ItemState::Accepted { .. })
                    && matches!(st.state, ItemState::Accepted { .. })
                {
                    survivor.state = st.state.clone();
                }
                absorbed.entry(idx).or_default().push(st.item.source);
            }
        }
    }
    for (idx, sources) in absorbed {
        let survivor = &mut out[idx];
        let own = survivor.item.source;
        let mut added: Vec<K> = Vec::new();
        for src in sources {
            if src != own && !added.contains(&src) {
                added.push(src);
            }
        }
        if added.is_empty() {
            continue;
        }
        let mut marker = String::from(" \u{00B7} +"); // middle dot, plus
        for src in added {
            marker.push_str(src.emoji());
        }
        let detail = if let Some(mut d) = survivor.item.detail.take() {
            d.push_str(&marker);
            d
        } else {
            let mut d = String::from("merged");
            d.push_str(&marker);
            d
        };
        survivor.item.detail = Some(detail);
    }
    out
}

/// Pure shade-in ramp — factored out for testing.
#[must_use]
pub fn shade_ramp(first_seen_ms: u64, now_ms: u64, shade_in_ms: u64) -> u8 {
    if shade_in_ms == 0 {
        return 255;
    }
    let elapsed = now_ms.saturating_sub(first_seen_ms);
    if elapsed >= shade_in_ms {
        return 255;
    }
    // 0..255 linear ramp.
    let frac = (elapsed.saturating_mul(255)) / shade_in_ms;
    u8::try_from(frac.min(255)).unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::SpawnSpec;
    use crate::testkit::TestKind;
    use proptest::prelude::*;

    /// The test snapshot magic — the ported framed-persist tests run against
    /// izumi's parameterized framing with a crate-local schema tag.
    const MAGIC: &[u8] = b"izumi-store-test v1\n";

    fn sug(source: TestKind, key: &str, title: &str) -> Item<TestKind, SpawnSpec> {
        Item::new(source, key, title, SpawnSpec::new("/code/x", title).unwrap())
    }

    #[test]
    fn ingest_replaces_only_its_own_source() {
        let store = Store::new();
        store.ingest(
            TestKind::TendRepos,
            vec![sug(TestKind::TendRepos, "a", "a"), sug(TestKind::TendRepos, "b", "b")],
            100,
        );
        store.ingest(TestKind::GitBranchPr, vec![sug(TestKind::GitBranchPr, "c", "c")], 100);
        assert_eq!(store.len(), 3);
        // Re-ingest tend with only "a" → "b" drops, git "c" untouched.
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 200);
        assert_eq!(store.len(), 2);
        assert!(store.get(ItemId::derive(TestKind::GitBranchPr, "c")).is_some());
        assert!(store.get(ItemId::derive(TestKind::TendRepos, "b")).is_none());
    }

    #[test]
    fn first_seen_is_preserved_across_reingest() {
        let store = Store::new();
        let id = ItemId::derive(TestKind::TendRepos, "a");
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 100);
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 500);
        // shade_alpha uses first_seen=100, so at now=100+shade it's full; the
        // re-ingest at 500 must NOT reset birth.
        let st = store.ranked_stored(10, 500);
        let first = st.iter().find(|s| s.item.id == id).unwrap().first_seen_ms;
        assert_eq!(first, 100, "first_seen preserved across re-ingest");
    }

    #[test]
    fn recurrence_survives_the_tombstone_window() {
        let store = Store::new();
        let id = ItemId::derive(TestKind::GrafanaAlerts, "flap");
        // Fires, resolves (vanishes), re-fires within the window → identity
        // continuity: original birth, times_seen 2.
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "flap", "flap")],
            1_000,
        );
        store.ingest(TestKind::GrafanaAlerts, vec![], 2_000);
        assert!(store.get(id).is_none(), "vanished from the board");
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "flap", "flap")],
            60_000,
        );
        let st = store.get_stored(id).expect("re-fired");
        assert_eq!(st.times_seen, 2, "recurrence counted");
        assert_eq!(st.first_seen_ms, 1_000, "original birth restored");

        // A re-fire AFTER the window is a brand-new issue.
        store.ingest(TestKind::GrafanaAlerts, vec![], 70_000);
        let later = 70_000 + TOMBSTONE_TTL_MS + 1;
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "flap", "flap")],
            later,
        );
        let st = store.get_stored(id).expect("re-fired late");
        assert_eq!(st.times_seen, 1, "outside the window = new issue");
        assert_eq!(st.first_seen_ms, later);
    }

    #[test]
    fn recurrence_and_aging_escalate_within_a_tier_never_across() {
        let now = 10 * 60 * 60 * 1000; // 10h
        let mk = |first_seen_ms: u64, times_seen: u32, urgency: Urgency| StoredItem {
            item: sug(TestKind::TendRepos, "k", "t").urgent(urgency),
            first_seen_ms,
            last_seen_ms: now,
            times_seen,
            state: ItemState::Offered,
        };
        // Same tier: an old repeat offender outranks a fresh first-timer.
        let fresh = mk(now, 1, Urgency::Normal);
        let veteran = mk(now - 3_600_000, 5, Urgency::Normal);
        assert!(
            effective_rank_key(&veteran, now) > effective_rank_key(&fresh, now),
            "aging + recurrence escalate within the tier"
        );
        // Across tiers: no amount of aging/recurrence beats real urgency.
        let ancient_low = mk(0, u32::MAX, Urgency::Low);
        let fresh_normal = mk(now, 1, Urgency::Normal);
        assert!(
            effective_rank_key(&fresh_normal, now) > effective_rank_key(&ancient_low, now),
            "urgency stays the dominant axis"
        );
    }

    #[test]
    fn lifecycle_accept_demotes_snooze_hides_dismiss_removes() {
        let store = Store::new();
        let hot = sug(TestKind::GrafanaAlerts, "hot", "hot").urgent(Urgency::Critical);
        let calm = sug(TestKind::TendRepos, "calm", "calm").urgent(Urgency::Normal);
        let hot_id = hot.id;
        let calm_id = calm.id;
        store.ingest(TestKind::GrafanaAlerts, vec![hot], 1_000);
        store.ingest(TestKind::TendRepos, vec![calm], 1_000);
        assert_eq!(store.ranked(10, 1_000)[0].id, hot_id, "critical on top");

        // Accept → soft-ack: demoted below the calm Offered row, still listed.
        assert!(store.mark_accepted(hot_id, "🔥 hot"));
        let ranked = store.ranked(10, 1_000);
        assert_eq!(ranked.len(), 2, "accepted stays on the board");
        assert_eq!(ranked[0].id, calm_id, "accepted row is demoted (in progress)");
        assert!(matches!(
            store.get_stored(hot_id).unwrap().state,
            ItemState::Accepted { .. }
        ));

        // Accepted state survives the source re-reporting the item.
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "hot", "hot").urgent(Urgency::Critical)],
            2_000,
        );
        assert!(matches!(
            store.get_stored(hot_id).unwrap().state,
            ItemState::Accepted { .. }
        ));

        // Snooze hides until the deadline, then re-offers.
        assert!(store.snooze(calm_id, 5_000));
        assert!(
            !store.ranked(10, 4_999).iter().any(|s| s.id == calm_id),
            "snoozed row hidden before the deadline"
        );
        assert!(
            store.ranked(10, 5_000).iter().any(|s| s.id == calm_id),
            "snoozed row re-offered at the deadline"
        );

        // Dismiss removes from every read and STICKS across re-ingest + the
        // tombstone window.
        assert!(store.dismiss(calm_id));
        assert!(!store.ranked(10, 6_000).iter().any(|s| s.id == calm_id));
        store.ingest(
            TestKind::TendRepos,
            vec![sug(TestKind::TendRepos, "calm", "calm")],
            7_000,
        );
        assert!(
            !store.ranked(10, 7_000).iter().any(|s| s.id == calm_id),
            "dismissed survives re-ingest"
        );
        store.ingest(TestKind::TendRepos, vec![], 8_000); // vanish → tombstone
        store.ingest(
            TestKind::TendRepos,
            vec![sug(TestKind::TendRepos, "calm", "calm")],
            9_000,
        );
        assert!(
            !store.ranked(10, 9_000).iter().any(|s| s.id == calm_id),
            "dismissed survives the tombstone round-trip"
        );

        // An unknown id fails, a repeat set succeeds without a bump.
        assert!(!store.dismiss(ItemId(42)));
    }

    #[test]
    fn upsert_is_additive_never_drops_siblings() {
        let store = Store::new();
        store.upsert(sug(TestKind::TendRepos, "a", "a"), 100);
        store.upsert(sug(TestKind::TendRepos, "b", "b"), 200);
        assert_eq!(store.len(), 2, "upserts accumulate");
        store.upsert(sug(TestKind::TendRepos, "a", "a2"), 300);
        assert_eq!(store.len(), 2, "same id updates in place");
        assert_eq!(
            store.get(ItemId::derive(TestKind::TendRepos, "a")).unwrap().title,
            "a2"
        );
    }

    #[test]
    fn record_poll_bumps_on_transition_not_heartbeat() {
        let store: Store<TestKind, SpawnSpec> = Store::new();
        let g0 = store.generation();
        store.record_poll(TestKind::JiraAssigned, SourceStatus::Ok, 1_000);
        let g1 = store.generation();
        assert!(g1 > g0, "first poll records (transition from unknown)");
        store.record_poll(TestKind::JiraAssigned, SourceStatus::Ok, 2_000);
        assert_eq!(store.generation(), g1, "same-status heartbeat must not bump");
        store.record_poll(TestKind::JiraAssigned, SourceStatus::Error, 3_000);
        assert!(store.generation() > g1, "a status transition bumps");
        let health = store.health();
        let (_, h) = health.iter().find(|(k, _)| *k == TestKind::JiraAssigned).unwrap();
        assert_eq!(h.status, SourceStatus::Error);
        assert_eq!(h.last_ok_ms, 2_000, "last_ok remembers the last good poll");
    }

    #[test]
    fn ranked_orders_by_urgency_then_score() {
        let store = Store::new();
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "fire", "fire").urgent(Urgency::Critical)],
            100,
        );
        store.ingest(
            TestKind::TendRepos,
            vec![sug(TestKind::TendRepos, "repo", "repo").urgent(Urgency::Low)],
            100,
        );
        let ranked = store.ranked(10, 100);
        assert_eq!(ranked[0].source, TestKind::GrafanaAlerts, "critical first");
    }

    #[test]
    fn decay_drops_stale_entries() {
        let store = Store::new();
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 100);
        store.decay(100 + 5000, 1000);
        assert_eq!(store.len(), 0, "stale entry decayed");
    }

    #[test]
    fn shade_ramp_is_linear_and_clamps() {
        assert_eq!(shade_ramp(0, 0, 600), 0);
        assert_eq!(shade_ramp(0, 300, 600), 127);
        assert_eq!(shade_ramp(0, 600, 600), 255);
        assert_eq!(shade_ramp(0, 10_000, 600), 255);
        assert_eq!(shade_ramp(0, 0, 0), 255, "zero shade = instant solid");
    }

    #[test]
    fn snapshot_round_trips_and_rebases_ages() {
        let store = Store::new();
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 100_000);
        let snap = store.to_snapshot(200_000);
        assert_eq!(snap.saved_at_ms, 200_000);

        // Load 1 hour later: the entry was 100s old at save; it must be 100s
        // old relative to NOW after load (not 1h+100s old).
        let restored = Store::new();
        let now = 3_800_000;
        restored.load_snapshot(snap, now);
        assert_eq!(restored.len(), 1);
        let st = restored.ranked_stored(1, now)[0].clone();
        assert_eq!(now - st.last_seen_ms, 100_000, "age-at-save preserved");
        assert_eq!(st.first_seen_ms, 100_000, "birth stays absolute (aging clock)");
        // The post-load decay with a 15-min TTL keeps it (it is 100s old).
        restored.decay(now, 900_000);
        assert_eq!(restored.len(), 1, "fresh-at-save row survives restart decay");

        // Legacy snapshot (saved_at 0) loads unrebased.
        let legacy = StoreSnapshot {
            entries: vec![StoredItem {
                item: sug(TestKind::TendRepos, "b", "b"),
                first_seen_ms: 50,
                last_seen_ms: 50,
                times_seen: 1,
                state: ItemState::Offered,
            }],
            saved_at_ms: 0,
        };
        let old = Store::new();
        old.load_snapshot(legacy, now);
        assert_eq!(old.ranked_stored(1, now)[0].last_seen_ms, 50, "legacy: no rebase");
    }

    #[test]
    fn framed_persist_load_round_trips_and_rejects_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snap.json");

        let store = Store::new();
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 100);
        store.persist_file(&path, MAGIC, 100);

        // Round-trip: a fresh store warm-loads the set.
        let loaded: Store<TestKind, SpawnSpec> = Store::new();
        loaded.load_file(&path, MAGIC, 100);
        assert_eq!(loaded.len(), 1, "warm restart re-surfaces the set");

        // A torn body (flip a json byte) → hash mismatch → start empty.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let torn: Store<TestKind, SpawnSpec> = Store::new();
        torn.load_file(&path, MAGIC, 100);
        assert_eq!(torn.len(), 0, "a torn body is rejected → empty");

        // Wrong magic (a foreign/old file) → start empty.
        std::fs::write(&path, b"garbage not ours").unwrap();
        let bad: Store<TestKind, SpawnSpec> = Store::new();
        bad.load_file(&path, MAGIC, 100);
        assert_eq!(bad.len(), 0, "wrong magic → empty");
    }

    #[test]
    fn generation_bumps_on_change_not_on_last_seen_heartbeat() {
        let store = Store::new();
        let g0 = store.generation();
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 100);
        let g1 = store.generation();
        assert!(g1 > g0, "first insert bumps");

        // Re-ingest the SAME item (only last_seen advances) → NO bump.
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 500);
        assert_eq!(store.generation(), g1, "a last_seen heartbeat must not bump");

        // A changed displayed field → bump.
        let mut changed = sug(TestKind::TendRepos, "a", "a");
        changed.title = String::from("a CHANGED");
        store.ingest(TestKind::TendRepos, vec![changed], 600);
        let g2 = store.generation();
        assert!(g2 > g1, "a displayed change bumps");

        // Removal → bump.
        store.ingest(TestKind::TendRepos, vec![], 700);
        assert!(store.generation() > g2, "removal bumps");
    }

    #[test]
    fn subscribe_broadcasts_on_change_not_on_heartbeat() {
        // Stage 2: an ingest (meaningful change) wakes a subscriber; a pure
        // last_seen heartbeat does not. This is the store→GUI wake a board
        // polls each frame.
        let store = Store::new();
        let mut rx = store.subscribe();
        assert!(!rx.has_changed().unwrap(), "no change before first ingest");

        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 100);
        assert!(rx.has_changed().unwrap(), "an ingest broadcasts to subscribers");
        rx.mark_unchanged();

        // Re-ingest the SAME row (only last_seen advances) → no broadcast.
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 500);
        assert!(!rx.has_changed().unwrap(), "a heartbeat must not wake a subscriber");

        // A new row → broadcast again.
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "b", "b")], 600);
        assert!(rx.has_changed().unwrap(), "a new row wakes the subscriber");
    }

    #[test]
    fn gc_caps_total_keeping_highest_ranked() {
        let store = Store::new();
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "fire", "fire").urgent(Urgency::Critical)],
            100,
        );
        let mut lows = Vec::new();
        for k in ["a", "b", "c", "d", "e"] {
            lows.push(sug(TestKind::TendRepos, k, k).urgent(Urgency::Low));
        }
        store.ingest(TestKind::TendRepos, lows, 100);
        assert_eq!(store.len(), 6);

        store.gc(3, 100);
        assert_eq!(store.len(), 3, "capped to 3");
        assert!(
            store.get(ItemId::derive(TestKind::GrafanaAlerts, "fire")).is_some(),
            "the Critical row is kept (highest rank)"
        );

        store.gc(0, 100); // unbounded → no-op
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn gc_tombstones_evictions_and_prefers_evicting_non_offerable() {
        let store = Store::new();
        // Three rows: a dismissed Critical (invisible, raw-rank highest), an
        // offerable Normal, an offerable Low.
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "dead", "dead").urgent(Urgency::Critical)],
            100,
        );
        store.ingest(
            TestKind::TendRepos,
            vec![
                sug(TestKind::TendRepos, "mid", "mid").urgent(Urgency::Normal),
                sug(TestKind::TendRepos, "low", "low").urgent(Urgency::Low),
            ],
            100,
        );
        let dead_id = ItemId::derive(TestKind::GrafanaAlerts, "dead");
        assert!(store.dismiss(dead_id));
        // Cap to 2: the DISMISSED row is evicted first despite its raw rank —
        // offerable rows win the survival contest.
        store.gc(2, 200);
        assert_eq!(store.len(), 2);
        assert!(store.get(dead_id).is_none(), "non-offerable evicted first");
        assert!(store.get(ItemId::derive(TestKind::TendRepos, "mid")).is_some());
        // The eviction left a tombstone carrying Dismissed: a re-ingest from
        // the still-firing source must NOT resurrect the row as Offered.
        store.ingest(
            TestKind::GrafanaAlerts,
            vec![sug(TestKind::GrafanaAlerts, "dead", "dead").urgent(Urgency::Critical)],
            300,
        );
        assert!(
            !store.ranked(10, 300).iter().any(|s| s.id == dead_id),
            "gc under pressure must not launder a dismissal"
        );
        assert!(store.is_dismissed(dead_id), "dismissal restored from the tombstone");
    }

    #[test]
    fn decay_per_source_respects_each_sources_ttl() {
        let store = Store::new();
        store.ingest(TestKind::TendRepos, vec![sug(TestKind::TendRepos, "a", "a")], 1000);
        store.ingest(TestKind::GrafanaAlerts, vec![sug(TestKind::GrafanaAlerts, "b", "b")], 1000);
        // At now=6000: tend (ttl 1000) is 5000ms stale → drop; grafana (ttl huge) kept.
        store.decay_per_source(6000, |k| match k {
            TestKind::TendRepos => 1000,
            _ => 100_000,
        });
        assert!(
            store.get(ItemId::derive(TestKind::TendRepos, "a")).is_none(),
            "the short-TTL source aged out"
        );
        assert!(
            store.get(ItemId::derive(TestKind::GrafanaAlerts, "b")).is_some(),
            "the long-TTL source is kept (no flicker)"
        );
    }

    fn stored(source: TestKind, key: &str) -> StoredItem<TestKind, SpawnSpec> {
        StoredItem {
            item: sug(source, key, key),
            first_seen_ms: 100,
            last_seen_ms: 100,
            times_seen: 1,
            state: ItemState::Offered,
        }
    }

    #[test]
    fn collapse_folds_cross_source_twins_keeping_first_and_badge_honesty() {
        use crate::catalog::Catalog as _;
        let corr = CorrKey::jira("ASM-1");
        // Sprint twin ranked first (input is display-ordered), assigned twin
        // second and ACCEPTED — the survivor keeps its slot but inherits the
        // in-progress badge, and wears the absorbed source's emoji.
        let mut sprint = stored(TestKind::JiraSprint, "ASM-1");
        sprint.item = sprint.item.clone().correlated(corr.clone()).detail("Highest");
        let mut assigned = stored(TestKind::JiraAssigned, "ASM-1");
        assigned.item = assigned.item.clone().correlated(corr.clone());
        assigned.state = ItemState::Accepted { session: String::from("s") };
        let none1 = stored(TestKind::GrafanaIncidents, "a");
        let none2 = stored(TestKind::GrafanaIncidents, "b");

        let out = collapse_correlated(
            vec![sprint, assigned, none1, none2],
            &std::collections::HashSet::new(),
        );
        assert_eq!(out.len(), 3, "twins folded; None-corr rows pass through");
        assert_eq!(out[0].item.source, TestKind::JiraSprint, "first occurrence survives");
        assert!(
            matches!(out[0].state, ItemState::Accepted { .. }),
            "absorbed Accepted state dominates the badge"
        );
        let d = out[0].item.detail.as_deref().unwrap();
        assert!(
            d.contains(TestKind::JiraAssigned.emoji()),
            "absorbed source emoji joins the detail: {d}"
        );

        // Namespaces never collide: jira:X vs alert:X are distinct keys.
        assert_ne!(
            CorrKey::jira("X").unwrap().as_str(),
            CorrKey::alert("X").unwrap().as_str()
        );
        // gh keys demand a true owner/repo.
        assert!(CorrKey::github("mado", 1).is_none(), "bare repo name is ambiguous");
        assert!(CorrKey::github("pleme-io/mado", 1).is_some());
    }

    #[test]
    fn collapse_drops_twins_of_live_suppressed_rows() {
        let corr = CorrKey::jira("ASM-2").unwrap();
        let mut twin = stored(TestKind::JiraAssigned, "ASM-2");
        twin.item = twin.item.clone().correlated(Some(corr.clone()));
        let other = stored(TestKind::TendRepos, "r");
        let mut live: std::collections::HashSet<CorrKey> = std::collections::HashSet::new();
        live.insert(corr);
        let out = collapse_correlated(vec![twin, other], &live);
        assert_eq!(out.len(), 1, "the live task's other spelling never resurfaces");
        assert_eq!(out[0].item.source, TestKind::TendRepos);
    }

    proptest! {
        #[test]
        fn collapse_is_idempotent_and_shrinking(n in 0usize..16, dup in 0usize..4) {
            let mut items = Vec::new();
            for i in 0..n {
                let mut st = stored(TestKind::TendRepos, &i.to_string());
                if i % 3 == 0 {
                    st.item = st.item.clone()
                        .correlated(CorrKey::jira(&(i % (dup + 1)).to_string()));
                }
                items.push(st);
            }
            let empty = std::collections::HashSet::new();
            let once = collapse_correlated(items.clone(), &empty);
            let twice = collapse_correlated(once.clone(), &empty);
            prop_assert!(once.len() <= items.len());
            // Idempotent on the ID sequence (details may gain merge markers
            // on the first pass only when something was absorbed; a second
            // pass absorbs nothing, so ids AND details are stable).
            let ids: Vec<_> = once.iter().map(|s| s.item.id).collect();
            let ids2: Vec<_> = twice.iter().map(|s| s.item.id).collect();
            prop_assert_eq!(ids, ids2);
            // Each Some-corr appears at most once.
            let mut seen = std::collections::HashSet::new();
            for st in &once {
                if let Some(c) = &st.item.corr {
                    prop_assert!(seen.insert(c.as_str().to_owned()), "corr appears once");
                }
            }
        }
    }

    #[test]
    fn balance_caps_per_source_keeping_rank_order() {
        // One Critical grafana alert + five Low tend repos.
        let mut items = vec![stored(TestKind::GrafanaAlerts, "fire")];
        items[0].item = items[0].item.clone().urgent(Urgency::Critical);
        for key in ["a", "b", "c", "d", "e"] {
            items.push(stored(TestKind::TendRepos, key));
        }
        // Rank first so balance sees urgency order (grafana on top).
        items.sort_by_key(|s| std::cmp::Reverse(s.item.rank_key()));
        let out = balance_per_source(items, 10, 2);
        assert_eq!(out.len(), 3, "1 grafana + 2 (capped) tend");
        assert_eq!(out[0].item.source, TestKind::GrafanaAlerts, "critical kept on top");
        assert_eq!(
            out.iter().filter(|s| s.item.source == TestKind::TendRepos).count(),
            2,
            "tend capped at 2"
        );
    }

    proptest! {
        #[test]
        fn ranked_is_sorted_by_effective_key_desc(n in 0usize..20) {
            let store = Store::new();
            let items: Vec<Item<TestKind, SpawnSpec>> = (0..n)
                .map(|i| sug(TestKind::TendRepos, &i.to_string(), "t").scored((u32::try_from(i).unwrap_or(0) * 37) % 1001))
                .collect();
            store.ingest(TestKind::TendRepos, items, 100);
            let ranked = store.ranked_stored(100, 100);
            for w in ranked.windows(2) {
                prop_assert!(effective_rank_key(&w[0], 100) >= effective_rank_key(&w[1], 100));
            }
        }

        #[test]
        fn balance_never_exceeds_cap_or_max(total in 0usize..30, cap in 1usize..5, max in 0usize..15) {
            let items: Vec<StoredItem<TestKind, SpawnSpec>> =
                (0..total).map(|i| stored(TestKind::TendRepos, &i.to_string())).collect();
            let out = balance_per_source(items, max, cap);
            prop_assert!(out.len() <= max);
            prop_assert!(
                out.iter().filter(|s| s.item.source == TestKind::TendRepos).count() <= cap
            );
        }

        /// Aging + recurrence bonuses are BOUNDED: no combination of waiting
        /// time, score, and times_seen lets a row cross into the next urgency
        /// tier's key range.
        #[test]
        fn escalation_never_crosses_an_urgency_tier(
            waited_min in 0u64..1_000_000,
            score in 0u32..=1000,
            times in 1u32..10_000,
        ) {
            let now = waited_min.saturating_mul(60_000).saturating_add(1);
            let st = StoredItem {
                item: sug(TestKind::TendRepos, "k", "t")
                    .urgent(Urgency::Normal)
                    .scored(score),
                first_seen_ms: 1,
                last_seen_ms: now,
                times_seen: times,
                state: ItemState::Offered,
            };
            let key = effective_rank_key(&st, now);
            let tier_floor = u64::from(Urgency::Normal.weight()) << 20;
            let next_tier = u64::from(Urgency::High.weight()) << 20;
            prop_assert!(key >= tier_floor, "never falls below its own tier");
            prop_assert!(key < next_tier, "never escalates across tiers");
        }

        /// DETERMINISTIC race coverage. Every store op takes the Mutex, so the
        /// store is LINEARIZABLE — concurrent interleavings reduce to some
        /// ordering of complete ops. Exhaustively exercising random ORDERINGS
        /// (proptest-seeded → deterministic, replayable) therefore soundly
        /// covers the map's concurrent behaviour. We assert the invariants the
        /// lock-free generation counter + the board memoization depend on:
        /// generation is monotonic non-decreasing, and `ranked` is always sorted.
        #[test]
        fn store_is_linearizable_invariants_hold(
            seq in prop::collection::vec((0u8..6, 0u8..8), 0..40),
        ) {
            let store = Store::new();
            let mut prev_gen = store.generation();
            let mut now = 1000u64;
            for (kind, arg) in seq {
                now += 10;
                match kind {
                    0 => {
                        let items: Vec<_> = (0..arg)
                            .map(|i| sug(TestKind::TendRepos, &i.to_string(), "t"))
                            .collect();
                        store.ingest(TestKind::TendRepos, items, now);
                    }
                    1 => {
                        let items: Vec<_> = (0..arg)
                            .map(|i| {
                                sug(TestKind::GrafanaAlerts, &i.to_string(), "t")
                                    .urgent(Urgency::Critical)
                            })
                            .collect();
                        store.ingest(TestKind::GrafanaAlerts, items, now);
                    }
                    2 => store.decay(now, u64::from(arg) * 5),
                    3 => store.gc(usize::from(arg), now),
                    4 => {
                        let _ = store.dismiss(ItemId::derive(
                            TestKind::TendRepos,
                            &arg.to_string(),
                        ));
                    }
                    _ => {
                        let _ = store.mark_accepted(
                            ItemId::derive(TestKind::GrafanaAlerts, &arg.to_string()),
                            "s",
                        );
                    }
                }
                let g = store.generation();
                prop_assert!(g >= prev_gen, "generation must be monotonic non-decreasing");
                prev_gen = g;
                let ranked = store.ranked_stored(1000, now);
                for w in ranked.windows(2) {
                    prop_assert!(
                        effective_rank_key(&w[0], now) >= effective_rank_key(&w[1], now),
                        "ranked must stay sorted after every op"
                    );
                }
            }
        }
    }

    /// REAL-concurrency smoke (complements the deterministic linearizability
    /// proptest): 8 threads hammer the one store. The Mutex makes a data race on
    /// the map unrepresentable; this catches what an ordering test can't — a
    /// DEADLOCK (lock-ordering / a lock held across a call → the test hangs and
    /// CI times out), a panic, or an atomic-ordering bug. Final invariants hold.
    #[test]
    fn concurrent_hammering_no_deadlock_no_panic_invariants_hold() {
        let store = std::sync::Arc::new(Store::new());
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let s = std::sync::Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                // Two threads share each source so ingest's retain/insert races.
                let src = if t % 2 == 0 {
                    TestKind::TendRepos
                } else {
                    TestKind::GitBranchPr
                };
                for i in 0..300u64 {
                    let n = usize::try_from(i % 6).unwrap_or(0);
                    let items: Vec<_> = (0..n).map(|j| sug(src, &j.to_string(), "t")).collect();
                    s.ingest(src, items, 1000 + i);
                    let _ = s.generation();
                    let _ = s.ranked_stored(10, 1000 + i);
                    s.record_poll(src, SourceStatus::Ok, 1000 + i);
                    if i % 5 == 0 {
                        s.decay(1000 + i, 50);
                    }
                    if i % 7 == 0 {
                        let _ = s.dismiss(ItemId::derive(src, "1"));
                    }
                    if i % 9 == 0 {
                        s.gc(30, 1000 + i);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("a store thread panicked or deadlocked");
        }
        let ranked = store.ranked_stored(1000, 2000);
        for w in ranked.windows(2) {
            assert!(effective_rank_key(&w[0], 2000) >= effective_rank_key(&w[1], 2000));
        }
    }
}
