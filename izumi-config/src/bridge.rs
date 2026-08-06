//! The config→engine bridge — translate the typed shikumi [`BoardConfig`]
//! into an [`izumi::EngineConfig`] over a consumer's typed catalog.
//!
//! The generalized port of mado's `engine_config_from`: reads the MERGED
//! source list (prescribed arm-list ⊕ operator overrides, see
//! [`BoardConfig::effective_sources`]), so a params-only yaml override never
//! disarms the rest of the surface. Slugs resolve through the consumer's
//! [`izumi::Catalog`].
//!
//! # A slug that resolves to nothing is a FINDING, not a no-op
//!
//! This bridge used to be `if let Some(kind) = K::from_slug(&s.kind) { … }`
//! — a slug the catalog does not know simply vanished, with no error, no
//! warning and nothing to grep for. That is the worst possible shape for
//! this particular surface: a lane that produces nothing looks exactly like
//! a lane that has nothing to say, so `jira-assigne` (one missing `d`)
//! disarms a source and the board just looks quiet. The same system has
//! already lost a source for 1407 consecutive polls without anyone noticing,
//! which is what a silent lane costs in practice.
//!
//! So [`to_engine_config`] hands back a [`Resolved`], which carries the
//! armed [`izumi::EngineConfig`] AND the typed [`UnknownKind`] report, and
//! offers no way to reach the config without answering for the report. The
//! caller picks the policy:
//!
//! * [`Resolved::into_result`] — FATAL: refuse the config outright.
//! * [`Resolved::report_then_arm`] — COLLECTED: surface the bad slugs
//!   loudly, arm everything that did resolve.
//!
//! **The shipped consumers choose COLLECTED**, and deliberately: this config
//! is read by a long-running GUI (mado's Ctrl-S board) and by the
//! `izumi-board` daemon, where refusing to start over one typo'd *optional*
//! source is a bigger outage than the source it disarms. `izumi-board`'s
//! `load_board_config` already carries that doctrine in words ("the board
//! degrades, never wedges"). What was wrong was never the degrading — it was
//! degrading *in silence*.

use std::collections::BTreeMap;

use crate::BoardConfig;

/// Furthest edit distance at which a valid slug is still offered as "did you
/// mean". Chosen to be generous enough for the real operator typos this
/// exists for (`gihub-actions-failing` → `github-actions-failing` and
/// `jira-assigne` → `jira-assigned` are both distance 1) while staying below
/// the distance between any two DISTINCT valid slugs, so a suggestion can
/// never be ambiguous between two real kinds. Measured 2026-08-06 over all
/// 300 pairs of `izumi-board`'s 25-kind `BoardKind` catalog — the closest
/// valid pair is `k8s-unhealthy` ↔ `aws-health` at distance 5 — and pinned by
/// `izumi-board::config::tests::catalog_slugs_are_farther_apart_than_the_suggestion_threshold`,
/// which fails if a future kind lands a near-twin slug.
const SUGGEST_MAX_DISTANCE: usize = 3;

/// How many equally-near suggestions a single [`UnknownKind`] will carry. A
/// list longer than this stops being a hint and starts being a catalog dump.
const SUGGEST_MAX_COUNT: usize = 3;

/// One operator-authored source slug that matched no kind in the consumer's
/// catalog, and therefore armed nothing.
///
/// Reported for EVERY unresolvable entry, including one whose `enabled` is
/// `false`: a typo'd *disable* is if anything worse than a typo'd enable,
/// because the operator believes they turned a source off and it is still
/// running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKind {
    /// The slug exactly as the operator wrote it.
    pub slug: String,
    /// The closest valid catalog slugs, in catalog order, all at the same
    /// (minimal) edit distance. Empty when nothing in the catalog is within
    /// [`SUGGEST_MAX_DISTANCE`] — i.e. when this is not a plausible typo of
    /// anything and the operator meant a source that does not exist.
    pub nearest: Vec<&'static str>,
}

impl core::fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "source kind {:?} matches no catalog kind", self.slug)?;
        match self.nearest.split_first() {
            None => write!(
                f,
                " and is not a near-miss of one — check the catalog for the kind you meant"
            ),
            Some((first, rest)) => {
                write!(f, " — did you mean {first:?}")?;
                for alt in rest {
                    write!(f, " or {alt:?}")?;
                }
                write!(f, "?")
            }
        }
    }
}

/// Every unknown slug found in one [`BoardConfig`], in merged-source order —
/// the typed error a caller that wants an unknown slug to be FATAL gets from
/// [`Resolved::into_result`].
///
/// Non-empty by construction: it is only ever built from a non-empty finding
/// list, so an `Err(UnknownKinds)` always names at least one offender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKinds(Vec<UnknownKind>);

impl UnknownKinds {
    /// The findings, in merged-source order.
    #[must_use]
    pub fn entries(&self) -> &[UnknownKind] {
        &self.0
    }

    /// Just the offending slugs — the shape a structured log field wants.
    #[must_use]
    pub fn slugs(&self) -> Vec<&str> {
        self.0.iter().map(|u| u.slug.as_str()).collect()
    }
}

impl core::fmt::Display for UnknownKinds {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let n = self.0.len();
        let noun = if n == 1 { "entry" } else { "entries" };
        write!(
            f,
            "{n} configured source {noun} armed nothing (unknown kind slug): "
        )?;
        for (i, u) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{u}")?;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownKinds {}

/// The bridge's output: the armed [`izumi::EngineConfig`] plus the typed
/// report of every configured slug that armed nothing.
///
/// The engine config is deliberately NOT a public field and has no borrowing
/// accessor: the only two ways out are [`into_result`](Self::into_result)
/// (fatal) and [`report_then_arm`](Self::report_then_arm) (collected), so
/// "drop the unknown slugs and say nothing" is not something a caller can do
/// by accident — it takes writing an empty sink, in the open, on purpose.
#[derive(Debug, Clone)]
pub struct Resolved<K> {
    config: izumi::EngineConfig<K>,
    unknown: Vec<UnknownKind>,
}

impl<K: izumi::Catalog> Resolved<K> {
    /// Every configured slug that matched no catalog kind, in merged-source
    /// order. Empty on a clean config.
    #[must_use]
    pub fn unknown(&self) -> &[UnknownKind] {
        &self.unknown
    }

    /// Whether every configured slug resolved to a catalog kind.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.unknown.is_empty()
    }

    /// FATAL projection — the engine config, or the whole collected report as
    /// one typed error. For a caller that would rather refuse a config it
    /// cannot fully honour (a CI validator, a `--strict` verb, a test).
    ///
    /// # Errors
    ///
    /// [`UnknownKinds`] naming every offending slug (and its nearest valid
    /// neighbours) when any configured slug matched no catalog kind.
    pub fn into_result(self) -> Result<izumi::EngineConfig<K>, UnknownKinds> {
        if self.unknown.is_empty() {
            Ok(self.config)
        } else {
            Err(UnknownKinds(self.unknown))
        }
    }

    /// COLLECTED projection — hand the report to `sink` (a `tracing::warn!`,
    /// a stderr line, a test recorder), then arm everything that did resolve.
    /// `sink` runs ONLY when there is something to report, so a clean config
    /// stays quiet.
    ///
    /// This is the policy the shipped long-running consumers take: one typo'd
    /// optional source must not cost the operator the other twenty-four.
    #[must_use]
    pub fn report_then_arm(self, sink: impl FnOnce(&UnknownKinds)) -> izumi::EngineConfig<K> {
        if !self.unknown.is_empty() {
            sink(&UnknownKinds(self.unknown));
        }
        self.config
    }
}

/// Translate the typed board config into an [`izumi::EngineConfig`] for a
/// consumer catalog `K`. `prescribed` is the consumer's prescribed arm-list
/// (mado passes its 25-kind workflow surface; izumi-board passes its own) —
/// operator `sources` overrides merge over it by slug, and `sources_replace`
/// turns the operator list into an explicit allow-list.
///
/// Returns a [`Resolved`], not a bare config: an operator slug that matches
/// no catalog kind is collected as an [`UnknownKind`] rather than silently
/// dropped, and the caller must choose a policy for it (see the module docs).
#[must_use]
pub fn to_engine_config<K: izumi::Catalog>(cfg: &BoardConfig, prescribed: &[K]) -> Resolved<K> {
    let slugs: Vec<&str> = prescribed.iter().map(|k| k.slug()).collect();
    let mut ec = izumi::EngineConfig {
        per_source: BTreeMap::new(),
        default_enabled: cfg.default_enabled,
    };
    let mut unknown: Vec<UnknownKind> = Vec::new();
    for s in &cfg.effective_sources(&slugs) {
        let Some(kind) = K::from_slug(&s.kind) else {
            unknown.push(UnknownKind {
                slug: s.kind.clone(),
                nearest: nearest_slugs::<K>(&s.kind),
            });
            continue;
        };
        let mut sc = izumi::SourceConfig::for_kind(kind);
        sc.enabled = s.enabled;
        if let Some(iv) = s.interval_secs {
            sc.interval = std::time::Duration::from_secs(iv.max(1));
        }
        if let Some(mx) = s.max_items {
            sc.max_items = mx;
        }
        sc.params = s.params.clone();
        ec.per_source.insert(kind, sc);
    }
    Resolved {
        config: ec,
        unknown,
    }
}

/// The catalog slugs closest to `slug`, in catalog order, all sharing the
/// minimal edit distance — empty when even the closest is further than
/// [`SUGGEST_MAX_DISTANCE`].
fn nearest_slugs<K: izumi::Catalog>(slug: &str) -> Vec<&'static str> {
    let mut best = usize::MAX;
    let mut hits: Vec<&'static str> = Vec::new();
    for &kind in K::ALL {
        let candidate = kind.slug();
        let d = edit_distance(slug, candidate);
        if d < best {
            best = d;
            hits.clear();
        }
        if d == best && hits.len() < SUGGEST_MAX_COUNT {
            hits.push(candidate);
        }
    }
    if best > SUGGEST_MAX_DISTANCE {
        hits.clear();
    }
    hits
}

/// Levenshtein distance over BYTES.
///
/// Catalog slugs are ASCII kebab-case by contract, so bytes and characters
/// coincide for every valid candidate; an operator slug carrying multi-byte
/// input just measures further away, which is the safe direction (it loses
/// its suggestion, never gains a wrong one). Two-row DP, O(|a|·|b|) on
/// ≤ ~32-byte strings, run once per catalog kind per unknown slug — i.e. at
/// most a few dozen microseconds, once, at config load. Nowhere near a hot
/// path, so the simple algorithm is the right one.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur: Vec<usize> = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use izumi::Catalog as _;
    use shikumi::TieredConfig as _;

    use super::*;
    use crate::SourceEntry;

    izumi::catalog! {
        /// A stand-in consumer catalog carrying the two slugs the motivating
        /// operator typos targeted, so the near-miss assertions below are
        /// about real slugs rather than invented ones.
        pub enum TestKind {
            GitBranchPr { slug: "git-branch-pr", emoji: "\u{1F33F}", label: "git branch ↔ PR", urgency: Low, needs_auth: false, interval_secs: 30 },
            JiraAssigned { slug: "jira-assigned", emoji: "\u{1F4CB}", label: "Jira assigned", urgency: Normal, needs_auth: true, interval_secs: 300 },
            GithubActionsFailing { slug: "github-actions-failing", emoji: "\u{1F6A8}", label: "GitHub Actions failing", urgency: High, needs_auth: true, interval_secs: 180 },
            GrafanaAlerts { slug: "grafana-alerts", emoji: "\u{1F525}", label: "grafana alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
        }
    }

    fn prescribed_all() -> &'static [TestKind] {
        TestKind::ALL
    }

    /// THE REGRESSION. Before this change these two entries resolved to
    /// `None` inside `if let Some(kind) = …` and vanished: the engine config
    /// came back with the same length as a clean one, nothing was returned,
    /// nothing was logged, and the operator's board was simply missing two
    /// lanes with no string anywhere to grep for. Both projections must now
    /// name them.
    #[test]
    fn a_typod_slug_is_reported_not_silently_dropped() {
        let cfg = BoardConfig {
            sources: vec![
                SourceEntry::enable("gihub-actions-failing"),
                SourceEntry::enable("jira-assigne"),
            ],
            ..BoardConfig::prescribed_default()
        };
        let resolved = to_engine_config(&cfg, prescribed_all());

        assert!(!resolved.is_clean(), "two typo'd slugs are a finding");
        assert_eq!(
            resolved.unknown().len(),
            2,
            "every unresolvable entry is collected, not just the first"
        );

        // The typed error names each offender AND what they meant.
        let err = to_engine_config(&cfg, prescribed_all())
            .into_result()
            .expect_err("a config with unknown slugs is refused by the strict projection");
        let msg = err.to_string();
        assert!(
            msg.contains("gihub-actions-failing"),
            "names offender 1: {msg}"
        );
        assert!(
            msg.contains("github-actions-failing"),
            "suggests the fix: {msg}"
        );
        assert!(msg.contains("jira-assigne\""), "names offender 2: {msg}");
        assert!(msg.contains("jira-assigned"), "suggests the fix: {msg}");
        assert_eq!(err.slugs(), vec!["gihub-actions-failing", "jira-assigne"]);

        // The collected projection still arms everything that resolved, and
        // hands the SAME report to the sink.
        let mut seen: Vec<String> = Vec::new();
        let ec = to_engine_config(&cfg, prescribed_all()).report_then_arm(|report| {
            seen = report.slugs().iter().map(|s| (*s).to_string()).collect();
        });
        assert_eq!(seen, vec!["gihub-actions-failing", "jira-assigne"]);
        assert_eq!(
            ec.per_source.len(),
            TestKind::ALL.len(),
            "the whole prescribed catalog is still armed — one typo costs one lane, not the board"
        );
    }

    /// A typo'd DISABLE is reported too. The operator believes they turned a
    /// source off; in fact it is still polling. Silence here is the same
    /// defect wearing the opposite sign.
    #[test]
    fn a_typod_disable_is_reported_because_the_source_is_still_running() {
        let mut off = SourceEntry::enable("grafana-alert"); // singular: typo
        off.enabled = false;
        let cfg = BoardConfig {
            sources: vec![off],
            ..BoardConfig::prescribed_default()
        };
        let resolved = to_engine_config(&cfg, prescribed_all());
        assert_eq!(resolved.unknown().len(), 1);
        assert_eq!(resolved.unknown()[0].nearest, vec!["grafana-alerts"]);
        let ec = resolved.report_then_arm(|_| {});
        assert!(
            ec.config_for(TestKind::GrafanaAlerts).enabled,
            "the real source is still armed — which is exactly why the typo must be surfaced"
        );
    }

    /// The failure mode to avoid: breaking every existing config in the name
    /// of catching typos. A valid config round-trips to byte-identical engine
    /// config and reports nothing.
    #[test]
    fn a_valid_config_still_round_trips_and_reports_nothing() {
        let mut over = SourceEntry::enable(TestKind::JiraAssigned.slug());
        over.interval_secs = Some(600);
        over.max_items = Some(2);
        over.params
            .insert(String::from("site"), String::from("acme.atlassian.net"));
        let mut off = SourceEntry::enable(TestKind::GitBranchPr.slug());
        off.enabled = false;
        let cfg = BoardConfig {
            sources: vec![over, off],
            ..BoardConfig::prescribed_default()
        };

        let resolved = to_engine_config(&cfg, prescribed_all());
        assert!(resolved.is_clean(), "no finding on a valid config");
        assert!(resolved.unknown().is_empty());

        // The strict projection accepts it, and the sink never fires.
        let ec = to_engine_config(&cfg, prescribed_all())
            .into_result()
            .expect("a valid config resolves cleanly");
        let mut fired = false;
        let ec2 = to_engine_config(&cfg, prescribed_all()).report_then_arm(|_| fired = true);
        assert!(!fired, "a clean config stays quiet");

        // …and both projections produce the SAME config the old silent bridge
        // produced: every prescribed kind armed, overrides landed verbatim.
        for ec in [&ec, &ec2] {
            assert_eq!(ec.per_source.len(), TestKind::ALL.len());
            let jira = ec.config_for(TestKind::JiraAssigned);
            assert!(jira.enabled);
            assert_eq!(jira.interval, std::time::Duration::from_secs(600));
            assert_eq!(jira.max_items, 2);
            assert_eq!(jira.param("site"), Some("acme.atlassian.net"));
            assert!(!ec.config_for(TestKind::GitBranchPr).enabled);
            assert!(ec.config_for(TestKind::GrafanaAlerts).enabled);
        }
    }

    /// `sources_replace` (the allow-list escape hatch) resolves through the
    /// same gate — a typo there disarms EVERYTHING that isn't listed, so it
    /// is the shape where a silent drop hurt most.
    #[test]
    fn replace_mode_reports_its_typos_too() {
        let cfg = BoardConfig {
            sources: vec![SourceEntry::enable("grafana-alerst")],
            sources_replace: true,
            ..BoardConfig::prescribed_default()
        };
        let resolved = to_engine_config(&cfg, prescribed_all());
        assert_eq!(resolved.unknown().len(), 1);
        assert_eq!(resolved.unknown()[0].nearest, vec!["grafana-alerts"]);
        let ec = resolved.report_then_arm(|_| {});
        assert!(
            ec.per_source.is_empty(),
            "the allow-list armed literally nothing — the report is the only signal there is"
        );
    }

    /// A slug that is not a near-miss of anything says so plainly instead of
    /// offering a bogus "did you mean". Distance from `zzzzzzzzzzzz` to every
    /// test-catalog slug is far past the threshold.
    #[test]
    fn a_wholly_bogus_slug_gets_no_misleading_suggestion() {
        let cfg = BoardConfig {
            sources: vec![SourceEntry::enable("zzzzzzzzzzzz")],
            ..BoardConfig::prescribed_default()
        };
        let resolved = to_engine_config(&cfg, prescribed_all());
        let finding = &resolved.unknown()[0];
        assert!(finding.nearest.is_empty(), "no plausible near-miss");
        let msg = finding.to_string();
        assert!(msg.contains("zzzzzzzzzzzz"), "still names it: {msg}");
        assert!(
            msg.contains("not a near-miss"),
            "says why there is no suggestion: {msg}"
        );
    }

    /// The distance function itself, pinned on the two motivating typos plus
    /// the degenerate cases the DP has to get right.
    #[test]
    fn edit_distance_is_levenshtein() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        // The two operator typos this whole change exists for.
        assert_eq!(
            edit_distance("gihub-actions-failing", "github-actions-failing"),
            1
        );
        assert_eq!(edit_distance("jira-assigne", "jira-assigned"), 1);
        // Transposition costs 2 under plain Levenshtein — still inside the
        // suggestion threshold, which is why the threshold is 3 and not 1.
        assert_eq!(edit_distance("grafana-alerst", "grafana-alerts"), 2);
    }

    /// The suggestion is never ambiguous between two REAL kinds: no two
    /// distinct slugs in this catalog are within the threshold of each other.
    /// (The same invariant is pinned against the real 25-kind `BoardKind`
    /// catalog in `izumi-board`.)
    #[test]
    fn no_two_valid_slugs_are_within_the_suggestion_threshold() {
        let slugs: Vec<&str> = TestKind::ALL.iter().map(|k| k.slug()).collect();
        for (i, a) in slugs.iter().enumerate() {
            for b in &slugs[i + 1..] {
                assert!(
                    edit_distance(a, b) > SUGGEST_MAX_DISTANCE,
                    "{a} and {b} are within {SUGGEST_MAX_DISTANCE} edits — a suggestion could be ambiguous"
                );
            }
        }
    }
}
