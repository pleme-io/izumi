//! `tend-repos` — workspace repos that need attention (dirty / stuck /
//! missing / unknown), surfaced as "go tidy this" items. Local CLI, no auth.
//!
//! Live wiring: `tend status --json` → an array of `{name, path, state}`. A
//! repo whose state is not `clean` becomes an item whose spawn drops you
//! in that repo's directory. Honesty contract: a failed/absent `tend` run is
//! `Unavailable(Error)` — only an OBSERVED run output is `Fetched` (so a
//! tooling blip never reads as "every repo clean").
//!
//! `stuck` (mid rebase/merge/cherry-pick) ranks `High`, distinct from the
//! `Low` given to routine `dirty` drift — a stuck repo can silently strand
//! real committed and uncommitted work under a conflict for weeks with no
//! other signal (the incident this distinction exists for: a rebase
//! abandoned 2026-05-29, found 2026-07-09, six weeks with zero visibility).
//!
//! # The severity ladder is the PRODUCER's, not ours
//!
//! `tend` ranks its own states, and this consumer mirrors that order rather
//! than inventing a second one. `no-remote` is the *most* severe state tend
//! can emit — above `stuck`, above `dirty` — because a stuck or dirty repo
//! still has its committed history backed up somewhere and a remote-less one
//! does not (tend `sync.rs:567`, and it paints `!R` red bold with
//! "<- history exists on this machine only" at `display.rs:82`). So it lands
//! `Critical` here, the only tier that keeps it above `stuck`'s `High`.
//!
//! # Why a typed vocabulary and not one more `match` arm
//!
//! This source used to rank straight off the raw string with a `_ => Low`
//! catch-all. That is a trap, not a bug: `no-remote` was born in tend
//! `8d9a305` and arrived here as the *quietest* rank on the board under a
//! 🧹 tidy-up broom — the single most severe thing tend can say, filed as
//! the least urgent thing izumi can show. The arm was never wrong when it
//! was written; it silently became wrong when the producer grew.
//!
//! [`TendRepoState`] closes the *class*: every state tend emits today is a
//! named variant, and anything else parses to [`TendRepoState::Unrecognized`]
//! — which ranks `High` and says so on its face. A newly-added producer
//! state now ANNOUNCES itself at the top of the board instead of sinking
//! into the quiet bucket. Same convention as
//! [`IncidentSeverity::Unknown`](crate::util::IncidentSeverity), which keeps
//! a firing-but-unlabeled alert urgent for exactly this reason.
//!
//! **Tier (do not round up): `only-mitigated`.** This is a loud runtime
//! default, not unrepresentability. The genuinely-typed fix — share tend's
//! `RepoStatus` so a new variant is a compile error — is *unavailable*, not
//! merely unchosen: `pleme-tend` is a binary-only crate (`[[bin]]`, no
//! `src/lib.rs`) and `RepoStatus` is `pub(crate)`, so there is no library
//! target to depend on. It would take a tend-side change (lib target +
//! public enum + a shared wire crate) before izumi could reach
//! `truly-unrepresentable` here.
//!
//! **Known residual, same class, deliberately not fixed here:** `RepoRow`'s
//! fields are `#[serde(default)]`, so if tend ever *renames* the `state`
//! field every row silently parses to an empty state and is filtered out.
//! That is a schema break rather than a new variant, and making it loud
//! would emit one alarm per repo; it is named here rather than papered over.

use izumi::{
    Catalog, Cmd, Environment, Item, PollOutcome, RunOutcome, Source, SourceConfig, SpawnSpec,
    Urgency,
};

/// A repo state as reported by `tend status --json`, parsed into a closed
/// vocabulary so ranking is an exhaustive match instead of a hand-kept
/// string table with a quiet default.
///
/// The variants mirror tend's own `RepoStatus` (`sync.rs:74`) one-for-one.
/// [`Unrecognized`](Self::Unrecognized) is NOT one of them — it is this
/// consumer's typed admission that the producer said something it has not
/// been taught yet, and it is deliberately loud.
///
/// Note the distinction [`Unknown`](Self::Unknown) vs
/// [`Unrecognized`](Self::Unrecognized): `unknown` is a real, *known* tend
/// state (a repo on disk that config does not declare — minor config
/// drift); `Unrecognized` means izumi could not name the string at all.
/// Collapsing those two would rebuild the very trap this type exists to
/// close, so they never share a variant.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TendRepoState {
    /// Clean *and* observed to have a remote to be clean relative to.
    Clean,
    /// Uncommitted changes in the working tree.
    Dirty,
    /// Mid rebase / merge / cherry-pick — real work may be stranded.
    Stuck,
    /// **No git remote at all** — the history exists on exactly one machine
    /// and has never been pushed. tend's most severe verdict.
    NoRemote,
    /// Expected by config but not cloned yet.
    Missing,
    /// On disk but not declared in config.
    Unknown,
    /// A state string this consumer does not know — carries the raw text so
    /// the board can show the operator exactly what the producer said.
    Unrecognized(String),
}

impl TendRepoState {
    /// Parse a `tend status --json` state string (case-insensitive).
    ///
    /// Anything unmatched becomes [`Unrecognized`](Self::Unrecognized)
    /// rather than silently folding into a calm known variant.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "clean" => Self::Clean,
            "dirty" => Self::Dirty,
            "stuck" => Self::Stuck,
            "no-remote" => Self::NoRemote,
            "missing" => Self::Missing,
            "unknown" => Self::Unknown,
            _ => Self::Unrecognized(raw.trim().to_string()),
        }
    }

    /// Where this state sits on the board.
    ///
    /// Mirrors tend's own ordering: `NoRemote` > `Stuck` > `Missing` >
    /// `Dirty`. An [`Unrecognized`](Self::Unrecognized) state ranks `High`
    /// — loud enough to be seen immediately, but deliberately *not* above
    /// `NoRemote`, since an unnamed state has not earned the top of the
    /// ladder over the worst state we actually understand.
    #[must_use]
    pub fn urgency(&self) -> Urgency {
        match self {
            Self::NoRemote => Urgency::Critical,
            Self::Stuck | Self::Unrecognized(_) => Urgency::High,
            Self::Missing => Urgency::Normal,
            Self::Dirty | Self::Unknown | Self::Clean => Urgency::Low,
        }
    }

    /// The board glyph. The load-bearing property is that the two alarm
    /// states never wear the 🧹 tidy-up broom, which reads as "housekeeping"
    /// — the exact misread that hid `no-remote`.
    #[must_use]
    pub fn icon(&self) -> &'static str {
        match self {
            // 🚨 unbacked history — the alarm.
            Self::NoRemote => "\u{1F6A8} ",
            // ⚠️ izumi does not know this state; the producer moved.
            Self::Unrecognized(_) => "\u{26A0}\u{FE0F} ",
            // 🚧 stranded work.
            Self::Stuck => "\u{1F6A7} ",
            // 🧹 routine tidy-up.
            Self::Clean | Self::Dirty | Self::Missing | Self::Unknown => "\u{1F9F9} ",
        }
    }

    /// Whether this state is worth a board item at all. Only `Clean` is not.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        !matches!(self, Self::Clean)
    }

    /// Board text for this state — the raw producer word, plus an explicit
    /// callout when izumi could not name it, so an unrecognized state is
    /// self-describing on the row rather than only implied by its rank.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Clean => "clean".to_string(),
            Self::Dirty => "dirty".to_string(),
            Self::Stuck => "stuck".to_string(),
            Self::NoRemote => "no-remote — history exists on this machine only".to_string(),
            Self::Missing => "missing".to_string(),
            Self::Unknown => "unknown".to_string(),
            Self::Unrecognized(raw) => {
                let mut s = raw.clone();
                s.push_str(" — unrecognized tend state, izumi needs updating");
                s
            }
        }
    }
}

pub struct TendRepos<K: Catalog> {
    kind: K,
}

impl<K: Catalog> TendRepos<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for TendRepos<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, _cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        // `tend status` scans every workspace repo (100+ git statuses) and
        // legitimately runs ~25-30s — declare it Slow (60s cap) so it doesn't
        // time out under the default 20s cap. And read the TYPED outcome so a
        // timeout reports as TimedOut ("slow — raise the budget"), never the
        // misleading Error ("broken"): the "ever_ok false for 200+ polls, looks
        // broken but is only slow" failure this seals.
        let cmd = Cmd::new("tend").arg("status").arg("--json").slow();
        match env.run_outcome(&cmd) {
            RunOutcome::Produced(out) => PollOutcome::Fetched(parse(self.kind, &out, env)),
            RunOutcome::TimedOut(_) => PollOutcome::timed_out(),
            RunOutcome::Failed => PollOutcome::error(),
        }
    }
}

/// Parse `tend status --json` into items for non-clean repos.
fn parse<K: Catalog>(kind: K, json: &str, env: &dyn Environment) -> Vec<Item<K, SpawnSpec>> {
    let Ok(rows) = serde_json::from_str::<Vec<RepoRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        // An empty state is a malformed/absent field, not a new producer
        // state — see the module-doc residual note.
        .filter(|r| !r.state.trim().is_empty())
        .filter_map(|r| {
            let state = TendRepoState::parse(&r.state);
            if !state.is_actionable() {
                return None;
            }
            // A repo with no path on disk needs cloning regardless of which
            // state it reports (preserves the original `|| r.path.is_empty()`).
            let needs_clone = state == TendRepoState::Missing || r.path.is_empty();
            let mut name = String::from(state.icon());
            name.push_str(&r.name);
            let spawn = if needs_clone {
                if r.name.is_empty() {
                    return None;
                }
                // A missing repo has no directory yet — the bare name is not
                // a cwd. Seat the session at the code root and kick off the
                // clone via tend.
                let mut sync = String::from("tend sync ");
                sync.push_str(&r.name);
                SpawnSpec::new(env.code_root(), name)?.with_command(sync)
            } else {
                SpawnSpec::new(r.path.clone(), name)?
            };
            let label = state.label();
            let mut title = r.name.clone();
            title.push_str(" — ");
            title.push_str(&label);
            Some(
                Item::new(kind, &r.name, title, spawn)
                    .detail(label)
                    .urgent(state.urgency()),
            )
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct RepoRow {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    state: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"name":"mado","path":"/code/github/pleme-io/mado","state":"dirty"},
        {"name":"tear","path":"/code/github/pleme-io/tear","state":"clean"},
        {"name":"newrepo","path":"","state":"missing"}
    ]"#;

    const STUCK_FIXTURE: &str = r#"[
        {"name":"engenho-promessa-controllers","path":"/code/github/pleme-io/engenho-promessa-controllers","state":"stuck"},
        {"name":"mado","path":"/code/github/pleme-io/mado","state":"dirty"}
    ]"#;

    #[test]
    fn surfaces_only_non_clean_repos() {
        let env = MockEnvironment::new().cmd("tend status --json", FIXTURE);
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let PollOutcome::Fetched(out) = TendRepos::new(TestKind::TendRepos).poll(&env, &cfg) else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2, "clean repo excluded");
        let dirty = out.iter().find(|s| s.title.contains("mado")).unwrap();
        assert!(dirty.title.contains("dirty"));
        // A dirty repo keeps its real directory as the spawn target.
        assert_eq!(dirty.spawn.cwd().to_str().unwrap(), "/code/github/pleme-io/mado");
        let missing = out.iter().find(|s| s.title.contains("newrepo")).unwrap();
        assert_eq!(missing.urgency, Urgency::Normal);
        // A missing repo seats you at the code root and kicks off the clone
        // (the bare name is not a cwd).
        assert_eq!(missing.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(missing.spawn.initial_command(), Some("tend sync newrepo"));
    }

    #[test]
    fn stuck_repo_ranks_above_routine_dirty() {
        let env = MockEnvironment::new().cmd("tend status --json", STUCK_FIXTURE);
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let PollOutcome::Fetched(out) = TendRepos::new(TestKind::TendRepos).poll(&env, &cfg) else {
            panic!("an observed run is Fetched");
        };
        let stuck = out
            .iter()
            .find(|s| s.title.contains("engenho-promessa-controllers"))
            .unwrap();
        assert_eq!(stuck.urgency, Urgency::High, "a stuck repo must outrank routine dirty drift");
        assert!(stuck.title.contains("stuck"));
        let dirty = out.iter().find(|s| s.title.contains("mado")).unwrap();
        assert_eq!(dirty.urgency, Urgency::Low);
        assert!(stuck.urgency > dirty.urgency, "stuck must rank above dirty");
    }

    /// A repo whose whole history exists on one disk is the MOST severe
    /// thing tend can report (`sync.rs:567`) — it must outrank `stuck`, and
    /// it must not wear the 🧹 tidy-up broom.
    ///
    /// Written to fail against the pre-fix code, which sank `no-remote`
    /// into the `_ => Low` catch-all at the same rank as `dirty`.
    #[test]
    fn no_remote_outranks_everything_matching_the_producer() {
        const FIX: &str = r#"[
            {"name":"ferrite-zig","path":"/code/github/pleme-io/ferrite-zig","state":"no-remote"},
            {"name":"engenho","path":"/code/github/pleme-io/engenho","state":"stuck"},
            {"name":"mado","path":"/code/github/pleme-io/mado","state":"dirty"}
        ]"#;
        let env = MockEnvironment::new().cmd("tend status --json", FIX);
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let PollOutcome::Fetched(out) = TendRepos::new(TestKind::TendRepos).poll(&env, &cfg) else {
            panic!("an observed run is Fetched");
        };
        let no_remote = out.iter().find(|s| s.title.contains("ferrite-zig")).unwrap();
        let stuck = out.iter().find(|s| s.title.contains("engenho")).unwrap();
        let dirty = out.iter().find(|s| s.title.contains("mado")).unwrap();

        assert_eq!(
            no_remote.urgency,
            Urgency::Critical,
            "an unbacked repo is the producer's most severe state"
        );
        assert!(
            no_remote.urgency > stuck.urgency && no_remote.urgency > dirty.urgency,
            "no-remote must outrank stuck AND dirty, as tend ranks it"
        );
        assert_ne!(
            no_remote.urgency,
            Urgency::Low,
            "the exact inversion this test exists for"
        );
        // The glyph must read as an alarm, never as housekeeping.
        assert!(
            !no_remote.spawn.name().contains('\u{1F9F9}'),
            "no-remote must not wear the 🧹 tidy-up broom"
        );
        assert!(no_remote.spawn.name().contains('\u{1F6A8}'), "🚨 expected");
        assert!(
            no_remote.title.contains("this machine only"),
            "the row must say what is actually wrong"
        );
    }

    /// ★ The load-bearing property: a state the producer invents TOMORROW
    /// cannot arrive quietly. Feed a string that exists in no arm and
    /// confirm it surfaces loudly instead of sinking to `Low`.
    #[test]
    fn an_unrecognized_state_announces_itself_never_sinks_to_low() {
        const FIX: &str = r#"[
            {"name":"futurerepo","path":"/code/github/pleme-io/futurerepo","state":"totally-new-state"},
            {"name":"mado","path":"/code/github/pleme-io/mado","state":"dirty"}
        ]"#;
        let env = MockEnvironment::new().cmd("tend status --json", FIX);
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let PollOutcome::Fetched(out) = TendRepos::new(TestKind::TendRepos).poll(&env, &cfg) else {
            panic!("an observed run is Fetched");
        };
        let novel = out
            .iter()
            .find(|s| s.title.contains("futurerepo"))
            .expect("an unknown state must still become a board item, never be dropped");

        assert_eq!(
            novel.urgency,
            Urgency::High,
            "an unrecognized producer state must be loud"
        );
        assert_ne!(
            novel.urgency,
            Urgency::Low,
            "the quiet bucket is what let no-remote hide for a whole release"
        );
        let dirty = out.iter().find(|s| s.title.contains("mado")).unwrap();
        assert!(
            novel.urgency > dirty.urgency,
            "an unknown state must outrank routine dirty drift"
        );
        // ...but it must NOT claim to be worse than the worst state we know.
        assert!(
            novel.urgency < Urgency::Critical,
            "an unnamed state has not earned the top of the ladder over no-remote"
        );
        // It must say WHY it is loud, on its face.
        assert!(novel.title.contains("totally-new-state"), "shows the raw word");
        assert!(
            novel.title.contains("unrecognized"),
            "the row must name itself as unrecognized so the operator knows izumi is behind"
        );
        assert!(novel.spawn.name().contains('\u{26A0}'), "⚠️ expected");
        assert!(
            !novel.spawn.name().contains('\u{1F9F9}'),
            "an unknown state must not wear the tidy-up broom"
        );
    }

    /// Forcing function: pin the FULL known vocabulary to its rank in one
    /// table. A future edit that re-ranks a state, or that folds a new
    /// producer word into a calm variant, has to come through here.
    #[test]
    fn every_known_state_keeps_its_rank_and_unknown_words_stay_loud() {
        // (producer word, expected urgency)
        const MATRIX: &[(&str, Urgency)] = &[
            ("clean", Urgency::Low), // filtered before ranking; pinned for completeness
            ("dirty", Urgency::Low),
            ("unknown", Urgency::Low),
            ("missing", Urgency::Normal),
            ("stuck", Urgency::High),
            ("no-remote", Urgency::Critical),
        ];
        let mut failures = Vec::new();
        for (word, want) in MATRIX {
            let got = TendRepoState::parse(word).urgency();
            if got != *want {
                failures.push(format!("{word}: want {want:?}, got {got:?}"));
            }
            assert!(
                !matches!(TendRepoState::parse(word), TendRepoState::Unrecognized(_)),
                "{word} is a real tend state and must parse to a named variant"
            );
        }
        assert!(failures.is_empty(), "rank drift:\n  - {}", failures.join("\n  - "));

        // Case-insensitivity + whitespace, matching the old `to_ascii_lowercase` read.
        assert_eq!(TendRepoState::parse("NO-REMOTE"), TendRepoState::NoRemote);
        assert_eq!(TendRepoState::parse("  Stuck "), TendRepoState::Stuck);

        // Anything outside the table is Unrecognized and loud — including
        // near-misses that must NOT be charitably folded into a calm state.
        for novel in ["totally-new-state", "no_remote", "detached-head", "ahead"] {
            assert_eq!(
                TendRepoState::parse(novel),
                TendRepoState::Unrecognized(novel.to_string()),
                "{novel} must not be silently absorbed by a known variant"
            );
            assert_eq!(TendRepoState::parse(novel).urgency(), Urgency::High);
        }

        // Only `clean` is non-actionable; every other state earns a row.
        assert!(!TendRepoState::Clean.is_actionable());
        for s in [
            TendRepoState::Dirty,
            TendRepoState::Stuck,
            TendRepoState::NoRemote,
            TendRepoState::Missing,
            TendRepoState::Unknown,
            TendRepoState::Unrecognized("x".to_string()),
        ] {
            assert!(s.is_actionable(), "{s:?} must reach the board");
        }
    }

    /// The states that existed before this change must be byte-for-byte
    /// unchanged — a fix that quietly re-ranks `dirty` is its own defect.
    #[test]
    fn pre_existing_states_are_unchanged() {
        const FIX: &str = r#"[
            {"name":"a","path":"/code/a","state":"stuck"},
            {"name":"b","path":"/code/b","state":"dirty"},
            {"name":"c","path":"","state":"missing"},
            {"name":"d","path":"/code/d","state":"clean"},
            {"name":"e","path":"/code/e","state":"unknown"}
        ]"#;
        let env = MockEnvironment::new().cmd("tend status --json", FIX);
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let PollOutcome::Fetched(out) = TendRepos::new(TestKind::TendRepos).poll(&env, &cfg) else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 4, "clean is still the only filtered state");
        let by = |n: &str| out.iter().find(|s| s.title.starts_with(n)).unwrap().urgency;
        assert_eq!(by("a"), Urgency::High, "stuck unchanged");
        assert_eq!(by("b"), Urgency::Low, "dirty unchanged");
        assert_eq!(by("c"), Urgency::Normal, "missing unchanged");
        assert_eq!(by("e"), Urgency::Low, "unknown unchanged (was the catch-all)");
        assert!(out.iter().all(|s| !s.title.starts_with('d')), "clean excluded");
        // The pre-existing glyph split (🚧 stuck vs 🧹 everything calm) holds.
        let stuck = out.iter().find(|s| s.title.starts_with('a')).unwrap();
        assert!(stuck.spawn.name().contains('\u{1F6A7}'));
        let dirty = out.iter().find(|s| s.title.starts_with('b')).unwrap();
        assert!(dirty.spawn.name().contains('\u{1F9F9}'));
        // A missing repo still routes to the clone spawn, not a bare cwd.
        let missing = out.iter().find(|s| s.title.starts_with('c')).unwrap();
        assert_eq!(missing.spawn.initial_command(), Some("tend sync c"));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (tend missing/failed) →
        // Error, never an empty Fetched (keep last rows).
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        assert_eq!(
            TendRepos::new(TestKind::TendRepos).poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn a_slow_tend_reports_timed_out_not_error() {
        // The seal end-to-end: a tend that TIMED OUT (slow, but alive) reads as
        // TimedOut ("raise the budget"), NEVER the broken Error — the "erroring
        // for 200+ polls, looks broken but is only slow" failure this seals.
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let env = MockEnvironment::new().cmd_timed_out("tend status --json");
        let outcome = TendRepos::new(TestKind::TendRepos).poll(&env, &cfg);
        assert_eq!(outcome, PollOutcome::timed_out());
        assert_ne!(outcome, PollOutcome::error(), "a timeout is NOT the broken Error");
    }
}
