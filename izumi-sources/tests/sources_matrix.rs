//! The CLOSED-LOOP verification matrix (MASS-SYNTHESIS rule 1): one row per
//! ported provider, each exercised end-to-end through a consumer-authored
//! `izumi::catalog!` kind enum — the same integration surface a real consumer
//! wires. A provider without a matrix row fails the count gate, so the crate
//! can never claim a provider the matrix doesn't prove.
//!
//! Each row carries the provider's happy-path fixture (reusing the ported
//! unit-test fixture), the minimum item count the fixture must yield, and —
//! where the provider has one — the `Unavailable` tier an EMPTY environment
//! with a bare config must report (the honesty-contract arm).

use std::sync::Arc;

use izumi::{
    Catalog, MockEnvironment, PollOutcome, Source, SourceConfig, SourceStatus, SpawnSpec,
};
use izumi_sources::util::pct;

izumi::catalog! {
    /// The matrix catalog — slugs / urgencies / auth / cadences exactly as
    /// the mado `SourceKind` table declares them for these 25 providers.
    pub enum MatrixKind {
        GitBranchPr { slug: "git-branch-pr", emoji: "\u{1F33F}", label: "git branch ↔ PR", urgency: Low, needs_auth: false, interval_secs: 30 },
        TendRepos { slug: "tend-repos", emoji: "\u{1F9F9}", label: "tend dirty repos", urgency: Low, needs_auth: false, interval_secs: 30 },
        CargoWarnings { slug: "cargo-warnings", emoji: "\u{1F980}", label: "cargo warnings", urgency: Low, needs_auth: false, interval_secs: 120 },
        TodoBacklog { slug: "todo-backlog", emoji: "\u{1F4DD}", label: "TODO backlog", urgency: Idle, needs_auth: false, interval_secs: 120 },
        GithubReviewRequested { slug: "github-review-requested", emoji: "\u{1F50D}", label: "GitHub review-requested", urgency: High, needs_auth: true, interval_secs: 180 },
        GithubAssignedIssues { slug: "github-assigned-issues", emoji: "\u{1F41B}", label: "GitHub assigned issues", urgency: Normal, needs_auth: true, interval_secs: 180 },
        GithubActionsFailing { slug: "github-actions-failing", emoji: "\u{1F6A8}", label: "GitHub Actions failing", urgency: High, needs_auth: true, interval_secs: 180 },
        JiraSprint { slug: "jira-sprint", emoji: "\u{1F3AB}", label: "Jira sprint", urgency: Normal, needs_auth: true, interval_secs: 300 },
        JiraAssigned { slug: "jira-assigned", emoji: "\u{1F4CB}", label: "Jira assigned", urgency: Normal, needs_auth: true, interval_secs: 300 },
        ConfluenceMentions { slug: "confluence-mentions", emoji: "\u{1F4AC}", label: "Confluence mentions", urgency: Low, needs_auth: true, interval_secs: 300 },
        FluxFailing { slug: "flux-failing", emoji: "\u{1F501}", label: "Flux failing", urgency: High, needs_auth: false, interval_secs: 60 },
        K8sUnhealthy { slug: "k8s-unhealthy", emoji: "\u{2638}", label: "k8s unhealthy pods", urgency: Critical, needs_auth: false, interval_secs: 60 },
        BreatheConflict { slug: "breathe-conflict", emoji: "\u{1F4A8}", label: "breathe Conflict bands", urgency: High, needs_auth: false, interval_secs: 60 },
        EngenhoNodes { slug: "engenho-nodes", emoji: "\u{1F5A5}", label: "engenho nodes", urgency: Normal, needs_auth: false, interval_secs: 60 },
        GrafanaAlerts { slug: "grafana-alerts", emoji: "\u{1F525}", label: "grafana alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
        GrafanaIncidents { slug: "grafana-incidents", emoji: "\u{1F6A9}", label: "grafana incidents", urgency: Critical, needs_auth: true, interval_secs: 90 },
        GrafanaOncall { slug: "grafana-oncall", emoji: "\u{1F4DF}", label: "grafana on-call", urgency: High, needs_auth: true, interval_secs: 600 },
        DatadogMonitors { slug: "datadog-monitors", emoji: "\u{1F415}", label: "Datadog monitors", urgency: Critical, needs_auth: true, interval_secs: 90 },
        OpsgenieAlerts { slug: "opsgenie-alerts", emoji: "\u{1F514}", label: "Opsgenie alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
        KurageAgents { slug: "kurage-agents", emoji: "\u{1F916}", label: "Cursor agents", urgency: Normal, needs_auth: true, interval_secs: 120 },
        AwsHealth { slug: "aws-health", emoji: "\u{2601}", label: "AWS health", urgency: Critical, needs_auth: true, interval_secs: 300 },
        CloudflareDeployments { slug: "cloudflare-deployments", emoji: "\u{1F310}", label: "Cloudflare deployments", urgency: High, needs_auth: true, interval_secs: 300 },
        GoogleTasks { slug: "google-tasks", emoji: "\u{2705}", label: "Google Tasks", urgency: Low, needs_auth: true, interval_secs: 300 },
        GoogleCalendar { slug: "google-calendar", emoji: "\u{1F4C5}", label: "Google Calendar", urgency: Normal, needs_auth: true, interval_secs: 300 },
        SecretAge { slug: "secret-age", emoji: "\u{1F511}", label: "secret age", urgency: Normal, needs_auth: false, interval_secs: 3600 },
    }
}

type Src = Arc<dyn Source<MatrixKind, SpawnSpec>>;

/// One provider's verification row.
struct MatrixRow {
    slug: &'static str,
    /// Provider constructor (each provider carries its kind as a value).
    source: fn(MatrixKind) -> Src,
    /// The happy-path fixture environment (the ported unit-test fixture).
    env: fn() -> MockEnvironment,
    /// The happy-path config (params where the provider needs them).
    cfg: fn(MatrixKind) -> SourceConfig,
    /// Minimum items the happy-path fixture must yield.
    min_items: usize,
    /// The `Unavailable` tier a bare config + EMPTY environment must report —
    /// `None` for providers whose bare poll is honestly `Fetched(empty)`
    /// (todo-backlog: rg exits 1 on zero matches, indistinguishable from
    /// failure through the run seam).
    unconfigured: Option<SourceStatus>,
}

const GH_PRS: &str = r#"[
    {"number":1234,"title":"fix the parser","url":"https://x","repository":{"name":"mado","nameWithOwner":"pleme-io/mado"}},
    {"number":12,"title":"bump deps","url":"https://y","repository":{"name":"tear","nameWithOwner":"pleme-io/tear"}}
]"#;

const GH_ISSUES: &str = r#"[
    {"number":42,"title":"login button is misaligned","url":"https://x","repository":{"name":"mado","nameWithOwner":"pleme-io/mado"}},
    {"number":7,"title":"crash on startup","url":"https://y","repository":{"name":"tear","nameWithOwner":"pleme-io/tear"}}
]"#;

const GH_RUNS: &str = r#"[
    {"databaseId":98765,"displayTitle":"fix the parser","workflowName":"CI","headBranch":"main"},
    {"databaseId":98766,"displayTitle":"bump deps","workflowName":"release","headBranch":"feat/x"}
]"#;

const TEND: &str = r#"[
    {"name":"mado","path":"/code/github/pleme-io/mado","state":"dirty"},
    {"name":"tear","path":"/code/github/pleme-io/tear","state":"clean"},
    {"name":"newrepo","path":"","state":"missing"}
]"#;

const CARGO: &str = concat!(
    r#"{"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/lib.rs"}]}}"#,
    "\n",
    r#"{"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/lib.rs"}]}}"#,
    "\n",
    r#"{"reason":"build-finished","success":true}"#,
    "\n",
);

const TODO: &str = "/code/github/pleme-io/mado/src/main.rs:42:    // TODO: wire the picker into the FSM\n/code/github/pleme-io/tear/src/lib.rs:7:    # FIXME handle the empty case\n";

const JIRA: &str = r#"{
    "issues": [
        {"key":"PROJ-1","fields":{"summary":"fix the parser","status":{"name":"In Progress"},"priority":{"name":"Highest"}}},
        {"key":"PROJ-2","fields":{"summary":"bump deps","status":{"name":"To Do"},"priority":{"name":"Low"}}}
    ]
}"#;

const CONFLUENCE: &str = r#"{
    "results": [
        {"content": {"id": "12345", "title": "Q3 planning notes"}},
        {"content": {"id": "67890", "title": "Architecture review"}}
    ]
}"#;

const FLUX: &str = r#"{
    "items": [
        {
            "kind": "Kustomization",
            "metadata": {"name": "apps", "namespace": "flux-system"},
            "status": {"conditions": [
                {"type": "Ready", "status": "False", "reason": "BuildFailed", "message": "kustomize build failed"}
            ]}
        }
    ]
}"#;

const PODS: &str = r#"{
    "items": [
        {
            "metadata": {"name": "api-7d9f", "namespace": "prod"},
            "status": {
                "phase": "Running",
                "containerStatuses": [
                    {"state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]
            }
        },
        {
            "metadata": {"name": "queued-1", "namespace": "prod"},
            "status": {"phase": "Pending", "containerStatuses": []}
        }
    ]
}"#;

const BANDS: &str = r#"{
    "items": [
        {"metadata":{"name":"ntfy","namespace":"observability"},"status":{"phase":"Conflict","conditions":[]}},
        {"metadata":{"name":"grafana","namespace":"monitoring"},"status":{"phase":"Ready","conditions":[{"type":"Conflict","status":"True"}]}}
    ]
}"#;

const NODES: &str = r#"{
    "items": [
        {"metadata":{"name":"zek"},"status":{"conditions":[{"type":"Ready","status":"False"}]}}
    ]
}"#;

const GRAFANA_ALERTS: &str = r#"{
    "data": {
        "alerts": [
            {"labels": {"alertname": "HighCPU", "severity": "critical"}, "state": "Alerting"},
            {"labels": {"alertname": "SlowQuery", "severity": "warning"}, "state": "firing"},
            {"labels": {"alertname": "DiskFull"}, "state": "firing"}
        ]
    }
}"#;

const GRAFANA_INCIDENTS: &str = r#"[
    {"id":7,"text":"disk full on rio"},
    {"id":42,"text":"api latency spike"}
]"#;

const GRAFANA_SHIFTS: &str = r#"{
    "results": [
        {"id":"S1","name":"primary rotation"},
        {"id":"S2","name":"secondary rotation"}
    ]
}"#;

const DATADOG: &str = r#"[
    {"id":111,"name":"api latency p99","overall_state":"Alert","priority":1},
    {"id":333,"name":"error rate spike","overall_state":"Alert","priority":3}
]"#;

const OPSGENIE: &str = r#"{
    "data": [
        {"id":"a1","message":"db replica down","priority":"P1"},
        {"id":"a2","message":"disk almost full","priority":"P3"}
    ]
}"#;

const KURAGE: &str = r#"[
    {"id":"a1","name":"refactor the suggest registry","status":"RUNNING","repository":"pleme-io/mado"},
    {"id":"a3","name":"draft docs","status":"QUEUED","repository":"standalone"}
]"#;

const AWS: &str = r#"{
    "events": [
        {"arn":"arn:aws:health::event/EC2/abc","service":"EC2","eventTypeCode":"AWS_EC2_INSTANCE_STORE_DRIVE_PERFORMANCE_DEGRADED","statusCode":"open"},
        {"arn":"arn:aws:health::event/RDS/xyz","service":"RDS","eventTypeCode":"AWS_RDS_MAINTENANCE_SCHEDULED","statusCode":"upcoming"}
    ]
}"#;

const CLOUDFLARE: &str = r#"{
    "result": [
        {"id":"dep-fail-1","latest_stage":{"name":"deploy","status":"failure"}},
        {"id":"dep-ok-2","latest_stage":{"name":"deploy","status":"success"}}
    ]
}"#;

const GTASKS: &str = r#"{"items":[
    {"id":"abc","title":"buy milk","due":"2026-07-01"},
    {"id":"def","title":"call mom","due":""}
]}"#;

const GCAL: &str = r#"{
    "items": [
        {"id":"evt-1","summary":"Standup with the team","start":{"dateTime":"2026-06-27T09:00:00Z"}},
        {"id":"evt-2","summary":"Planning","start":{"dateTime":"2026-06-27T14:30:00Z"}}
    ]
}"#;

const SECRETS: &str = "/home/op/.config/github/token\n/home/op/.config/akeyless/access-token\n";

fn jira_sprint_url() -> String {
    let mut u = String::from(
        "https://acme.atlassian.net/rest/api/3/search/jql?maxResults=5&fields=summary,priority&jql=",
    );
    u.push_str(&pct("sprint=42"));
    u
}

fn jira_assigned_url() -> String {
    let mut u = String::from("https://pleme.atlassian.net/rest/api/3/search/jql?jql=");
    u.push_str(&pct(
        "assignee=currentUser() AND statusCategory != Done ORDER BY updated DESC",
    ));
    u.push_str("&maxResults=5&fields=summary,status,priority");
    u
}

fn with_params(kind: MatrixKind, pairs: &[(&str, &str)]) -> SourceConfig {
    let mut cfg = SourceConfig::for_kind(kind);
    for (k, v) in pairs {
        cfg.params.insert((*k).to_string(), (*v).to_string());
    }
    cfg
}

/// The whole matrix — ONE row per ported provider, in catalog order.
#[allow(clippy::too_many_lines)]
fn matrix() -> Vec<MatrixRow> {
    vec![
        MatrixRow {
            slug: "git-branch-pr",
            source: |k| Arc::new(izumi_sources::GitBranchPr::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .path("/code/github/pleme-io/mado")
                    .cmd(
                        "gh search prs --author=@me --state=open --json number,title,url,repository --limit 5",
                        GH_PRS,
                    )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "tend-repos",
            source: |k| Arc::new(izumi_sources::TendRepos::new(k)),
            env: || MockEnvironment::new().cmd("tend status --json", TEND),
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "cargo-warnings",
            source: |k| Arc::new(izumi_sources::CargoWarnings::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .cmd("cargo check --message-format json", CARGO)
            },
            cfg: SourceConfig::for_kind,
            min_items: 1,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "todo-backlog",
            source: |k| Arc::new(izumi_sources::TodoBacklog::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .cmd("rg --no-heading -n -e TODO -e FIXME /code", TODO)
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            // rg exits 1 on zero matches — the bare poll is honestly
            // Fetched(empty), never Unavailable.
            unconfigured: None,
        },
        MatrixRow {
            slug: "github-review-requested",
            source: |k| Arc::new(izumi_sources::GithubReviewRequested::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .path("/code/github/pleme-io/mado")
                    .cmd(
                        "gh search prs --review-requested=@me --state=open --json number,title,url,repository --limit 5",
                        GH_PRS,
                    )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "github-assigned-issues",
            source: |k| Arc::new(izumi_sources::GithubAssignedIssues::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .path("/code/github/pleme-io/mado")
                    .cmd(
                        "gh search issues --assignee=@me --state=open --json number,title,url,repository --limit 5",
                        GH_ISSUES,
                    )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "github-actions-failing",
            source: |k| Arc::new(izumi_sources::GithubActionsFailing::new(k)),
            env: || {
                MockEnvironment::new().roots("/code", "/home/op").cmd(
                    "gh run list --status=failure --json databaseId,displayTitle,workflowName,headBranch --limit 5",
                    GH_RUNS,
                )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "jira-sprint",
            source: |k| Arc::new(izumi_sources::JiraSprint::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("atlassian/api-token", "tok")
                    .http(jira_sprint_url(), JIRA)
            },
            cfg: |k| {
                with_params(
                    k,
                    &[
                        ("base_url", "https://acme.atlassian.net"),
                        ("email", "me@acme.io"),
                        ("jql", "sprint=42"),
                    ],
                )
            },
            min_items: 2,
            unconfigured: Some(SourceStatus::Unconfigured),
        },
        MatrixRow {
            slug: "jira-assigned",
            source: |k| Arc::new(izumi_sources::JiraAssigned::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("atlassian/api-token", "tok")
                    .http(jira_assigned_url(), JIRA)
            },
            cfg: |k| {
                with_params(
                    k,
                    &[("site", "pleme.atlassian.net"), ("email", "op@pleme.io")],
                )
            },
            min_items: 2,
            unconfigured: Some(SourceStatus::Unconfigured),
        },
        MatrixRow {
            slug: "confluence-mentions",
            source: |k| Arc::new(izumi_sources::ConfluenceMentions::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("atlassian/api-token", "tok")
                    .http(
                        "https://x.atlassian.net/wiki/rest/api/search?limit=5&cql=mention%20%3D%20currentUser%28%29%20order%20by%20lastModified%20desc",
                        CONFLUENCE,
                    )
            },
            cfg: |k| {
                with_params(
                    k,
                    &[("base_url", "https://x.atlassian.net"), ("email", "me@x.io")],
                )
            },
            min_items: 2,
            unconfigured: Some(SourceStatus::Unconfigured),
        },
        MatrixRow {
            slug: "flux-failing",
            source: |k| Arc::new(izumi_sources::FluxFailing::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .cmd("kubectl get kustomizations,helmreleases -A -o json", FLUX)
            },
            cfg: SourceConfig::for_kind,
            min_items: 1,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "k8s-unhealthy",
            source: |k| Arc::new(izumi_sources::K8sUnhealthy::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .cmd("kubectl get pods -A -o json", PODS)
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "breathe-conflict",
            source: |k| Arc::new(izumi_sources::BreatheConflict::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .cmd("kubectl get breathebands -A -o json", BANDS)
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "engenho-nodes",
            source: |k| Arc::new(izumi_sources::EngenhoNodes::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .cmd("kubectl get nodes -o json", NODES)
            },
            cfg: SourceConfig::for_kind,
            min_items: 1,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "grafana-alerts",
            source: |k| Arc::new(izumi_sources::GrafanaAlerts::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("grafana/api-token", "tok-123")
                    .http(
                        "https://grafana.rio/api/prometheus/grafana/api/v1/alerts",
                        GRAFANA_ALERTS,
                    )
            },
            cfg: |k| with_params(k, &[("base_url", "https://grafana.rio")]),
            min_items: 3,
            unconfigured: Some(SourceStatus::Unconfigured),
        },
        MatrixRow {
            slug: "grafana-incidents",
            source: |k| Arc::new(izumi_sources::GrafanaIncidents::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("grafana/api-token", "tok")
                    .http(
                        "https://grafana.rio/api/annotations?tags=incident&limit=5",
                        GRAFANA_INCIDENTS,
                    )
            },
            cfg: |k| with_params(k, &[("base_url", "https://grafana.rio")]),
            min_items: 2,
            unconfigured: Some(SourceStatus::Unconfigured),
        },
        MatrixRow {
            slug: "grafana-oncall",
            source: |k| Arc::new(izumi_sources::GrafanaOncall::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("grafana/oncall-token", "tok")
                    .http("https://oncall.example/api/v1/shifts?per_page=5", GRAFANA_SHIFTS)
            },
            cfg: |k| with_params(k, &[("oncall_url", "https://oncall.example")]),
            min_items: 2,
            unconfigured: Some(SourceStatus::Unconfigured),
        },
        MatrixRow {
            slug: "datadog-monitors",
            source: |k| Arc::new(izumi_sources::DatadogMonitors::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("datadog/api-key", "ddk")
                    .secret_val("datadog/app-key", "dak")
                    .http(
                        "https://api.datadoghq.com/api/v1/monitor?group_states=alert",
                        DATADOG,
                    )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::AuthMissing),
        },
        MatrixRow {
            slug: "opsgenie-alerts",
            source: |k| Arc::new(izumi_sources::OpsgenieAlerts::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("opsgenie/api-key", "k3y")
                    .http(
                        "https://api.opsgenie.com/v2/alerts?limit=5&query=status%3A%20open",
                        OPSGENIE,
                    )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::AuthMissing),
        },
        MatrixRow {
            slug: "kurage-agents",
            source: |k| Arc::new(izumi_sources::KurageAgents::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .path("/code/github/pleme-io/mado")
                    .cmd("kurage list-agents --json", KURAGE)
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "aws-health",
            source: |k| Arc::new(izumi_sources::AwsHealth::new(k)),
            env: || {
                MockEnvironment::new().roots("/code", "/home/op").cmd(
                    "aws health describe-events --filter eventStatusCodes=open,upcoming --output json --region us-east-1",
                    AWS,
                )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
        MatrixRow {
            slug: "cloudflare-deployments",
            source: |k| Arc::new(izumi_sources::CloudflareDeployments::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("cloudflare/api-token", "tok")
                    .http(
                        "https://api.cloudflare.com/client/v4/accounts/acct-1/pages/projects/gaveta-web/deployments?per_page=5",
                        CLOUDFLARE,
                    )
            },
            cfg: |k| {
                with_params(k, &[("account_id", "acct-1"), ("pages_project", "gaveta-web")])
            },
            min_items: 1,
            unconfigured: Some(SourceStatus::Unconfigured),
        },
        MatrixRow {
            slug: "google-tasks",
            source: |k| Arc::new(izumi_sources::GoogleTasks::new(k)),
            env: || {
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("google/tasks-token", "tok")
                    .http(
                        "https://tasks.googleapis.com/tasks/v1/lists/@default/tasks?showCompleted=false&maxResults=5",
                        GTASKS,
                    )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::AuthMissing),
        },
        MatrixRow {
            slug: "google-calendar",
            source: |k| Arc::new(izumi_sources::GoogleCalendar::new(k)),
            env: || {
                // The mock clock defaults to 1_000_000 → the pct-encoded
                // timeMin cursor below.
                MockEnvironment::new()
                    .roots("/code", "/home/op")
                    .secret_val("google/calendar-token", "tok-123")
                    .http(
                        "https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=5&timeMin=1970-01-12T13%3A46%3A40Z",
                        GCAL,
                    )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::AuthMissing),
        },
        MatrixRow {
            slug: "secret-age",
            source: |k| Arc::new(izumi_sources::SecretAge::new(k)),
            env: || {
                MockEnvironment::new().roots("/code", "/home/op").cmd(
                    "find /home/op/.config -maxdepth 2 -type f -name *token* -mtime +90",
                    SECRETS,
                )
            },
            cfg: SourceConfig::for_kind,
            min_items: 2,
            unconfigured: Some(SourceStatus::Error),
        },
    ]
}

/// Build a labeled failure line without `format!()` (TYPED EMISSION: typed
/// string construction through `push_str`).
fn fail_line(slug: &str, reason: &str) -> String {
    let mut s = String::new();
    s.push_str(slug);
    s.push_str(": ");
    s.push_str(reason);
    s
}

/// Rule-1 forcing function: EVERY provider's happy path yields its minimum
/// item count through the real `Source::poll` border, and every provider with
/// an unavailability tier reports it against an empty environment. Failures
/// aggregate before the assert so one run reports every broken provider.
#[test]
fn every_provider_in_the_matrix_works() {
    let mut failures: Vec<String> = Vec::new();
    for row in matrix() {
        let Some(kind) = MatrixKind::from_slug(row.slug) else {
            failures.push(fail_line(row.slug, "slug not in the matrix catalog"));
            continue;
        };
        let source = (row.source)(kind);
        if source.kind() != kind {
            failures.push(fail_line(row.slug, "provider reports a different kind"));
            continue;
        }
        // Happy path: the fixture must yield at least min_items.
        match source.poll(&(row.env)(), &(row.cfg)(kind)) {
            PollOutcome::Fetched(items) => {
                if items.len() < row.min_items {
                    let mut reason = String::from("happy path yielded ");
                    reason.push_str(&items.len().to_string());
                    reason.push_str(" items, expected at least ");
                    reason.push_str(&row.min_items.to_string());
                    failures.push(fail_line(row.slug, &reason));
                } else if items.iter().any(|s| s.source != kind) {
                    failures.push(fail_line(row.slug, "an item carries a foreign kind"));
                }
            }
            PollOutcome::Unavailable(status) => {
                let mut reason = String::from("happy path was Unavailable(");
                reason.push_str(status.label());
                reason.push(')');
                failures.push(fail_line(row.slug, &reason));
            }
        }
        // Honesty arm: a bare config + empty environment reports the typed tier.
        if let Some(expected) = row.unconfigured {
            let bare = SourceConfig::for_kind(kind);
            match source.poll(&MockEnvironment::new(), &bare) {
                PollOutcome::Unavailable(status) if status == expected => {}
                PollOutcome::Unavailable(status) => {
                    let mut reason = String::from("bare poll reported ");
                    reason.push_str(status.label());
                    reason.push_str(", expected ");
                    reason.push_str(expected.label());
                    failures.push(fail_line(row.slug, &reason));
                }
                PollOutcome::Fetched(_) => {
                    failures.push(fail_line(
                        row.slug,
                        "bare poll was Fetched — the honesty tier is missing",
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} provider row(s) failed:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

/// The count gate: a new provider landing without a matrix row fails the
/// build here (and a row landing without a provider fails above).
#[test]
fn matrix_has_exactly_one_row_per_provider() {
    let rows = matrix();
    assert_eq!(rows.len(), 25, "the matrix carries exactly 25 provider rows");
    assert_eq!(
        MatrixKind::ALL.len(),
        25,
        "the matrix catalog carries exactly 25 kinds"
    );
    // Every row's slug resolves to a distinct catalog kind.
    let mut kinds: Vec<MatrixKind> = rows
        .iter()
        .filter_map(|r| MatrixKind::from_slug(r.slug))
        .collect();
    assert_eq!(kinds.len(), 25, "every row slug resolves in the catalog");
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds.len(), 25, "no two rows share a slug");
}

/// The exported registry-wiring helper proves a full matrix-derived registry
/// covers the catalog exactly — the same invariant a consumer's test runs.
#[test]
fn matrix_registry_covers_the_catalog() {
    let sources: Vec<Src> = matrix()
        .iter()
        .filter_map(|r| MatrixKind::from_slug(r.slug).map(r.source))
        .collect();
    izumi_sources::assert_registry_wiring(&sources, MatrixKind::ALL);
}
