//! `github-review-requested` — PRs across the fleet that are waiting on *your*
//! review, correlated to where you'd review them. Fully local-tooling (the `gh`
//! CLI), no extra credential beyond `gh auth`. Enter spawns a session in the
//! PR's repo and kicks off `gh pr checkout <n>`.
//!
//! Live wiring: `gh search prs --review-requested=@me --state=open --json
//! number,title,url,repository --limit N`. `gh`'s `--json` output is stable +
//! documented. A review request is something blocking a teammate → High
//! urgency. Honesty contract: a failed/unauthed `gh` run is
//! `Unavailable(Error)` — only an OBSERVED run output is `Fetched` (so a
//! network blip never reads as "nobody is waiting on you").

use izumi::{Catalog, Environment, PollOutcome, Source, SourceConfig, SpawnSpec, Urgency};

pub struct GithubReviewRequested<K: Catalog> {
    kind: K,
}

impl<K: Catalog> GithubReviewRequested<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for GithubReviewRequested<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let cmd = crate::gh_pr::search_cmd(DEFAULT_SELECTOR, cfg, env);
        let Some(out) = env.run(&cmd) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(crate::gh_pr::parse(
            self.kind,
            &out,
            env,
            "\u{1F50D}", // 🔍
            Urgency::High,
        ))
        .tagged(crate::util::origin_of("github", cfg, None))
    }
}

/// This lane's identity. `params.query` overrides it — see [`crate::gh_pr`]
/// for why the selector is the only thing that varies between PR lanes.
const DEFAULT_SELECTOR: &str = "--review-requested=@me";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"number":1234,"title":"fix the parser","url":"https://x","repository":{"name":"mado","nameWithOwner":"pleme-io/mado"}},
        {"number":12,"title":"bump deps","url":"https://y","repository":{"name":"tear","nameWithOwner":"pleme-io/tear"}}
    ]"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado")
            .cmd(
                "gh search prs --review-requested=@me --state=open --json number,title,url,repository --limit 5",
                FIXTURE,
            )
    }

    #[test]
    fn produces_an_item_per_pr_with_checkout_command() {
        let cfg = SourceConfig::for_kind(TestKind::GithubReviewRequested);
        let PollOutcome::Fetched(out) =
            GithubReviewRequested::new(TestKind::GithubReviewRequested).poll(&env(), &cfg)
        else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2);
        let mado = out.iter().find(|s| s.title.contains("pr#1234")).unwrap();
        assert!(mado.title.contains("fix the parser"));
        assert_eq!(mado.spawn.cwd().to_str().unwrap(), "/code/github/pleme-io/mado");
        assert_eq!(mado.spawn.initial_command(), Some("gh pr checkout 1234"));
        assert_eq!(mado.urgency, Urgency::High);
        // The repo whose dir does not exist falls back to the code root.
        // (Match by repo detail — "pr#12" is a substring of "pr#1234".)
        let tear = out
            .iter()
            .find(|s| s.detail.as_deref() == Some("pleme-io/tear"))
            .unwrap();
        assert!(tear.title.contains("bump deps"));
        assert_eq!(tear.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (gh missing/unauthed/
        // failed) → Error, never an empty Fetched (keep last rows).
        let cfg = SourceConfig::for_kind(TestKind::GithubReviewRequested);
        assert_eq!(
            GithubReviewRequested::new(TestKind::GithubReviewRequested)
                .poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(crate::gh_pr::parse(
            TestKind::GithubReviewRequested,
            "not json",
            &MockEnvironment::new(),
            "\u{1F50D}",
            Urgency::High,
        )
        .is_empty());
    }

    /// The widening, exercised through the REAL source rather than the
    /// builder alone — a param that never reaches `poll` is a dead knob.
    #[test]
    fn query_param_widens_this_lane_end_to_end() {
        let mut cfg = SourceConfig::for_kind(TestKind::GithubReviewRequested);
        cfg.params.insert("query".into(), "involves:@me".into());
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado")
            .cmd(
                "gh search prs involves:@me --state=open --json number,title,url,repository --limit 5",
                FIXTURE,
            );
        let PollOutcome::Fetched(out) =
            GithubReviewRequested::new(TestKind::GithubReviewRequested).poll(&env, &cfg)
        else {
            panic!("the widened query must be the command actually run");
        };
        assert_eq!(out.len(), 2);
    }
}
