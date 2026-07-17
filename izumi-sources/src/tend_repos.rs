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

use izumi::{Catalog, Cmd, Environment, Item, PollOutcome, SourceConfig, Source, SpawnSpec, Urgency};

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
        // legitimately runs ~25-30s — declare it Slow so it doesn't time out
        // under the default 20s cap and error() on every poll (the "ever_ok
        // false for 200+ polls" failure this seals).
        let Some(out) = env.run(&Cmd::new("tend").arg("status").arg("--json").slow()) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(self.kind, &out, env))
    }
}

/// Parse `tend status --json` into items for non-clean repos.
fn parse<K: Catalog>(kind: K, json: &str, env: &dyn Environment) -> Vec<Item<K, SpawnSpec>> {
    let Ok(rows) = serde_json::from_str::<Vec<RepoRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter(|r| !r.state.eq_ignore_ascii_case("clean") && !r.state.is_empty())
        .filter_map(|r| {
            let missing = r.state.eq_ignore_ascii_case("missing") || r.path.is_empty();
            let stuck = r.state.eq_ignore_ascii_case("stuck");
            let icon = if stuck { "\u{1F6A7} " } else { "\u{1F9F9} " }; // 🚧 vs 🧹
            let mut name = String::from(icon);
            name.push_str(&r.name);
            let spawn = if missing {
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
            let urgency = match r.state.to_ascii_lowercase().as_str() {
                "stuck" => Urgency::High,
                "missing" => Urgency::Normal,
                _ => Urgency::Low,
            };
            let mut title = r.name.clone();
            title.push_str(" — ");
            title.push_str(&r.state);
            Some(
                Item::new(kind, &r.name, title, spawn)
                    .detail(r.state)
                    .urgent(urgency),
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
}
