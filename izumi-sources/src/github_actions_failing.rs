//! `github-actions-failing` — failed CI runs in the current repo, surfaced as
//! "go look at this red build" items. Local CLI, no auth beyond `gh
//! auth`. Enter spawns a session and tails the failed logs via `gh run view
//! <id> --log-failed`.
//!
//! Live wiring: `gh run list --json
//! databaseId,displayTitle,workflowName,headBranch,conclusion,status,createdAt
//! --limit N`. Note `gh run list` is cwd-repo-scoped by default — it reports
//! the runs of whatever repo the consumer is sitting in; the optional `repos`
//! param (comma-separated `owner/name` list) widens the poll to a fixed fleet
//! via `--repo`. A red build on `main`/`master` is fleet-blocking → Critical;
//! other branches stay High. Honesty contract: a failed/unauthed `gh` run is
//! `Unavailable(Error)` (in fleet mode: only if EVERY repo's run failed) —
//! only an OBSERVED run output is `Fetched` (so a network blip never reads as
//! "CI is green").
//!
//! # Currency: this asks "is it red NOW", not "did it ever fail"
//!
//! The query deliberately carries NO `--status=failure`. A completed run's
//! conclusion is immutable, and `gh run list --status=failure` has no recency
//! bound and no notion of the workflow's current head state — so a failure
//! that was fixed minutes later keeps being returned indefinitely. Because
//! the source kept re-listing those runs, the store's `last_seen_ms` was
//! refreshed on every poll and its decay pass could never fire, so resolved
//! failures sat on the board for weeks (observed 2026-07-25: all five rows
//! this source produced were superseded by a later SUCCESS of the same
//! workflow on the same branch — every one of them phantom).
//!
//! So the source scans recent runs of every status and applies
//! [`still_red`]: per `(workflow, branch)`, only the NEWEST run decides. A
//! resolved failure is dropped at the border, which means the row leaves the
//! board on the next poll through the ordinary ingest-vanish path — no new
//! store machinery, and the provider's own poll IS the proof of currency.

use std::collections::BTreeMap;

use izumi::{Catalog, Cmd, Environment, Item, PollOutcome, Rank, SourceConfig, Source, SpawnSpec, Urgency};

pub struct GithubActionsFailing<K: Catalog> {
    kind: K,
}

impl<K: Catalog> GithubActionsFailing<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for GithubActionsFailing<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let limit = scan_limit(cfg).to_string();
        let Some(repos) = cfg.param("repos") else {
            // Default: cwd-repo-scoped, today's behavior.
            let Some(out) = env.run(&run_list_cmd(env, &limit, None)) else {
                return PollOutcome::error();
            };
            return PollOutcome::Fetched(parse(self.kind, &out, env, cfg));
        };
        // Fleet mode: poll each named repo and merge. ALL-OR-NOTHING: ingest
        // replaces this source's whole store slice, so a partial merge would
        // silently WIPE the rows of every repo whose gh call failed — the
        // exact unobserved-path wipe the PollOutcome border exists to
        // prevent, scoped per-repo. One failed repo = Error (keep every
        // last-known row; health shows erroring) rather than a partial truth.
        let names: Vec<&str> = repos
            .split(',')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .collect();
        if names.is_empty() {
            // A present-but-empty `repos` param is a config mistake, not a
            // fetch failure.
            return PollOutcome::unconfigured();
        }
        let mut merged = Vec::new();
        for repo in names {
            let Some(out) = env.run(&run_list_cmd(env, &limit, Some(repo))) else {
                return PollOutcome::error();
            };
            merged.extend(parse(self.kind, &out, env, cfg));
        }
        PollOutcome::Fetched(merged)
    }
}

/// How many recent runs to SCAN, which is deliberately not `max_items`.
///
/// `max_items` bounds how many red builds we may DISPLAY; the scan has to be
/// wide enough to observe the newest run of every workflow+branch pair that
/// could be red, because without `--status=failure` the window is shared by
/// successes, tag runs and in-flight runs. A repo with several workflows
/// (mado has `auto-release` on `main` plus `release-binaries` per tag) burns
/// the window fast, and a pair whose newest run falls outside it is simply
/// invisible — a false GREEN, the one error direction this source must not
/// make. So scan generously and truncate late. Overridable via the
/// `scan_limit` param; floored at 30 so a small `max_items` can never
/// narrow the evidence window.
fn scan_limit(cfg: &SourceConfig) -> usize {
    cfg.param("scan_limit")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or_else(|| cfg.max_items.saturating_mul(6))
        .max(30)
}

/// Build the `gh run list` invocation — cwd-repo-scoped by default,
/// `--repo`-scoped in fleet mode.
///
/// Deliberately NOT `--status=failure`. That filter asks a HISTORICAL
/// question ("did a run ever fail?"), and a completed run's conclusion is
/// immutable, so a failure that has since been fixed keeps being returned
/// forever — the store re-ingests it every poll, `last_seen_ms` never goes
/// stale, and decay can never fire. We instead scan the recent runs of
/// EVERY status and decide currency ourselves in [`parse`]: a workflow is
/// red only if its NEWEST run is a completed failure.
fn run_list_cmd(env: &dyn Environment, limit: &str, repo: Option<&str>) -> Cmd {
    let mut c = Cmd::new("gh").arg("run").arg("list");
    if let Some(r) = repo {
        c = c.arg("--repo").arg(r);
    }
    c = c
        .arg("--json")
        .arg("databaseId,displayTitle,workflowName,headBranch,conclusion,status,createdAt")
        .arg("--limit")
        .arg(limit);
    // A Dock-launched consumer carries no shell env, so the sops-rendered
    // token authenticates gh. The mock key is unchanged (envs are excluded
    // from Cmd::key).
    if let Some(tok) = env.secret("github/token") {
        c = c.env("GH_TOKEN", tok);
    }
    c
}

/// Reduce a scan of recent runs to the workflows that are STILL RED.
///
/// Grouping is **per workflow**, and only that workflow's NEWEST run
/// decides: a completed failure means the workflow is red right now; a
/// success, or a still-running run, means it is not, and every older
/// failure is resolved history that must not reach the board. `createdAt`
/// is RFC3339 UTC, which sorts lexicographically in chronological order,
/// so "newest" needs no date parsing.
///
/// # Why not group by `(workflow, branch)`
///
/// That was the first shape, and live data refuted it. A tag-triggered
/// workflow gets a DISTINCT `headBranch` per release (`v0.1.90`,
/// `v0.1.91`, …), and a tag ref is immutable and single-shot — so each
/// tag's one run is trivially "newest for its pair" and can never be
/// superseded. Grouping per pair therefore pinned every historical failed
/// release to the board forever: measured against pleme-io/mado on
/// 2026-07-25, it turned 13 rows into 8, of which 6 were ancient tag runs
/// (v0.1.87…v0.1.92) that will never re-run. Per-workflow grouping answers
/// the question an operator actually acts on — "is the release pipeline
/// broken *now*" — and yields 2 genuinely-red rows for the same input.
///
/// The cost, stated plainly: a failure on one ref is masked by a newer run
/// of the same workflow on another ref, so a red feature branch goes quiet
/// once anything newer runs. That is the right trade for a board whose
/// complaint was week-old phantom rows, but it IS a loss of per-branch
/// signal, not a free win. `headBranch` survives as the row's detail, so
/// which ref is red stays visible.
///
/// Pure — the unit the currency rule is tested through.
fn still_red(rows: Vec<RunRow>) -> Vec<RunRow> {
    let mut newest: BTreeMap<String, RunRow> = BTreeMap::new();
    for run in rows {
        match newest.get(&run.workflow_name) {
            Some(seen) if seen.created_at >= run.created_at => {}
            _ => {
                newest.insert(run.workflow_name.clone(), run);
            }
        }
    }
    newest
        .into_values()
        // `status == "completed"` gates on a settled run: an in-progress or
        // queued newest run means "we do not yet know", which is not a red
        // build. `conclusion` is null until completion, so this also keeps a
        // null from ever reading as a failure.
        .filter(|r| r.status == "completed" && r.conclusion == "failure")
        .collect()
}

/// Parse `gh run list --json …` output into items. Pure — the unit the
/// source is tested through.
fn parse<K: Catalog>(
    kind: K,
    json: &str,
    env: &dyn Environment,
    cfg: &SourceConfig,
) -> Vec<Item<K, SpawnSpec>> {
    let Ok(rows) = serde_json::from_str::<Vec<RunRow>>(json) else {
        return Vec::new();
    };
    let cwd = env.code_root();
    still_red(rows)
        .into_iter()
        .take(cfg.max_items.max(1))
        .filter_map(|run| {
            let spawn = {
                let mut view = String::from("gh run view ");
                view.push_str(&run.database_id.to_string());
                view.push_str(" --log-failed");
                SpawnSpec::new(cwd.clone(), "\u{1F6A8} CI")?.with_command(view) // 🚨
            };
            let mut title = run.workflow_name.clone();
            title.push_str(" failed: ");
            let display: String = run.display_title.chars().take(120).collect();
            title.push_str(display.trim());
            let key = run.database_id.to_string();
            // A red build on the default branch blocks everyone → Critical;
            // a feature-branch failure keeps the should-look-soon High tier.
            let s = Item::new(kind, &key, title, spawn);
            let s = if run.head_branch == "main" || run.head_branch == "master" {
                s.ranked(Rank::critical())
            } else {
                s.urgent(Urgency::High)
            };
            Some(s.detail(run.head_branch))
        })
        .collect()
}

#[derive(serde::Deserialize, Default, Clone)]
struct RunRow {
    #[serde(default, rename = "databaseId")]
    database_id: u64,
    #[serde(default, rename = "displayTitle")]
    display_title: String,
    #[serde(default, rename = "workflowName")]
    workflow_name: String,
    #[serde(default, rename = "headBranch")]
    head_branch: String,
    /// `"failure"` / `"success"` / `"cancelled"` / … — null (hence empty
    /// via `default`) while a run is still in flight.
    #[serde(default)]
    conclusion: String,
    /// `"completed"` / `"in_progress"` / `"queued"`.
    #[serde(default)]
    status: String,
    /// RFC3339 UTC — lexicographic order IS chronological order.
    #[serde(default, rename = "createdAt")]
    created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    /// Two workflow+branch pairs, each currently red (one completed
    /// failure per pair, nothing newer).
    const FIXTURE: &str = r#"[
        {"databaseId":98765,"displayTitle":"fix the parser","workflowName":"CI","headBranch":"main","conclusion":"failure","status":"completed","createdAt":"2026-07-25T10:00:00Z"},
        {"databaseId":98766,"displayTitle":"bump deps","workflowName":"release","headBranch":"feat/x","conclusion":"failure","status":"completed","createdAt":"2026-07-25T09:00:00Z"}
    ]"#;

    /// The REAL shape that shipped the bug: an older failure on
    /// `(auto-release, main)` superseded by a NEWER success of the same
    /// pair. Verbatim from the operator's board — the three `auto-release
    /// failed` rows were all of this shape.
    const SUPERSEDED_FIXTURE: &str = r#"[
        {"databaseId":29951892447,"displayTitle":"release: workspace v0.1.92","workflowName":"auto-release","headBranch":"main","conclusion":"success","status":"completed","createdAt":"2026-07-22T19:39:39Z"},
        {"databaseId":29951204728,"displayTitle":"fix: never split a box-drawing run","workflowName":"auto-release","headBranch":"main","conclusion":"failure","status":"completed","createdAt":"2026-07-22T19:29:53Z"},
        {"databaseId":29949899528,"displayTitle":"fix: re-apply native window styling","workflowName":"auto-release","headBranch":"main","conclusion":"failure","status":"completed","createdAt":"2026-07-22T19:11:11Z"}
    ]"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new().roots("/code", "/home/op").cmd(
            "gh run list --json databaseId,displayTitle,workflowName,headBranch,conclusion,status,createdAt --limit 30",
            FIXTURE,
        )
    }

    #[test]
    fn produces_an_item_per_failing_run_with_log_command() {
        let cfg = SourceConfig::for_kind(TestKind::GithubActionsFailing);
        let PollOutcome::Fetched(out) =
            GithubActionsFailing::new(TestKind::GithubActionsFailing).poll(&env(), &cfg)
        else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 2);
        let first = out.iter().find(|s| s.title.contains("fix the parser")).unwrap();
        assert!(first.title.contains("CI failed:"));
        assert_eq!(first.detail.as_deref(), Some("main"));
        // A red build on the default branch blocks everyone → Critical.
        assert_eq!(first.urgency, Urgency::Critical);
        // `gh run list` is cwd-repo-scoped → spawn into the code root.
        assert_eq!(first.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(
            first.spawn.initial_command(),
            Some("gh run view 98765 --log-failed")
        );
        let second = out.iter().find(|s| s.title.contains("bump deps")).unwrap();
        assert!(second.title.contains("release failed:"));
        assert_eq!(second.detail.as_deref(), Some("feat/x"));
        // A feature-branch failure keeps the High tier.
        assert_eq!(second.urgency, Urgency::High);
        assert!(
            first.rank_key() > second.rank_key(),
            "the default-branch failure must rank above the feature-branch one"
        );
    }

    #[test]
    fn repos_param_is_all_or_nothing() {
        // Fleet mode is ALL-OR-NOTHING: ingest replaces the source's whole
        // store slice, so a partial merge would wipe the failed repos' rows.
        // One repo answering while the other fails → Error (keep last rows).
        let mut cfg = SourceConfig::for_kind(TestKind::GithubActionsFailing);
        cfg.params.insert(
            "repos".to_string(),
            "pleme-io/mado,pleme-io/tear".to_string(),
        );
        let partial = MockEnvironment::new().roots("/code", "/home/op").cmd(
            "gh run list --repo pleme-io/mado --json databaseId,displayTitle,workflowName,headBranch,conclusion,status,createdAt --limit 30",
            FIXTURE,
        );
        assert_eq!(
            GithubActionsFailing::new(TestKind::GithubActionsFailing).poll(&partial, &cfg),
            PollOutcome::error(),
            "a partially-observed fleet must not become a partial truth"
        );
        // EVERY repo answering → Fetched(merged).
        let full = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd(
                "gh run list --repo pleme-io/mado --json databaseId,displayTitle,workflowName,headBranch,conclusion,status,createdAt --limit 30",
                FIXTURE,
            )
            .cmd(
                "gh run list --repo pleme-io/tear --json databaseId,displayTitle,workflowName,headBranch,conclusion,status,createdAt --limit 30",
                "[]",
            );
        let PollOutcome::Fetched(out) =
            GithubActionsFailing::new(TestKind::GithubActionsFailing).poll(&full, &cfg)
        else {
            panic!("all repos observed → Fetched");
        };
        assert_eq!(out.len(), 2, "merged across the fleet");
        // EVERY repo's run failing → Error (the fleet was not observed).
        let dead = MockEnvironment::new();
        assert_eq!(
            GithubActionsFailing::new(TestKind::GithubActionsFailing).poll(&dead, &cfg),
            PollOutcome::error()
        );
    }

    /// THE REGRESSION: a failure superseded by a later SUCCESS of the same
    /// workflow+branch must NOT reach the board.
    ///
    /// This is the exact shape that put three week-old `auto-release failed`
    /// rows on the operator's board: `gh run list --status=failure` kept
    /// returning immutable historical failures, so the store's
    /// `last_seen_ms` was refreshed every poll and decay could never fire.
    /// Verified live 2026-07-25 against pleme-io/mado — all five rows this
    /// source produced were of this shape.
    #[test]
    fn a_failure_superseded_by_a_later_success_is_dropped() {
        let cfg = SourceConfig::for_kind(TestKind::GithubActionsFailing);
        let env = MockEnvironment::new().roots("/code", "/home/op").cmd(
            "gh run list --json databaseId,displayTitle,workflowName,headBranch,conclusion,status,createdAt --limit 30",
            SUPERSEDED_FIXTURE,
        );
        let PollOutcome::Fetched(out) =
            GithubActionsFailing::new(TestKind::GithubActionsFailing).poll(&env, &cfg)
        else {
            panic!("an observed run is Fetched");
        };
        assert!(
            out.is_empty(),
            "(auto-release, main)'s newest run SUCCEEDED — both older failures \
             are resolved history and must not reach the board; got {:?}",
            out.iter().map(|s| s.title.clone()).collect::<Vec<_>>()
        );
    }

    /// The currency rule, at the pure-function border. Each case is one
    /// `(workflow, branch)` group; only the newest run may speak for it.
    #[test]
    fn still_red_keeps_only_a_currently_failing_head() {
        let row = |wf: &str, br: &str, concl: &str, status: &str, at: &str| RunRow {
            database_id: 1,
            display_title: "t".into(),
            workflow_name: wf.into(),
            head_branch: br.into(),
            conclusion: concl.into(),
            status: status.into(),
            created_at: at.into(),
        };
        // Newest failed → red. Order in the input must not matter, so the
        // older success is listed AFTER the newer failure here.
        let red = still_red(vec![
            row("ci", "main", "failure", "completed", "2026-07-25T10:00:00Z"),
            row("ci", "main", "success", "completed", "2026-07-25T09:00:00Z"),
        ]);
        assert_eq!(red.len(), 1, "a currently-red head is reported");

        // Newest succeeded → not red, regardless of how many older failures.
        let green = still_red(vec![
            row("ci", "main", "success", "completed", "2026-07-25T10:00:00Z"),
            row("ci", "main", "failure", "completed", "2026-07-25T09:00:00Z"),
            row("ci", "main", "failure", "completed", "2026-07-25T08:00:00Z"),
        ]);
        assert!(green.is_empty(), "a fixed build is not red");

        // Newest still running → unknown, NOT red (conclusion is null).
        let running = still_red(vec![
            row("ci", "main", "", "in_progress", "2026-07-25T10:00:00Z"),
            row("ci", "main", "failure", "completed", "2026-07-25T09:00:00Z"),
        ]);
        assert!(running.is_empty(), "an in-flight head is not a red build");

        // Grouping is PER WORKFLOW, so a newer run on another ref settles
        // the workflow — the documented cost of not grouping per branch.
        let masked = still_red(vec![
            row("ci", "main", "success", "completed", "2026-07-25T10:00:00Z"),
            row("ci", "feat/x", "failure", "completed", "2026-07-25T09:00:00Z"),
        ]);
        assert!(
            masked.is_empty(),
            "a newer run of the same workflow settles it, whatever the ref"
        );

        // Workflows remain independent of each other.
        let per_workflow = still_red(vec![
            row("ci", "main", "success", "completed", "2026-07-25T10:00:00Z"),
            row("release", "main", "failure", "completed", "2026-07-25T09:00:00Z"),
        ]);
        assert_eq!(per_workflow.len(), 1);
        assert_eq!(per_workflow[0].workflow_name, "release");
    }

    /// THE TAG EXPLOSION: per-`(workflow, branch)` grouping pinned every
    /// historical failed release to the board forever, because a tag ref is
    /// immutable and single-shot so each tag's lone run is always "newest"
    /// for its own pair. Measured against pleme-io/mado on 2026-07-25: six
    /// ancient tag runs (v0.1.87…v0.1.92) that will never re-run. Per-workflow
    /// grouping collapses them to the workflow's current state.
    #[test]
    fn historical_failed_tag_releases_do_not_accumulate() {
        let tag_run = |tag: &str, concl: &str, at: &str| RunRow {
            database_id: 1,
            display_title: "release".into(),
            workflow_name: "release-binaries".into(),
            head_branch: tag.into(),
            conclusion: concl.into(),
            status: "completed".into(),
            created_at: at.into(),
        };
        let out = still_red(vec![
            tag_run("v0.1.87", "failure", "2026-07-18T00:00:00Z"),
            tag_run("v0.1.88", "failure", "2026-07-20T00:00:00Z"),
            tag_run("v0.1.89", "failure", "2026-07-20T01:00:00Z"),
            tag_run("v0.1.90", "failure", "2026-07-22T00:00:00Z"),
            tag_run("v0.1.91", "failure", "2026-07-22T01:00:00Z"),
            tag_run("v0.1.92", "failure", "2026-07-22T02:00:00Z"),
        ]);
        assert_eq!(
            out.len(),
            1,
            "one workflow is red once, not once per historical tag"
        );
        assert_eq!(out[0].head_branch, "v0.1.92", "the NEWEST run represents it");

        // And once the pipeline is fixed, the whole pile goes quiet.
        let fixed = still_red(vec![
            tag_run("v0.1.91", "failure", "2026-07-22T01:00:00Z"),
            tag_run("v0.1.92", "failure", "2026-07-22T02:00:00Z"),
            tag_run("v0.1.93", "success", "2026-07-26T00:00:00Z"),
        ]);
        assert!(fixed.is_empty(), "a successful newer release clears the pile");
    }

    /// The scan window must never narrow to `max_items`: a pair whose newest
    /// run falls outside the window is invisible, which is a false GREEN.
    #[test]
    fn scan_limit_is_floored_and_independent_of_max_items() {
        let mut cfg = SourceConfig::for_kind(TestKind::GithubActionsFailing);
        cfg.max_items = 1;
        assert_eq!(scan_limit(&cfg), 30, "a tiny max_items cannot narrow the scan");
        cfg.max_items = 20;
        assert_eq!(scan_limit(&cfg), 120, "a large max_items widens it");
        cfg.params.insert("scan_limit".into(), "75".into());
        assert_eq!(scan_limit(&cfg), 75, "an explicit override wins");
        cfg.params.insert("scan_limit".into(), "2".into());
        assert_eq!(scan_limit(&cfg), 30, "even an override respects the floor");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None (gh missing/unauthed/
        // failed) → Error, never an empty Fetched (keep last rows).
        let cfg = SourceConfig::for_kind(TestKind::GithubActionsFailing);
        assert_eq!(
            GithubActionsFailing::new(TestKind::GithubActionsFailing)
                .poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
        // A present-but-empty `repos` param is a config mistake, not a fetch
        // failure → Unconfigured.
        let mut cfg = SourceConfig::for_kind(TestKind::GithubActionsFailing);
        cfg.params.insert("repos".to_string(), " , ".to_string());
        assert_eq!(
            GithubActionsFailing::new(TestKind::GithubActionsFailing)
                .poll(&MockEnvironment::new(), &cfg),
            PollOutcome::unconfigured()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        let cfg = SourceConfig::for_kind(TestKind::GithubActionsFailing);
        assert!(
            parse(TestKind::GithubActionsFailing, "not json", &MockEnvironment::new(), &cfg)
                .is_empty()
        );
    }
}
