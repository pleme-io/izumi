//! The typed item plane — the data model for a continuously-refreshing
//! "what could I act on right now" board.
//!
//! An [`Item`] is a *latent, always-actionable* task pointer: it carries the
//! typed action payload `A` (mado: a [`SpawnSpec`](crate::SpawnSpec) — cwd +
//! name + optional kickoff command) needed to act on it on Enter. Per the
//! UNREPRESENTABILITY model the payload type enforces its own construction
//! invariants (e.g. `SpawnSpec::new` rejects an empty cwd/name), so an item
//! only exists once it owns a valid action.

use crate::catalog::Catalog;
use crate::payload::Payload;

/// FNV-1a over bytes — the same run-stable hash family praça uses for its
/// `stable_seed`, so an item's identity is cheap + deterministic for the
/// same underlying task across refreshes (shade-in continuity rides on it).
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// The typed availability of a source's last poll — the border's honest
/// outcome. `Ok` means the upstream WAS observed (even if it yielded zero
/// items — genuine resolution); the other tiers mean it COULD NOT be
/// observed, so the store keeps the source's last-known rows (aging them out
/// via TTL) instead of wiping them. This is the "no silent wrong answers"
/// rule applied to the source border: an infrastructure blip is typed, never
/// disguised as "everything resolved".
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum SourceStatus {
    /// Upstream observed; the returned set is the truth.
    Ok,
    /// The source needs a param (site/`base_url`/…) the config doesn't supply.
    Unconfigured,
    /// The source's credential/secret is absent.
    AuthMissing,
    /// The fetch or parse failed (network, timeout, tool exit, bad shape).
    Error,
}

impl SourceStatus {
    /// Short operator-facing label for health footers / MCP output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SourceStatus::Ok => "ok",
            SourceStatus::Unconfigured => "needs config",
            SourceStatus::AuthMissing => "needs auth",
            SourceStatus::Error => "erroring",
        }
    }
}

/// How urgently an item wants attention — the dominant ranking axis.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Urgency {
    /// Background fodder (recent dirs, marks).
    Idle,
    /// Nice-to-do (dirty repo, stale TODO).
    Low,
    /// Real work queued (assigned ticket, your PR list).
    #[default]
    Normal,
    /// Should look soon (failing CI, on-call, your review requested).
    High,
    /// Actively on fire (incident, `CrashLoop`, alert firing).
    Critical,
}

impl Urgency {
    /// Numeric weight for ranking (0..=1000), urgency dominating score.
    #[must_use]
    pub fn weight(self) -> u32 {
        match self {
            Urgency::Idle => 0,
            Urgency::Low => 250,
            Urgency::Normal => 500,
            Urgency::High => 750,
            Urgency::Critical => 1000,
        }
    }
}

/// An item's typed rank contribution — an [`Urgency`] tier plus the
/// within-tier score (0..=1000) — applied via [`Item::ranked`]. The one typed
/// result a source's priority/severity scale produces; replaces loose
/// `(Urgency, u32)` tuples so a source can't swap the two or invent an
/// off-scale value. The named constructors ARE the normalized ladder every
/// scale maps onto, so Jira priorities and incident severities land on one
/// shared scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rank {
    pub urgency: Urgency,
    pub score: u32,
}

impl Rank {
    /// An explicit `(urgency, score)` rank (score clamped to 0..=1000).
    #[must_use]
    pub const fn new(urgency: Urgency, score: u32) -> Self {
        Self {
            urgency,
            score: if score > 1000 { 1000 } else { score },
        }
    }

    /// Top of the stream — the most urgent, highest-priority thing.
    #[must_use]
    pub const fn critical_top() -> Self {
        Self::new(Urgency::Critical, 1000)
    }
    /// Critical tier, just below the top (a High-priority ticket / an error).
    #[must_use]
    pub const fn critical() -> Self {
        Self::new(Urgency::Critical, 900)
    }
    /// Should-look-soon (a warning alert / a P3).
    #[must_use]
    pub const fn high() -> Self {
        Self::new(Urgency::High, 700)
    }
    /// Ordinary queued work (the calm default).
    #[must_use]
    pub const fn normal() -> Self {
        Self::new(Urgency::Normal, 500)
    }
    /// Nice-to-do.
    #[must_use]
    pub const fn low() -> Self {
        Self::new(Urgency::Low, 300)
    }
    /// The very bottom (a Lowest-priority ticket / a P5).
    #[must_use]
    pub const fn lowest() -> Self {
        Self::new(Urgency::Low, 150)
    }
}

/// Stable identity of an item — content-addressed from `(source, key)` so
/// the SAME underlying task keeps ONE id across refreshes (the store dedups
/// and the shade-in continuity ride on this). Wire form: a transparent `u64`
/// number, identical to the mado `SuggestionId`.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ItemId(pub u64);

impl ItemId {
    /// Derive from the owning source + a source-stable key (PR number, ticket
    /// id, repo path, …). Byte-identical to the mado derivation: fnv1a over
    /// `slug ':' key`.
    #[must_use]
    pub fn derive<K: Catalog>(source: K, key: &str) -> Self {
        Self::derive_slug(source.slug(), key)
    }

    /// Derive from a raw slug + key — the catalog-erased twin of
    /// [`ItemId::derive`] (cross-process consumers that know only the slug).
    #[must_use]
    pub fn derive_slug(slug: &str, key: &str) -> Self {
        let mut buf = String::with_capacity(slug.len() + 1 + key.len());
        buf.push_str(slug);
        buf.push(':');
        buf.push_str(key);
        Self(fnv1a(buf.as_bytes()))
    }
}

/// A cross-source correlation key: the CANONICAL identity of the real-world
/// issue behind an item, so the same ticket/PR/alert surfacing through two
/// sources can collapse to one board row at view time. The namespaced
/// constructors ARE the load-bearing invariant — the whole mechanism dies
/// silently if two providers spell the same identity differently, and a raw
/// `Option<String>` cannot prevent that drift. Construction only through the
/// constructors; `None` when no canonical identity exists (identity-less
/// rows never collapse).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct CorrKey(String);

impl CorrKey {
    /// A Jira issue: `jira:ASM-1234`.
    #[must_use]
    pub fn jira(key: &str) -> Option<Self> {
        let k = key.trim();
        if k.is_empty() {
            return None;
        }
        let mut s = String::from("jira:");
        s.push_str(k);
        Some(Self(s))
    }

    /// A GitHub PR/issue: `gh:owner/repo#42`. `None` unless `repo` is a true
    /// `owner/repo` (a bare name is ambiguous across owners). PRs and issues
    /// share one number space per repo, so this is canonical across the
    /// author/review/assigned providers.
    #[must_use]
    pub fn github(repo: &str, number: u64) -> Option<Self> {
        let r = repo.trim();
        if r.is_empty() || !r.contains('/') {
            return None;
        }
        let mut s = String::from("gh:");
        s.push_str(r);
        s.push('#');
        s.push_str(&number.to_string());
        Some(Self(s))
    }

    /// An alerting rule: `alert:HighCPU`. Deliberately per-RULE, so N firing
    /// instances of one rule fold into one board row.
    #[must_use]
    pub fn alert(alias: &str) -> Option<Self> {
        let a = alias.trim();
        if a.is_empty() {
            return None;
        }
        let mut s = String::from("alert:");
        s.push_str(a);
        Some(Self(s))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One latent task a board can shade in + act on. Plain typed data — built
/// by a source's `poll`, ranked by the store, rendered by the consumer.
///
/// Wire compatibility: the field names are EXACTLY the mado v1 codec's —
/// in particular the action payload field is literally named `spawn` for ANY
/// `A` (a documented v1-codec legacy; renaming it would break every persisted
/// mado snapshot).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Item<K, A> {
    pub id: ItemId,
    pub source: K,
    /// The task itself (the row's primary text), e.g. `pr#1234 fix the parser`.
    pub title: String,
    /// Optional secondary context (repo, assignee, age) shown dimmer.
    pub detail: Option<String>,
    pub urgency: Urgency,
    /// How to act on it — the typed action payload (v1 wire name: `spawn`).
    pub spawn: A,
    /// Source-relative score 0..=1000, ranking tie-break within an urgency.
    pub score: u32,
    /// Canonical cross-source identity ([`CorrKey`]) — the view layer
    /// collapses rows sharing it. `None` (the default) never collapses.
    #[serde(default)]
    pub corr: Option<CorrKey>,
}

impl<K: Catalog, A: Payload> Item<K, A> {
    /// Build an item. `key` is the source-stable id key; the urgency defaults
    /// to the source's default (override with [`Item::urgent`]).
    #[must_use]
    pub fn new(source: K, key: &str, title: impl Into<String>, spawn: A) -> Self {
        Self {
            id: ItemId::derive(source, key),
            source,
            title: title.into(),
            detail: None,
            urgency: source.default_urgency(),
            spawn,
            score: 500,
            corr: None,
        }
    }

    /// Attach the canonical cross-source identity. Option-taking so a
    /// provider passes a [`CorrKey`] constructor result straight through.
    #[must_use]
    pub fn correlated(mut self, c: Option<CorrKey>) -> Self {
        self.corr = c;
        self
    }

    #[must_use]
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        let d = d.into();
        self.detail = if d.trim().is_empty() { None } else { Some(d) };
        self
    }

    #[must_use]
    pub fn urgent(mut self, u: Urgency) -> Self {
        self.urgency = u;
        self
    }

    #[must_use]
    pub fn scored(mut self, score: u32) -> Self {
        self.score = score.min(1000);
        self
    }

    /// Apply a typed [`Rank`] — sets urgency + score in one move. The
    /// chokepoint a priority/severity scale feeds (e.g.
    /// `.ranked(JiraPriority::rank_of(p))`), replacing the
    /// `.urgent(u).scored(s)` pair so a source can't set one without the other.
    #[must_use]
    pub fn ranked(mut self, rank: Rank) -> Self {
        self.urgency = rank.urgency;
        self.score = rank.score.min(1000);
        self
    }

    /// Composite rank key — urgency weight in the high bits dominates, score
    /// in the low bits breaks ties. Higher = surfaced first.
    #[must_use]
    pub fn rank_key(&self) -> u64 {
        (u64::from(self.urgency.weight()) << 20) | u64::from(self.score.min(1000))
    }

    /// The typed board-row render of this item: `<emoji> <title>  <detail>`
    /// via `Display` (TYPED EMISSION — a render surface, not a `String`
    /// factory). Any latent badge (mado's ○) is the consumer's concern.
    pub fn picker_label(&self) -> PickerLabel<'_, K, A> {
        PickerLabel(self)
    }
}

/// The typed board-row label of an [`Item`] — a `Display` render surface
/// producing `<emoji> <title>  <detail>` (byte-identical to the mado
/// `picker_label` string composition).
#[must_use]
pub struct PickerLabel<'a, K, A>(&'a Item<K, A>);

impl<K: Catalog, A: Payload> core::fmt::Display for PickerLabel<'_, K, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let emoji = self.0.source.emoji();
        let title = self.0.title.trim();
        write!(f, "{emoji} {title}")?;
        if let Some(d) = &self.0.detail {
            let d = d.trim();
            write!(f, "  {d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::SpawnSpec;
    use crate::testkit::TestKind;
    use proptest::prelude::*;

    #[test]
    fn rank_named_scale_is_ordered_and_ranked_applies_both() {
        // The named ladder descends strictly: critical_top > critical (both
        // Critical) > high (High) > normal (Normal) > low (Low) > lowest (Low).
        assert_eq!(Rank::critical_top(), Rank::new(Urgency::Critical, 1000));
        assert_eq!(Rank::critical().urgency, Urgency::Critical);
        assert_eq!(Rank::high().urgency, Urgency::High);
        assert_eq!(Rank::normal().urgency, Urgency::Normal);
        assert_eq!(Rank::low().urgency, Urgency::Low);
        assert_eq!(Rank::lowest().urgency, Urgency::Low);
        assert!(Rank::critical_top().score > Rank::critical().score);
        assert!(Rank::low().score > Rank::lowest().score);
        // The rank_key induced by the ladder is strictly descending.
        let ladder = [
            Rank::critical_top(),
            Rank::critical(),
            Rank::high(),
            Rank::normal(),
            Rank::low(),
            Rank::lowest(),
        ];
        let spawn = SpawnSpec::new("/code", "n").unwrap();
        let keys: Vec<u64> = ladder
            .iter()
            .map(|r| {
                Item::new(TestKind::JiraSprint, "k", "t", spawn.clone())
                    .ranked(*r)
                    .rank_key()
            })
            .collect();
        assert!(keys.windows(2).all(|w| w[0] > w[1]), "ladder must be strictly ranked: {keys:?}");
        // new() clamps an off-scale score; ranked() sets BOTH urgency + score.
        assert_eq!(Rank::new(Urgency::Critical, 9999).score, 1000);
        let s = Item::new(TestKind::JiraSprint, "k", "t", spawn)
            .ranked(Rank::new(Urgency::High, 5000));
        assert_eq!(s.urgency, Urgency::High);
        assert_eq!(s.score, 1000, "ranked clamps score to 1000");
    }

    #[test]
    fn item_id_is_stable_per_source_key() {
        let a = ItemId::derive(TestKind::GitBranchPr, "1234");
        let b = ItemId::derive(TestKind::GitBranchPr, "1234");
        let c = ItemId::derive(TestKind::JiraSprint, "1234");
        assert_eq!(a, b, "same (source,key) → same id");
        assert_ne!(a, c, "different source → different id");
    }

    #[test]
    fn derive_slug_matches_typed_derive() {
        // The catalog-erased twin lands on the SAME id — the raw reader and
        // the typed store agree on identity.
        assert_eq!(
            ItemId::derive(TestKind::TendRepos, "mado"),
            ItemId::derive_slug("tend-repos", "mado")
        );
    }

    #[test]
    fn rank_key_orders_urgency_over_score() {
        let spawn = SpawnSpec::new("/x", "n").unwrap();
        let crit_low = Item::new(TestKind::GrafanaAlerts, "a", "fire", spawn.clone())
            .urgent(Urgency::Critical)
            .scored(0);
        let idle_high = Item::new(TestKind::RecentDirs, "b", "dir", spawn)
            .urgent(Urgency::Idle)
            .scored(1000);
        assert!(
            crit_low.rank_key() > idle_high.rank_key(),
            "urgency dominates score"
        );
    }

    #[test]
    fn picker_label_is_emoji_native() {
        use crate::catalog::Catalog as _;
        let spawn = SpawnSpec::new("/code/mado", "x").unwrap();
        let s = Item::new(TestKind::GithubReviewRequested, "1", "pr#1 fix", spawn)
            .detail("mado · 2h");
        let label = s.picker_label().to_string();
        assert!(label.starts_with(TestKind::GithubReviewRequested.emoji()));
        assert!(label.contains("pr#1 fix"));
        assert!(label.contains("mado"));
    }

    #[test]
    fn picker_label_trims_and_double_spaces_detail() {
        // Byte-exact composition contract: `<emoji> <title>  <detail>` with
        // both segments trimmed — identical to the mado String composition.
        let spawn = SpawnSpec::new("/x", "n").unwrap();
        let s = Item::new(TestKind::TendRepos, "k", "  mado  ", spawn.clone()).detail(" dirty ");
        assert_eq!(s.picker_label().to_string(), "\u{1F9F9} mado  dirty");
        let bare = Item::new(TestKind::TendRepos, "k", "mado", spawn);
        assert_eq!(bare.picker_label().to_string(), "\u{1F9F9} mado");
    }

    #[test]
    fn urgency_always_dominates_score_in_rank_key() {
        // A higher-urgency item scored 0 still outranks a lower-urgency one
        // scored max — the dominant axis is urgency, by construction.
        let order = [
            Urgency::Idle,
            Urgency::Low,
            Urgency::Normal,
            Urgency::High,
            Urgency::Critical,
        ];
        let spawn = SpawnSpec::new("/x", "n").unwrap();
        for w in order.windows(2) {
            let lo = Item::new(TestKind::RecentDirs, "a", "t", spawn.clone())
                .urgent(w[0])
                .scored(1000);
            let hi = Item::new(TestKind::RecentDirs, "b", "t", spawn.clone())
                .urgent(w[1])
                .scored(0);
            assert!(
                hi.rank_key() > lo.rank_key(),
                "{:?}@0 must outrank {:?}@1000",
                w[1],
                w[0]
            );
        }
    }

    proptest! {
        #[test]
        fn item_id_is_deterministic(key in ".*") {
            prop_assert_eq!(
                ItemId::derive(TestKind::GitBranchPr, &key),
                ItemId::derive(TestKind::GitBranchPr, &key)
            );
        }
    }
}
