//! The board's source-kind catalog — the 25 generic kinds this consumer
//! arms, authored through [`izumi::catalog!`] (dogfooding the macro: slug
//! uniqueness is a compile-time check, the reflection surface + serde wire
//! form + `Display` + the generated `__izumi_catalog_tests` come free).
//!
//! Slugs / urgencies / auth flags / cadences EXACTLY match the mado
//! `SourceKind` table entries for these kinds, so item ids (fnv1a over
//! `slug ':' key`) and snapshot rows are byte-compatible with a mado board
//! for every shared source.

izumi::catalog! {
    /// The izumi-board catalog (the 25-provider generic surface ported from
    /// mado's suggestion plane — the mado-only kinds like `recent-dirs` /
    /// `agent` / `safra` stay consumer-side in mado).
    pub enum BoardKind {
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
