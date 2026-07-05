//! `google-tasks` — your incomplete Google Tasks (the default list), surfaced
//! as "go knock this out" items. HTTP against the Tasks API with a bearer
//! token from `cofre` (secret `google/tasks-token`, overridable via the
//! `secret` param). Enter spawns a session in your code root.
//!
//! Live wiring: `GET https://tasks.googleapis.com/tasks/v1/lists/@default/tasks
//! ?showCompleted=false&maxResults=N` with `Authorization: Bearer <token>`.
//! Honesty contract: a missing token is `Unavailable(AuthMissing)` (no request
//! is fired), a failed fetch `Unavailable(Error)` — only an OBSERVED response
//! is `Fetched` (so an auth outage never reads as "all tasks done").

use izumi::{Catalog, Environment, HttpReq, Item, PollOutcome, SourceConfig, Source, SpawnSpec, Urgency};

pub struct GoogleTasks<K: Catalog> {
    kind: K,
}

impl<K: Catalog> GoogleTasks<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for GoogleTasks<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let max = cfg.max_items.max(1);
        let secret_key = cfg.param("secret").unwrap_or("google/tasks-token");
        let Some(token) = env.secret(secret_key) else {
            return PollOutcome::auth_missing();
        };
        let mut url = String::from(
            "https://tasks.googleapis.com/tasks/v1/lists/@default/tasks?showCompleted=false&maxResults=",
        );
        url.push_str(&max.to_string());
        let req = HttpReq::new(url).bearer(&token);
        let Some(out) = env.http_get(&req) else {
            return PollOutcome::error();
        };
        let mut items = parse(self.kind, &out, env);
        items.truncate(max);
        PollOutcome::Fetched(items)
    }
}

/// Parse the Tasks API `{items:[…]}` body into items for incomplete
/// tasks. Pure — the unit the source is tested through.
fn parse<K: Catalog>(kind: K, json: &str, env: &dyn Environment) -> Vec<Item<K, SpawnSpec>> {
    let Ok(resp) = serde_json::from_str::<TasksResponse>(json) else {
        return Vec::new();
    };
    resp.items
        .into_iter()
        .filter(|t| !t.title.trim().is_empty())
        .filter_map(|t| {
            let cwd = env.code_root();
            let name = String::from("\u{2705} task"); // ✅
            let spawn = SpawnSpec::new(cwd, name)?;
            let title = t.title.trim().to_string();
            Some(
                Item::new(kind, &t.id, title, spawn)
                    .detail(t.due)
                    .urgent(Urgency::Low),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct TasksResponse {
    #[serde(default)]
    items: Vec<TaskRow>,
}

#[derive(serde::Deserialize, Default)]
struct TaskRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    due: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    const URL: &str =
        "https://tasks.googleapis.com/tasks/v1/lists/@default/tasks?showCompleted=false&maxResults=5";

    const FIXTURE: &str = r#"{"items":[
        {"id":"abc","title":"buy milk","due":"2026-07-01"},
        {"id":"def","title":"call mom","due":""},
        {"id":"ghi","title":"","due":""}
    ]}"#;

    #[test]
    fn produces_an_item_per_incomplete_task() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("google/tasks-token", "tok")
            .http(URL, FIXTURE);
        let mut cfg = SourceConfig::for_kind(TestKind::GoogleTasks);
        cfg.max_items = 5;
        let PollOutcome::Fetched(out) = GoogleTasks::new(TestKind::GoogleTasks).poll(&env, &cfg)
        else {
            panic!("an observed response is Fetched");
        };
        assert_eq!(out.len(), 2, "empty-title task excluded");
        let milk = out.iter().find(|s| s.title == "buy milk").unwrap();
        assert_eq!(milk.spawn.cwd().to_str().unwrap(), "/code");
        assert_eq!(milk.urgency, Urgency::Low);
        assert_eq!(milk.detail.as_deref(), Some("2026-07-01"));
        // A task with no due date still surfaces (detail just goes empty).
        assert!(out.iter().any(|s| s.title == "call mom"));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No token secret → AuthMissing (needs auth, not "no tasks") — and no
        // request is ever fired unauthenticated.
        let cfg = SourceConfig::for_kind(TestKind::GoogleTasks);
        assert_eq!(
            GoogleTasks::new(TestKind::GoogleTasks).poll(&MockEnvironment::new(), &cfg),
            PollOutcome::auth_missing()
        );
        // Token present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new().secret_val("google/tasks-token", "tok");
        assert_eq!(
            GoogleTasks::new(TestKind::GoogleTasks).poll(&env, &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse(TestKind::GoogleTasks, "not json", &MockEnvironment::new()).is_empty());
    }
}
