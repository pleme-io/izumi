//! # izumi-config — the shikumi `TieredConfig` board-config surface
//!
//! The typed operator-facing configuration for an izumi board — the
//! generalized extraction of mado's `SuggestionsConfig` (the Ctrl-S
//! suggestion stream). One [`BoardConfig`] carries the board-wide knobs
//! (master switch, TTL floor, persistence, entry caps) plus the per-source
//! override list ([`SourceEntry`], keyed by kebab kind slug), and the
//! load-bearing [`BoardConfig::effective_sources`] merge that keeps a
//! params-only override from disarming the rest of the armed surface.
//!
//! What deliberately stays OUT of this crate: the picker-presentation knobs
//! (`max_visible`, `shade_in_ms`, `reserved_rows`, `attention_on_critical`)
//! are consumer-side render concerns and remain in mado's config — this
//! surface is the *board substrate* half only.
//!
//! Tiered per the shikumi contract: `bare()` = the whole board off
//! (stripped); `prescribed_default()` = the board ON at the fleet cadence
//! with an EMPTY override list — the prescribed *arm-list* is the
//! consumer's catalog knowledge, supplied per-call to
//! [`BoardConfig::effective_sources`] / [`bridge::to_engine_config`], never
//! baked into this generic crate.

pub mod bridge;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use bridge::to_engine_config;

/// serde default helper — mirrors mado's `default_true` so a partial YAML
/// entry (`kind` only) arms the source, byte-compatible with the mado
/// `suggestions.sources` schema.
const fn default_true() -> bool {
    true
}

/// Per-source override in [`BoardConfig::sources`] — field-for-field the
/// wire shape of mado's `SuggestionSourceConfig`, so operator YAML written
/// for mado's `suggestions.sources` parses unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEntry {
    /// Source kind kebab slug (e.g. `git-branch-pr`). Unknown slugs ignored.
    pub kind: String,
    /// Run this source. Defaults `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the poll cadence (seconds).
    #[serde(default)]
    pub interval_secs: Option<u64>,
    /// Override the per-poll item cap.
    #[serde(default)]
    pub max_items: Option<usize>,
    /// Free per-source params (token env override, JQL, grafana folder, …).
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl SourceEntry {
    /// An override that simply enables a source at its default cadence/params
    /// — the typed way to opt a catalog kind into the stream. Pass the kind's
    /// slug (`K::slug()` for a typed [`izumi::Catalog`] kind, or the literal
    /// kebab string an operator would write in YAML).
    #[must_use]
    pub fn enable(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            enabled: true,
            interval_secs: None,
            max_items: None,
            params: BTreeMap::new(),
        }
    }
}

/// The continuously-refreshing board stream a consumer shades in (see
/// `izumi`). Tiered: bare = fully OFF (stripped — no engine, no watchers,
/// no rows); prescribed = ON with a gentle cadence. Per-source overrides
/// live in `sources` (keyed by kebab catalog slug).
// The four independent bools ARE the wire shape (byte-compatible with mado's
// `suggestions:` YAML schema) — folding them into enums would break every
// operator config, so the excessive-bools refactor is deliberately declined.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoardConfig {
    /// Master switch. `false` (bare) = no engine, no watchers, no rows.
    pub enabled: bool,
    /// Whether a source with no explicit `sources` override runs by default.
    pub default_enabled: bool,
    /// Cap how many rows a single source may contribute to the visible band,
    /// so one noisy source (20 `CrashLoop` pods) can't drown your PRs/tickets.
    /// The band stays diverse. 0 = no cap.
    pub per_source_cap: usize,
    /// Global TTL FLOOR (seconds): an item is dropped at least this long
    /// after it was last seen. NOT the whole story — each source's items live
    /// for `max(3× its poll interval, this floor)`, so a slow (hourly) source
    /// never flickers under a fast global TTL. `0` does **not** mean "never":
    /// it removes the floor, leaving each source's `3× poll interval`
    /// fallback in force — items still age out, just per-source.
    pub ttl_secs: u64,
    /// Lazily persist the cache to disk (atomic temp→rename) so a restart
    /// re-surfaces the last-known items instantly while the watchers re-poll.
    pub persist: bool,
    /// Coalesce disk writes: persist at most once per this many seconds, so
    /// the parallel watchers can't thrash the disk. 0 = persist on every
    /// change.
    pub persist_debounce_secs: u64,
    /// Hard cap on total cached items (memory insurance): if exceeded, the
    /// lowest-ranked / stalest are evicted. The store is already structurally
    /// bounded for well-behaved sources; this guards a source that stops
    /// polling with `ttl_secs = 0` or a mis-set `max_items`. 0 = unbounded.
    pub max_entries: usize,
    /// Per-source overrides, MERGED over the prescribed arm-list by kind slug
    /// (see [`BoardConfig::effective_sources`]): an entry overrides that one
    /// kind (params, cadence, enabled) and never disarms the others. Kinds in
    /// neither list follow `default_enabled`.
    pub sources: Vec<SourceEntry>,
    /// Escape hatch: `true` makes `sources` REPLACE the prescribed arm-list
    /// entirely instead of merging over it — an explicit allow-list for an
    /// operator who wants exactly-these-sources and nothing else.
    pub sources_replace: bool,
}

impl Default for BoardConfig {
    fn default() -> Self {
        <Self as shikumi::TieredConfig>::prescribed_default()
    }
}

impl shikumi::TieredConfig for BoardConfig {
    /// Bare tier — the whole stream off (stripped). Every field enumerated
    /// explicitly: the floor is a documented value, never an accident of
    /// `..Default::default()` drift.
    fn bare() -> Self {
        Self {
            enabled: false,
            default_enabled: false,
            per_source_cap: 0,
            ttl_secs: 0,
            persist: false,
            persist_debounce_secs: 0,
            max_entries: 0,
            sources: Vec::new(),
            sources_replace: false,
        }
    }

    /// Prescribed tier — the stream is ON at the fleet cadence. Unlike the
    /// mado original (whose prescribed tier bakes the 25-source arm-list into
    /// the config), this generic surface ships an EMPTY override list and
    /// `default_enabled = true`: the prescribed arm-list is catalog knowledge
    /// the consumer supplies per-call to
    /// [`BoardConfig::effective_sources`] / [`bridge::to_engine_config`].
    ///
    /// A yaml `sources` list is a per-kind OVERRIDE merged over that
    /// arm-list (see [`BoardConfig::effective_sources`]) — supplying params
    /// for one source never disarms the rest.
    fn prescribed_default() -> Self {
        Self {
            enabled: true,
            default_enabled: true,
            per_source_cap: 3,
            ttl_secs: 900,
            persist: true,
            persist_debounce_secs: 5,
            max_entries: 200,
            sources: Vec::new(),
            sources_replace: false,
        }
    }
}

impl BoardConfig {
    /// The EFFECTIVE per-source override list the engine runs from: the
    /// consumer's prescribed arm-list (`prescribed`, kind slugs) with the
    /// operator's `sources` entries merged over it by kind slug (an operator
    /// entry wins wholesale for its kind; unknown slugs ride along and are
    /// ignored downstream). This is the load-bearing fix for the "a
    /// params-only yaml override disarmed 22 sources" failure: serde replaces
    /// a yaml `Vec` outright, so the merge has to happen here, after
    /// deserialize. `sources_replace = true` restores replace semantics as an
    /// explicit allow-list.
    #[must_use]
    pub fn effective_sources(&self, prescribed: &[&str]) -> Vec<SourceEntry> {
        if self.sources_replace {
            return self.sources.clone();
        }
        let mut merged: Vec<SourceEntry> =
            prescribed.iter().copied().map(SourceEntry::enable).collect();
        for over in &self.sources {
            match merged.iter_mut().find(|m| m.kind == over.kind) {
                Some(slot) => *slot = over.clone(),
                None => merged.push(over.clone()),
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use shikumi::TieredConfig as _;

    use super::*;

    /// A stand-in for a consumer's prescribed arm-list (mado's is the
    /// 25-source workflow surface; the semantics under test are list-size
    /// independent).
    const PRESCRIBED: &[&str] = &[
        "git-branch-pr",
        "tend-repos",
        "jira-assigned",
        "todo-backlog",
        "grafana-alerts",
    ];

    #[test]
    fn source_overrides_merge_over_prescribed_not_replace() {
        // The exact failure this pins: a nix/yaml block supplying ONLY a
        // params override for one source (serde replaces the whole Vec) must
        // NOT disarm the other prescribed sources.
        let mut over = SourceEntry::enable("jira-assigned");
        over.params
            .insert(String::from("site"), String::from("acme.atlassian.net"));
        let cfg = BoardConfig {
            sources: vec![over],
            ..BoardConfig::prescribed_default()
        };
        let eff = cfg.effective_sources(PRESCRIBED);
        let jira = eff
            .iter()
            .find(|s| s.kind == "jira-assigned")
            .expect("jira-assigned present");
        assert_eq!(
            jira.params.get("site").map(String::as_str),
            Some("acme.atlassian.net"),
            "the override's params win for its kind"
        );
        assert_eq!(
            eff.iter().filter(|s| s.enabled).count(),
            PRESCRIBED.len(),
            "a params-only override must not change the armed count"
        );
        // A kind unknown to the prescribed list rides along (e.g. an opt-in
        // cost poller the operator arms explicitly).
        let cfg2 = BoardConfig {
            sources: vec![SourceEntry::enable("aws-health")],
            ..BoardConfig::prescribed_default()
        };
        assert!(
            cfg2.effective_sources(PRESCRIBED)
                .iter()
                .any(|s| s.kind == "aws-health" && s.enabled),
            "an explicitly-armed opt-out kind is appended"
        );
        // An explicit disable override disarms exactly its kind.
        let mut off = SourceEntry::enable("todo-backlog");
        off.enabled = false;
        let cfg3 = BoardConfig {
            sources: vec![off],
            ..BoardConfig::prescribed_default()
        };
        let eff3 = cfg3.effective_sources(PRESCRIBED);
        assert!(
            !eff3
                .iter()
                .find(|s| s.kind == "todo-backlog")
                .expect("todo-backlog present")
                .enabled,
            "an explicit disable wins for its kind"
        );
        assert_eq!(
            eff3.iter().filter(|s| s.enabled).count(),
            PRESCRIBED.len() - 1,
            "only the disabled kind is disarmed"
        );
        // The escape hatch: sources_replace = true restores allow-list
        // semantics — exactly the listed sources, nothing else.
        let cfg4 = BoardConfig {
            sources: vec![SourceEntry::enable("jira-assigned")],
            sources_replace: true,
            ..BoardConfig::prescribed_default()
        };
        assert_eq!(
            cfg4.effective_sources(PRESCRIBED).len(),
            1,
            "replace mode is an allow-list"
        );
    }

    #[test]
    fn tiers_are_honest() {
        let bare = BoardConfig::bare();
        assert!(!bare.enabled, "bare strips the whole stream");
        assert!(!bare.default_enabled);
        assert!(bare.sources.is_empty());

        let pres = BoardConfig::prescribed_default();
        assert!(pres.enabled, "prescribed arms the board");
        assert!(pres.default_enabled);
        assert_eq!(pres.per_source_cap, 3);
        assert_eq!(pres.ttl_secs, 900);
        assert!(pres.persist);
        assert_eq!(pres.persist_debounce_secs, 5);
        assert_eq!(pres.max_entries, 200);
        assert!(pres.sources.is_empty(), "the arm-list is consumer-supplied");
        assert!(!pres.sources_replace);

        // The standard idiom delegates to the prescribed tier.
        assert_eq!(BoardConfig::default(), pres);
    }

    #[test]
    fn yaml_operator_block_round_trips_and_merges() {
        // A representative operator YAML block — the same shape a mado
        // `suggestions:` section (minus the picker-only knobs) or an
        // izumi-board config file carries.
        let yaml = "\
enabled: true
ttl_secs: 600
sources:
  - kind: jira-assigned
    params:
      site: acme.atlassian.net
  - kind: grafana-alerts
    enabled: false
  - kind: aws-health
    interval_secs: 3600
    max_items: 2
";
        let cfg: BoardConfig = serde_yaml::from_str(yaml).expect("operator YAML parses");
        // Fields absent from the YAML take the prescribed default (container
        // serde(default) = Default = prescribed) — matching mado's partial-
        // yaml semantics.
        assert_eq!(cfg.per_source_cap, 3, "missing knob takes the prescribed value");
        assert!(cfg.persist);
        assert_eq!(cfg.ttl_secs, 600, "present knob wins");

        let eff = cfg.effective_sources(PRESCRIBED);
        // The params-only jira override merged in place, rest still armed.
        let jira = eff
            .iter()
            .find(|s| s.kind == "jira-assigned")
            .expect("jira-assigned present");
        assert!(jira.enabled);
        assert_eq!(
            jira.params.get("site").map(String::as_str),
            Some("acme.atlassian.net")
        );
        // Partial entry defaults: enabled defaulted true, cadence untouched.
        assert_eq!(jira.interval_secs, None);
        // The explicit disable landed.
        assert!(
            !eff.iter()
                .find(|s| s.kind == "grafana-alerts")
                .expect("grafana-alerts present")
                .enabled
        );
        // The opt-in kind rides along with its cadence override.
        let aws = eff
            .iter()
            .find(|s| s.kind == "aws-health")
            .expect("aws-health appended");
        assert!(aws.enabled);
        assert_eq!(aws.interval_secs, Some(3600));
        assert_eq!(aws.max_items, Some(2));
        // Everything else in the prescribed arm-list is still armed.
        assert_eq!(
            eff.iter().filter(|s| s.enabled).count(),
            PRESCRIBED.len() - 1 + 1, // -grafana-alerts +aws-health
        );

        // Round-trip: serialize → deserialize is identity.
        let back: BoardConfig =
            serde_yaml::from_str(&serde_yaml::to_string(&cfg).expect("serializes"))
                .expect("round-trips");
        assert_eq!(back, cfg);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // deny_unknown_fields on both structs — a typo'd operator knob is a
        // parse error, never a silently-ignored no-op.
        assert!(serde_yaml::from_str::<BoardConfig>("max_visible: 6\n").is_err());
        assert!(
            serde_yaml::from_str::<SourceEntry>("kind: jira-assigned\ncadence: 5\n").is_err()
        );
    }
}
