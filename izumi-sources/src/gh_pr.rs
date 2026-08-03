//! Shared `gh search prs` query builder + row parser.
//!
//! `github-review-requested` and `git-branch-pr` were the *same source* with
//! two characters different: the selector (`--review-requested=@me` vs
//! `--author=@me`), an emoji, and an urgency. Everything else — the argv, the
//! `GH_TOKEN` injection, the `PrRow`/`Repo` shapes, the cwd resolution, the
//! `SpawnSpec` with its `gh pr checkout <n>` kickoff, the `CorrKey`, the
//! `pr#<n> <title>` label — was duplicated byte-for-byte. Verified by
//! `diff`-ing the two `parse` bodies: the only hunks were the emoji and the
//! `Urgency`.
//!
//! That duplication was load-bearing in the wrong direction. Widening one
//! source's query without the other would have produced two sources that
//! *look* alike and answer differently, and a third PR lane (assignee,
//! `involves:@me`) would have been a third copy.
//!
//! # The selector is a parameter now
//!
//! Each source keeps its own DEFAULT selector, so an unconfigured board is
//! byte-identical to before. `params.query` overrides it, following the
//! `jira_assigned.rs` `cfg.param("jql").unwrap_or(DEFAULT_JQL)` shape — one
//! source serving review-requested / author / assignee / `involves:@me`.
//!
//! **The override is split on whitespace into separate argv entries, never
//! handed to a shell.** `Cmd` is typed argv, so a value like
//! `involves:@me draft:false` becomes two arguments and a malformed value
//! becomes a failed `gh` run — which the honesty contract already renders as
//! `Unavailable(Error)`, not as an empty-but-successful board.

use izumi::{Catalog, Cmd, CorrKey, Environment, Item, SourceConfig, SpawnSpec, Urgency};

/// Build the `gh search prs` command for a PR lane.
///
/// `default_selector` is the source's own identity (e.g.
/// `--review-requested=@me`); `params.query` replaces it wholesale.
/// `params.state` defaults to `open` — a board of merged PRs is not a board
/// of work.
pub(crate) fn search_cmd(
    default_selector: &str,
    cfg: &SourceConfig,
    env: &dyn Environment,
) -> Cmd {
    let limit = cfg.max_items.max(1).to_string();
    let selector = cfg.param("query").unwrap_or(default_selector);
    let state = cfg.param("state").unwrap_or("open");

    let mut cmd = Cmd::new("gh").arg("search").arg("prs");
    // Whitespace-split so a multi-qualifier override stays typed argv.
    // `filter(!is_empty)` so a stray double space cannot produce an empty
    // argument, which `gh` rejects with a usage error.
    for term in selector.split_whitespace().filter(|t| !t.is_empty()) {
        cmd = cmd.arg(term);
    }
    let mut state_arg = String::from("--state=");
    state_arg.push_str(state);
    cmd = cmd
        .arg(state_arg)
        .arg("--json")
        .arg("number,title,url,repository")
        .arg("--limit")
        .arg(limit);

    // A Dock-launched consumer carries no shell env, so the sops-rendered
    // token authenticates gh. The mock key is unchanged (envs are excluded
    // from `Cmd::key`), which is why widening the SELECTOR moves fixture keys
    // but adding the token does not.
    if let Some(tok) = env.secret("github/token") {
        cmd = cmd.env("GH_TOKEN", tok);
    }
    cmd
}

/// Parse `gh search prs --json …` output into items. Pure — the unit both
/// sources are tested through.
///
/// `emoji` and `urgency` are the *only* things that differed between the two
/// original copies, so they are the only parameters.
pub(crate) fn parse<K: Catalog>(
    kind: K,
    json: &str,
    env: &dyn Environment,
    emoji: &str,
    urgency: Urgency,
) -> Vec<Item<K, SpawnSpec>> {
    let Ok(rows) = serde_json::from_str::<Vec<PrRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter_map(|pr| {
            let owner = pr.repository.name_with_owner();
            let cwd = crate::util::repo_cwd(env, &owner);
            let mut name = String::from(emoji);
            name.push_str(" pr#");
            name.push_str(&pr.number.to_string());
            let checkout = {
                let mut c = String::from("gh pr checkout ");
                c.push_str(&pr.number.to_string());
                c
            };
            let spawn = SpawnSpec::new(cwd, name)?.with_command(checkout);
            let mut key = String::new();
            key.push_str(&owner);
            key.push('#');
            key.push_str(&pr.number.to_string());
            let mut title = String::from("pr#");
            title.push_str(&pr.number.to_string());
            title.push(' ');
            title.push_str(pr.title.trim());
            Some(
                Item::new(kind, &key, title, spawn)
                    .correlated(CorrKey::github(&owner, pr.number))
                    .detail(owner)
                    .urgent(urgency),
            )
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct PrRow {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    repository: Repo,
}

#[derive(serde::Deserialize, Default)]
struct Repo {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "nameWithOwner")]
    name_with_owner: String,
}

impl Repo {
    fn name_with_owner(&self) -> String {
        if self.name_with_owner.is_empty() {
            self.name.clone()
        } else {
            self.name_with_owner.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    /// The default selector must reproduce the pre-extraction argv exactly.
    /// This is the byte-compatibility gate: an unconfigured board issues the
    /// same command it always did, so every existing fixture key holds.
    #[test]
    fn default_selector_reproduces_the_original_argv() {
        let env = MockEnvironment::new();
        let cfg = SourceConfig::for_kind(TestKind::GithubReviewRequested);
        let cmd = search_cmd("--review-requested=@me", &cfg, &env);
        assert!(
            cmd.key().starts_with(
                "gh search prs --review-requested=@me --state=open \
                 --json number,title,url,repository --limit"
            ),
            "default argv must be byte-identical to the pre-extraction form: {}",
            cmd.key()
        );
    }

    /// The widening the whole extraction exists for.
    #[test]
    fn query_param_replaces_the_selector() {
        let env = MockEnvironment::new();
        let mut cfg = SourceConfig::for_kind(TestKind::GithubReviewRequested);
        cfg.params.insert("query".into(), "involves:@me".into());
        let cmd = search_cmd("--review-requested=@me", &cfg, &env);
        assert!(
            cmd.key().contains("prs involves:@me --state=open"),
            "override must replace the selector, not append to it: {}",
            cmd.key()
        );
        assert!(
            !cmd.key().contains("--review-requested"),
            "the default must be GONE, not merged: {}",
            cmd.key()
        );
    }

    /// A multi-qualifier override stays typed argv — one entry per term, so
    /// nothing is ever handed to a shell to re-split.
    #[test]
    fn multi_term_override_splits_into_separate_args() {
        let env = MockEnvironment::new();
        let mut cfg = SourceConfig::for_kind(TestKind::GithubReviewRequested);
        cfg.params
            .insert("query".into(), "involves:@me  draft:false".into());
        let cmd = search_cmd("--author=@me", &cfg, &env);
        // Double space must NOT produce an empty argument — `gh` rejects one.
        assert!(cmd.key().contains("prs involves:@me draft:false --state="));
    }

    #[test]
    fn state_is_overridable_and_defaults_to_open() {
        let env = MockEnvironment::new();
        let mut cfg = SourceConfig::for_kind(TestKind::GithubReviewRequested);
        assert!(search_cmd("--author=@me", &cfg, &env).key().contains("--state=open"));
        cfg.params.insert("state".into(), "merged".into());
        assert!(search_cmd("--author=@me", &cfg, &env).key().contains("--state=merged"));
    }

    /// The two lanes differ in exactly two values and nothing else — the
    /// claim that justified collapsing them.
    #[test]
    fn emoji_and_urgency_are_the_only_per_lane_difference() {
        let env = MockEnvironment::new().roots("/code", "/home/op");
        let json = r#"[{"number":7,"title":"t","url":"u",
                        "repository":{"name":"mado","nameWithOwner":"pleme-io/mado"}}]"#;
        let k = TestKind::GithubReviewRequested;
        let hi = parse(k, json, &env, "\u{1F50D}", Urgency::High);
        let lo = parse(k, json, &env, "\u{1F33F}", Urgency::Low);
        assert_eq!(hi.len(), 1);
        assert_eq!(hi[0].title, lo[0].title, "title is lane-independent");
        assert_eq!(hi[0].corr, lo[0].corr, "correlation is lane-independent");
        assert_ne!(hi[0].urgency, lo[0].urgency);
    }
}
