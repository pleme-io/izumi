//! izumi-sources — the generic provider catalog: 25 data-source providers
//! ported from mado's Ctrl-S suggestion plane, each generic over ANY
//! [`izumi::Catalog`] kind enum.
//!
//! Where the mado originals were hard-wired to mado's `SourceKind`, every
//! provider here carries its kind as a value: construct it with
//! `Provider::new(MyKind::Whatever)` and it implements
//! `izumi::Source<MyKind, izumi::SpawnSpec>`. Fetch/parse logic, constants,
//! honesty tiers, ranking ladders, and doc contracts are verbatim from mado —
//! this crate is a GENERALIZATION, not a rewrite.
//!
//! * **Local CLI providers** (no auth): [`git_branch_pr`], [`tend_repos`],
//!   [`cargo_warnings`], [`todo_backlog`], [`secret_age`].
//! * **GitHub** (via the `gh` CLI): [`github_review_requested`],
//!   [`github_assigned_issues`], [`github_actions_failing`].
//! * **Atlassian** (HTTP Basic): [`jira_sprint`], [`jira_assigned`],
//!   [`confluence_mentions`].
//! * **Cluster / `GitOps`** (via `kubectl`): [`flux_failing`], [`k8s_unhealthy`],
//!   [`breathe_conflict`], [`engenho_nodes`].
//! * **Observability / incidents** (HTTP): [`grafana_alerts`],
//!   [`grafana_incidents`], [`grafana_oncall`], [`datadog_monitors`],
//!   [`opsgenie_alerts`].
//! * **Agents / cloud**: [`kurage_agents`], [`aws_health`],
//!   [`cloudflare_deployments`].
//! * **Calendar / tasks** (HTTP): [`google_tasks`], [`google_calendar`].
//!
//! Shared provider primitives (priority scales, percent-encoding, JSON
//! helpers, the workspace-convention cwd resolver) live in [`util`].

#![forbid(unsafe_code)]

pub mod aws_health;
pub mod breathe_conflict;
pub mod cargo_warnings;
pub mod cloudflare_deployments;
pub mod confluence_mentions;
pub mod datadog_monitors;
pub mod engenho_nodes;
pub mod flux_failing;
pub mod git_branch_pr;
pub mod github_actions_failing;
pub mod github_assigned_issues;
pub mod github_review_requested;
pub mod google_calendar;
pub mod google_tasks;
pub mod grafana_alerts;
pub mod grafana_incidents;
pub mod grafana_oncall;
pub mod jira_assigned;
pub mod jira_sprint;
pub mod k8s_unhealthy;
pub mod kurage_agents;
pub mod opsgenie_alerts;
pub mod secret_age;
pub mod tend_repos;
pub mod todo_backlog;
pub mod util;

pub use aws_health::AwsHealth;
pub use breathe_conflict::BreatheConflict;
pub use cargo_warnings::CargoWarnings;
pub use cloudflare_deployments::CloudflareDeployments;
pub use confluence_mentions::ConfluenceMentions;
pub use datadog_monitors::DatadogMonitors;
pub use engenho_nodes::EngenhoNodes;
pub use flux_failing::FluxFailing;
pub use git_branch_pr::GitBranchPr;
pub use github_actions_failing::GithubActionsFailing;
pub use github_assigned_issues::GithubAssignedIssues;
pub use github_review_requested::GithubReviewRequested;
pub use google_calendar::GoogleCalendar;
pub use google_tasks::GoogleTasks;
pub use grafana_alerts::GrafanaAlerts;
pub use grafana_incidents::GrafanaIncidents;
pub use grafana_oncall::GrafanaOncall;
pub use jira_assigned::JiraAssigned;
pub use jira_sprint::JiraSprint;
pub use k8s_unhealthy::K8sUnhealthy;
pub use kurage_agents::KurageAgents;
pub use opsgenie_alerts::OpsgenieAlerts;
pub use secret_age::SecretAge;
pub use tend_repos::TendRepos;
pub use todo_backlog::TodoBacklog;

/// The registry invariant helper consumers run in their tests: assert a
/// consumer's `Vec<Arc<dyn Source>>` registry covers `expected` exactly, with
/// no kind registered twice. Panics with a labeled message on any mismatch —
/// the mechanical proof that the consumer's catalog can never claim a source
/// its engine can't actually run (CLOSED-LOOP MASS-SYNTHESIS rule 1, ported
/// from the mado registry test).
///
/// # Panics
///
/// * a kind is registered more than once (named by slug);
/// * an `expected` kind has no registered provider (all named by slug);
/// * a registered kind is not in `expected` (all named by slug).
pub fn assert_registry_wiring<K: izumi::Catalog, A: izumi::Payload>(
    sources: &[std::sync::Arc<dyn izumi::Source<K, A>>],
    expected: &[K],
) {
    let mut kinds: Vec<K> = sources.iter().map(|s| s.kind()).collect();
    kinds.sort_unstable();
    for w in kinds.windows(2) {
        assert!(
            w[0] != w[1],
            "registry wiring: source kind {:?} ({}) is registered twice",
            w[0],
            w[0].slug()
        );
    }
    let registered: std::collections::BTreeSet<K> = kinds.into_iter().collect();
    let wanted: std::collections::BTreeSet<K> = expected.iter().copied().collect();
    let missing: Vec<&str> = wanted
        .iter()
        .filter(|k| !registered.contains(k))
        .map(|k| k.slug())
        .collect();
    assert!(
        missing.is_empty(),
        "registry wiring: {} expected kind(s) have no registered provider: {}",
        missing.len(),
        missing.join(", ")
    );
    let extra: Vec<&str> = registered
        .iter()
        .filter(|k| !wanted.contains(k))
        .map(|k| k.slug())
        .collect();
    assert!(
        extra.is_empty(),
        "registry wiring: {} registered kind(s) are not expected: {}",
        extra.len(),
        extra.join(", ")
    );
}

/// The in-crate test catalog — one variant per ported provider, with slugs /
/// urgencies / auth flags / cadences EXACTLY matching the mado `SourceKind`
/// table entries, so the fnv1a-derived id expectations and rank expectations
/// of the ported test suites are unchanged from mado.
#[cfg(test)]
pub(crate) mod testkit {
    izumi::catalog! {
        /// The test catalog (the 25-provider subset of mado's `SourceKind`).
        pub enum TestKind {
            /// Local git branches correlated to their open PR titles.
            GitBranchPr { slug: "git-branch-pr", emoji: "\u{1F33F}", label: "git branch ↔ PR", urgency: Low, needs_auth: false, interval_secs: 30 },
            /// `tend` workspace repos that are dirty / unsynced / missing.
            TendRepos { slug: "tend-repos", emoji: "\u{1F9F9}", label: "tend dirty repos", urgency: Low, needs_auth: false, interval_secs: 30 },
            /// `cargo` warnings/errors in the current project.
            CargoWarnings { slug: "cargo-warnings", emoji: "\u{1F980}", label: "cargo warnings", urgency: Low, needs_auth: false, interval_secs: 120 },
            /// `TODO` / `FIXME` backlog under the code root.
            TodoBacklog { slug: "todo-backlog", emoji: "\u{1F4DD}", label: "TODO backlog", urgency: Idle, needs_auth: false, interval_secs: 120 },
            /// PRs awaiting your review.
            GithubReviewRequested { slug: "github-review-requested", emoji: "\u{1F50D}", label: "GitHub review-requested", urgency: High, needs_auth: true, interval_secs: 180 },
            /// Issues assigned to you.
            GithubAssignedIssues { slug: "github-assigned-issues", emoji: "\u{1F41B}", label: "GitHub assigned issues", urgency: Normal, needs_auth: true, interval_secs: 180 },
            /// GitHub Actions runs that are failing.
            GithubActionsFailing { slug: "github-actions-failing", emoji: "\u{1F6A8}", label: "GitHub Actions failing", urgency: High, needs_auth: true, interval_secs: 180 },
            /// Jira issues in your active sprint.
            JiraSprint { slug: "jira-sprint", emoji: "\u{1F3AB}", label: "Jira sprint", urgency: Normal, needs_auth: true, interval_secs: 300 },
            /// Jira issues assigned to you.
            JiraAssigned { slug: "jira-assigned", emoji: "\u{1F4CB}", label: "Jira assigned", urgency: Normal, needs_auth: true, interval_secs: 300 },
            /// Confluence pages mentioning you.
            ConfluenceMentions { slug: "confluence-mentions", emoji: "\u{1F4AC}", label: "Confluence mentions", urgency: Low, needs_auth: true, interval_secs: 300 },
            /// FluxCD Kustomizations/HelmReleases failing to reconcile.
            FluxFailing { slug: "flux-failing", emoji: "\u{1F501}", label: "Flux failing", urgency: High, needs_auth: false, interval_secs: 60 },
            /// Kubernetes pods Pending / CrashLoopBackOff / unhealthy.
            K8sUnhealthy { slug: "k8s-unhealthy", emoji: "\u{2638}", label: "k8s unhealthy pods", urgency: Critical, needs_auth: false, interval_secs: 60 },
            /// `breathe` resource bands stuck in Conflict.
            BreatheConflict { slug: "breathe-conflict", emoji: "\u{1F4A8}", label: "breathe Conflict bands", urgency: High, needs_auth: false, interval_secs: 60 },
            /// engenho cluster nodes not Ready.
            EngenhoNodes { slug: "engenho-nodes", emoji: "\u{1F5A5}", label: "engenho nodes", urgency: Normal, needs_auth: false, interval_secs: 60 },
            /// grafana alerts firing.
            GrafanaAlerts { slug: "grafana-alerts", emoji: "\u{1F525}", label: "grafana alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// grafana incidents open.
            GrafanaIncidents { slug: "grafana-incidents", emoji: "\u{1F6A9}", label: "grafana incidents", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// grafana on-call shifts assigned to you.
            GrafanaOncall { slug: "grafana-oncall", emoji: "\u{1F4DF}", label: "grafana on-call", urgency: High, needs_auth: true, interval_secs: 600 },
            /// Datadog monitors alerting.
            DatadogMonitors { slug: "datadog-monitors", emoji: "\u{1F415}", label: "Datadog monitors", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// Opsgenie alerts open/unacked.
            OpsgenieAlerts { slug: "opsgenie-alerts", emoji: "\u{1F514}", label: "Opsgenie alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// Cursor cloud (kurage) agents needing follow-up.
            KurageAgents { slug: "kurage-agents", emoji: "\u{1F916}", label: "Cursor agents", urgency: Normal, needs_auth: true, interval_secs: 120 },
            /// AWS health/PHD events affecting your account.
            AwsHealth { slug: "aws-health", emoji: "\u{2601}", label: "AWS health", urgency: Critical, needs_auth: true, interval_secs: 300 },
            /// Cloudflare Pages/Workers deployments that failed.
            CloudflareDeployments { slug: "cloudflare-deployments", emoji: "\u{1F310}", label: "Cloudflare deployments", urgency: High, needs_auth: true, interval_secs: 300 },
            /// Google Tasks due soon.
            GoogleTasks { slug: "google-tasks", emoji: "\u{2705}", label: "Google Tasks", urgency: Low, needs_auth: true, interval_secs: 300 },
            /// Google Calendar events imminent.
            GoogleCalendar { slug: "google-calendar", emoji: "\u{1F4C5}", label: "Google Calendar", urgency: Normal, needs_auth: true, interval_secs: 300 },
            /// Secrets whose age exceeds a rotation threshold.
            SecretAge { slug: "secret-age", emoji: "\u{1F511}", label: "secret age", urgency: Normal, needs_auth: false, interval_secs: 3600 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::TestKind;
    use izumi::{Catalog, Environment, PollOutcome, Source, SourceConfig, SpawnSpec};
    use std::sync::Arc;

    /// The registry helper accepts a complete, duplicate-free wiring.
    #[test]
    fn registry_wiring_accepts_a_complete_registry() {
        let sources: Vec<Arc<dyn Source<TestKind, SpawnSpec>>> = vec![
            Arc::new(crate::GitBranchPr::new(TestKind::GitBranchPr)),
            Arc::new(crate::TendRepos::new(TestKind::TendRepos)),
        ];
        crate::assert_registry_wiring(&sources, &[TestKind::GitBranchPr, TestKind::TendRepos]);
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn registry_wiring_rejects_duplicates() {
        let sources: Vec<Arc<dyn Source<TestKind, SpawnSpec>>> = vec![
            Arc::new(crate::TendRepos::new(TestKind::TendRepos)),
            Arc::new(crate::TendRepos::new(TestKind::TendRepos)),
        ];
        crate::assert_registry_wiring(&sources, &[TestKind::TendRepos]);
    }

    #[test]
    #[should_panic(expected = "no registered provider")]
    fn registry_wiring_rejects_a_missing_provider() {
        let sources: Vec<Arc<dyn Source<TestKind, SpawnSpec>>> =
            vec![Arc::new(crate::TendRepos::new(TestKind::TendRepos))];
        crate::assert_registry_wiring(&sources, &[TestKind::TendRepos, TestKind::GitBranchPr]);
    }

    #[test]
    #[should_panic(expected = "not expected")]
    fn registry_wiring_rejects_an_unexpected_provider() {
        let sources: Vec<Arc<dyn Source<TestKind, SpawnSpec>>> = vec![
            Arc::new(crate::TendRepos::new(TestKind::TendRepos)),
            Arc::new(crate::GitBranchPr::new(TestKind::GitBranchPr)),
        ];
        crate::assert_registry_wiring(&sources, &[TestKind::TendRepos]);
    }

    /// Every provider reports the kind it was constructed with — the one
    /// invariant the generic `kind: K` field carries.
    #[test]
    fn every_provider_reports_its_constructed_kind() {
        let pairs: Vec<(Arc<dyn Source<TestKind, SpawnSpec>>, TestKind)> = vec![
            (Arc::new(crate::GitBranchPr::new(TestKind::GitBranchPr)), TestKind::GitBranchPr),
            (Arc::new(crate::TendRepos::new(TestKind::TendRepos)), TestKind::TendRepos),
            (Arc::new(crate::CargoWarnings::new(TestKind::CargoWarnings)), TestKind::CargoWarnings),
            (Arc::new(crate::TodoBacklog::new(TestKind::TodoBacklog)), TestKind::TodoBacklog),
            (
                Arc::new(crate::GithubReviewRequested::new(TestKind::GithubReviewRequested)),
                TestKind::GithubReviewRequested,
            ),
            (
                Arc::new(crate::GithubAssignedIssues::new(TestKind::GithubAssignedIssues)),
                TestKind::GithubAssignedIssues,
            ),
            (
                Arc::new(crate::GithubActionsFailing::new(TestKind::GithubActionsFailing)),
                TestKind::GithubActionsFailing,
            ),
            (Arc::new(crate::JiraSprint::new(TestKind::JiraSprint)), TestKind::JiraSprint),
            (Arc::new(crate::JiraAssigned::new(TestKind::JiraAssigned)), TestKind::JiraAssigned),
            (
                Arc::new(crate::ConfluenceMentions::new(TestKind::ConfluenceMentions)),
                TestKind::ConfluenceMentions,
            ),
            (Arc::new(crate::FluxFailing::new(TestKind::FluxFailing)), TestKind::FluxFailing),
            (Arc::new(crate::K8sUnhealthy::new(TestKind::K8sUnhealthy)), TestKind::K8sUnhealthy),
            (
                Arc::new(crate::BreatheConflict::new(TestKind::BreatheConflict)),
                TestKind::BreatheConflict,
            ),
            (Arc::new(crate::EngenhoNodes::new(TestKind::EngenhoNodes)), TestKind::EngenhoNodes),
            (Arc::new(crate::GrafanaAlerts::new(TestKind::GrafanaAlerts)), TestKind::GrafanaAlerts),
            (
                Arc::new(crate::GrafanaIncidents::new(TestKind::GrafanaIncidents)),
                TestKind::GrafanaIncidents,
            ),
            (Arc::new(crate::GrafanaOncall::new(TestKind::GrafanaOncall)), TestKind::GrafanaOncall),
            (
                Arc::new(crate::DatadogMonitors::new(TestKind::DatadogMonitors)),
                TestKind::DatadogMonitors,
            ),
            (
                Arc::new(crate::OpsgenieAlerts::new(TestKind::OpsgenieAlerts)),
                TestKind::OpsgenieAlerts,
            ),
            (Arc::new(crate::KurageAgents::new(TestKind::KurageAgents)), TestKind::KurageAgents),
            (Arc::new(crate::AwsHealth::new(TestKind::AwsHealth)), TestKind::AwsHealth),
            (
                Arc::new(crate::CloudflareDeployments::new(TestKind::CloudflareDeployments)),
                TestKind::CloudflareDeployments,
            ),
            (Arc::new(crate::GoogleTasks::new(TestKind::GoogleTasks)), TestKind::GoogleTasks),
            (
                Arc::new(crate::GoogleCalendar::new(TestKind::GoogleCalendar)),
                TestKind::GoogleCalendar,
            ),
            (Arc::new(crate::SecretAge::new(TestKind::SecretAge)), TestKind::SecretAge),
        ];
        assert_eq!(pairs.len(), 25, "one pair per ported provider");
        for (src, kind) in &pairs {
            assert_eq!(src.kind(), *kind);
        }
        let sources: Vec<Arc<dyn Source<TestKind, SpawnSpec>>> =
            pairs.iter().map(|(s, _)| Arc::clone(s)).collect();
        crate::assert_registry_wiring(&sources, TestKind::ALL);
    }

    /// Item ids derived through the test catalog match the mado derivation
    /// byte-for-byte (slug ':' key through fnv1a) — the wire-compat anchor
    /// for every ported id expectation.
    #[test]
    fn item_ids_match_the_mado_derivation() {
        let id = izumi::ItemId::derive(TestKind::TendRepos, "mado");
        assert_eq!(id, izumi::ItemId::derive_slug("tend-repos", "mado"));
        let mut buf = String::from("tend-repos");
        buf.push(':');
        buf.push_str("mado");
        assert_eq!(id.0, izumi::fnv1a(buf.as_bytes()));
    }

    /// A poll through a provider flows into the shared store sink exactly as
    /// the mado engine drove it (`refresh_once` = poll → ingest).
    #[test]
    fn refresh_once_drives_a_ported_provider_end_to_end() {
        let env = izumi::MockEnvironment::new().cmd(
            "tend status --json",
            r#"[{"name":"mado","path":"/code/github/pleme-io/mado","state":"dirty"}]"#,
        );
        let store = izumi::Store::new();
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        let source = crate::TendRepos::new(TestKind::TendRepos);
        izumi::refresh_once(&source, &env, &store, &cfg, 1000);
        assert_eq!(store.len(), 1);
        assert!(store.ranked(10, 1000).iter().any(|s| s.title.contains("mado")));
    }

    /// A provider polled through `&dyn Environment` honors the honesty tiers.
    #[test]
    fn dyn_environment_polling_is_object_safe() {
        let env: Box<dyn Environment> = Box::new(izumi::MockEnvironment::new());
        let cfg = SourceConfig::for_kind(TestKind::TendRepos);
        assert_eq!(
            crate::TendRepos::new(TestKind::TendRepos).poll(env.as_ref(), &cfg),
            PollOutcome::error()
        );
    }
}
