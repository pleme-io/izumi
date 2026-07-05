//! `jira-sprint` — issues assigned to you in the current open sprint, surfaced
//! as "go work this ticket" items. HTTP against Atlassian Cloud's Jira
//! search API; auth is the `atlassian/api-token` secret + an `email` param
//! (HTTP Basic). Enter spawns a session rooted at your code root.
//!
//! Live wiring: `GET <base_url>/rest/api/3/search?maxResults=N&fields=summary&jql=<jql>`
//! with `Authorization: Basic <email:token>`. Config params: `base_url`
//! (required), `email`, `jql` (override the default query), `secret` (override
//! the token's `category/name`). Honesty contract: a missing `base_url` is
//! `Unavailable(Unconfigured)`, a missing token `Unavailable(AuthMissing)`, a
//! failed fetch `Unavailable(Error)` — only an OBSERVED response is `Fetched`
//! (so a network blip never reads as "sprint cleared").

use crate::util::{pct, JiraPriority, PriorityScale};
use izumi::{Catalog, CorrKey, Environment, HttpReq, Item, PollOutcome, SourceConfig, Source, SpawnSpec};

pub struct JiraSprint<K: Catalog> {
    kind: K,
}

impl<K: Catalog> JiraSprint<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for JiraSprint<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let Some(base) = cfg.param("base_url") else {
            return PollOutcome::unconfigured();
        };
        let secret_key = cfg.param("secret").unwrap_or("atlassian/api-token");
        let Some(token) = env.secret(secret_key) else {
            return PollOutcome::auth_missing();
        };
        let email = cfg.param("email").unwrap_or("");
        let jql = cfg.param("jql").unwrap_or(
            "assignee=currentUser() AND sprint in openSprints() AND statusCategory != Done",
        );
        let max = cfg.max_items.max(1);
        let mut url = String::new();
        url.push_str(base);
        // /search/jql — the current Jira Cloud endpoint (legacy /search was
        // removed 2025-05-01); the response shape (issues[].key/fields) is the same.
        url.push_str("/rest/api/3/search/jql?maxResults=");
        url.push_str(&max.to_string());
        url.push_str("&fields=summary,priority&jql=");
        url.push_str(&pct(jql));
        let req = HttpReq::new(url)
            .basic_auth(email, token)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(self.kind, &out, env, base, max))
    }
}

/// Parse a Jira `/rest/api/3/search` response into items. Pure — the unit
/// the source is tested through.
fn parse<K: Catalog>(
    kind: K,
    json: &str,
    env: &dyn Environment,
    base: &str,
    max: usize,
) -> Vec<Item<K, SpawnSpec>> {
    let Ok(result) = serde_json::from_str::<SearchResult>(json) else {
        return Vec::new();
    };
    result
        .issues
        .into_iter()
        .filter(|i| !i.key.is_empty())
        .take(max)
        .filter_map(|issue| {
            let cwd = env.code_root();
            let summary: String = issue.fields.summary.trim().chars().take(80).collect();
            let mut name = String::from("\u{1F3AB} "); // 🎫
            name.push_str(&issue.key);
            // Kickoff: pop the ticket in the browser while the shell seats you
            // at the code root — Enter lands you working, not searching.
            // `base_url` already carries the scheme, so no `https://` prefix.
            let mut kickoff = String::from("open ");
            kickoff.push_str(base);
            kickoff.push_str("/browse/");
            kickoff.push_str(&issue.key);
            let spawn = SpawnSpec::new(cwd, name)?.with_command(kickoff);
            let mut title = String::new();
            title.push_str(&issue.key);
            title.push(' ');
            title.push_str(&summary);
            // Priority drives rank: a high-priority sprint ticket rises to the
            // top of the session-generation stream (operator directive).
            let prio = issue.fields.priority.name.trim();
            let mut detail = issue.key.clone();
            if !prio.is_empty() {
                detail.push_str(" \u{00B7} "); // ·
                detail.push_str(prio);
            }
            Some(
                Item::new(kind, &issue.key, title, spawn)
                    .correlated(CorrKey::jira(&issue.key))
                    .detail(detail)
                    .ranked(JiraPriority::rank_of(prio)),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct SearchResult {
    #[serde(default)]
    issues: Vec<Issue>,
}

#[derive(serde::Deserialize, Default)]
struct Issue {
    #[serde(default)]
    key: String,
    #[serde(default)]
    fields: Fields,
}

#[derive(serde::Deserialize, Default)]
struct Fields {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    priority: NamedField,
}

#[derive(serde::Deserialize, Default)]
struct NamedField {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::{MockEnvironment, Urgency};

    const FIXTURE: &str = r#"{
        "issues": [
            {"key":"PROJ-1","fields":{"summary":"fix the parser","priority":{"name":"Highest"}}},
            {"key":"PROJ-2","fields":{"summary":"bump deps","priority":{"name":"Low"}}}
        ]
    }"#;

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(TestKind::JiraSprint);
        cfg.max_items = 5;
        cfg.params
            .insert("base_url".into(), "https://acme.atlassian.net".into());
        cfg.params.insert("email".into(), "me@acme.io".into());
        cfg.params.insert("jql".into(), "sprint=42".into());
        cfg
    }

    fn url() -> String {
        // Built with the same helper poll uses so the mock matches exactly.
        let mut u = String::from(
            "https://acme.atlassian.net/rest/api/3/search/jql?maxResults=5&fields=summary,priority&jql=",
        );
        u.push_str(&pct("sprint=42"));
        u
    }

    #[test]
    fn produces_an_item_per_issue() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("atlassian/api-token", "tok")
            .http(url(), FIXTURE);
        let PollOutcome::Fetched(out) = JiraSprint::new(TestKind::JiraSprint).poll(&env, &cfg())
        else {
            panic!("an observed response is Fetched");
        };
        assert_eq!(out.len(), 2);
        let one = out.iter().find(|s| s.title.contains("PROJ-1")).unwrap();
        assert!(one.title.contains("fix the parser"));
        // Detail carries the key + the priority name.
        assert_eq!(one.detail.as_deref(), Some("PROJ-1 \u{00B7} Highest"));
        // Highest priority → Critical tier (the absolute top of the stream).
        assert_eq!(one.urgency, Urgency::Critical);
        assert_eq!(one.spawn.cwd().to_str().unwrap(), "/code");
        // Enter launches WORK: the kickoff opens the ticket itself.
        assert_eq!(
            one.spawn.initial_command(),
            Some("open https://acme.atlassian.net/browse/PROJ-1")
        );
        // The Highest ticket ranks above the Low one.
        let two = out.iter().find(|s| s.title.contains("PROJ-2")).unwrap();
        assert_eq!(two.urgency, Urgency::Low);
        assert!(one.rank_key() > two.rank_key());
    }

    #[test]
    fn jql_is_percent_encoded() {
        // The '=' in the JQL must be %3D — proving the request URL the mock
        // keys on is exactly what poll builds.
        assert_eq!(pct("sprint=42"), "sprint%3D42");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No base_url param → Unconfigured (needs config, not "no work").
        let env = MockEnvironment::new();
        let bare = SourceConfig::for_kind(TestKind::JiraSprint);
        assert_eq!(
            JiraSprint::new(TestKind::JiraSprint).poll(&env, &bare),
            PollOutcome::unconfigured()
        );
        // Params present but the token secret is missing → AuthMissing.
        assert_eq!(
            JiraSprint::new(TestKind::JiraSprint).poll(&env, &cfg()),
            PollOutcome::auth_missing()
        );
        // Params + token present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new().secret_val("atlassian/api-token", "tok");
        assert_eq!(
            JiraSprint::new(TestKind::JiraSprint).poll(&env, &cfg()),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(
            parse(TestKind::JiraSprint, "not json", &MockEnvironment::new(), "https://x", 5)
                .is_empty()
        );
    }
}
