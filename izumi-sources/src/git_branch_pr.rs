//! `git-branch-pr` — your open PRs across the fleet, correlated to where you'd
//! work on them. Fully local-tooling (the `gh` CLI), no extra credential
//! beyond `gh auth`. Enter spawns a session in the PR's repo and kicks off
//! `gh pr checkout <n>`.
//!
//! Live wiring: `gh search prs --author=@me --state=open --json
//! number,title,url,repository --limit N`. `gh`'s `--json` output is stable +
//! documented. Honesty contract: a failed/unauthed `gh` run is
//! `Unavailable(Error)` — only an OBSERVED run output is `Fetched` (so a
//! network blip never reads as "all your PRs merged").

use izumi::{Catalog, Environment, PollOutcome, Source, SourceConfig, SpawnSpec, Urgency};

pub struct GitBranchPr<K: Catalog> {
    kind: K,
}

impl<K: Catalog> GitBranchPr<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for GitBranchPr<K> {
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
            "\u{1F33F}", // 🌿
            Urgency::Low,
        ))
    }
}

/// This lane's identity. `params.query` overrides it — see [`crate::gh_pr`].
const DEFAULT_SELECTOR: &str = "--author=@me";

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
                "gh search prs --author=@me --state=open --json number,title,url,repository --limit 5",
                FIXTURE,
            )
    }

    #[test]
    fn produces_an_item_per_pr_with_checkout_command() {
        let cfg = SourceConfig::for_kind(TestKind::GitBranchPr);
        let PollOutcome::Fetched(out) = GitBranchPr::new(TestKind::GitBranchPr).poll(&env(), &cfg)
        else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2);
        let mado = out.iter().find(|s| s.title.contains("pr#1234")).unwrap();
        assert!(mado.title.contains("fix the parser"));
        assert_eq!(mado.spawn.cwd().to_str().unwrap(), "/code/github/pleme-io/mado");
        assert_eq!(mado.spawn.initial_command(), Some("gh pr checkout 1234"));
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
        let cfg = SourceConfig::for_kind(TestKind::GitBranchPr);
        assert_eq!(
            GitBranchPr::new(TestKind::GitBranchPr).poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(crate::gh_pr::parse(TestKind::GitBranchPr, "not json", &MockEnvironment::new(), "\u{1F33F}", Urgency::Low).is_empty());
    }
}
