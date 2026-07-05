//! `kurage-agents` — your Cursor cloud agents that are still in flight, surfaced
//! as "go check on this" items. Local CLI, no extra auth beyond `kurage`'s
//! own session. Enter drops you into the agent's repo so you can review or follow
//! up on its work.
//!
//! Live wiring: `kurage list-agents --json` → an array of `{id, name, status,
//! repository}`. An agent whose status is not terminal (FINISHED / COMPLETED /
//! STOPPED) becomes an item landing in that agent's working copy.
//! Honesty contract: only an OBSERVED run is `Fetched` (garbage JSON parses
//! to empty — the upstream WAS observed); `kurage` missing/failing is
//! `Unavailable(Error)` — a CLI blip never reads as "no agents in flight".

use izumi::{Catalog, Cmd, Environment, Item, PollOutcome, SourceConfig, Source, SpawnSpec, Urgency};

pub struct KurageAgents<K: Catalog> {
    kind: K,
}

impl<K: Catalog> KurageAgents<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for KurageAgents<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let cmd = Cmd::new("kurage").arg("list-agents").arg("--json");
        let Some(out) = env.run(&cmd) else {
            return PollOutcome::error();
        };
        let mut out = parse(self.kind, &out, env);
        out.truncate(cfg.max_items.max(1));
        PollOutcome::Fetched(out)
    }
}

/// Parse `kurage list-agents --json` output into items for in-flight
/// agents. Pure — the unit the source is tested through.
fn parse<K: Catalog>(kind: K, json: &str, env: &dyn Environment) -> Vec<Item<K, SpawnSpec>> {
    let Ok(rows) = serde_json::from_str::<Vec<AgentRow>>(json) else {
        return Vec::new();
    };
    rows.into_iter()
        .filter(|a| {
            let status = a.status.to_ascii_uppercase();
            !matches!(status.as_str(), "FINISHED" | "COMPLETED" | "STOPPED")
        })
        .filter_map(|a| {
            // An `owner/repo` repository resolves to its working copy; anything
            // else (or blank) falls back to the code root.
            let cwd = if a.repository.contains('/') {
                crate::util::repo_cwd(env, &a.repository)
            } else {
                env.code_root()
            };
            let short: String = a.name.chars().take(24).collect();
            let mut name = String::from("\u{1F916} "); // 🤖
            name.push_str(&short);
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = short.clone();
            title.push_str(" [");
            title.push_str(&a.status);
            title.push(']');
            Some(
                Item::new(kind, &a.id, title, spawn)
                    .detail(a.status)
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct AgentRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    repository: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    const FIXTURE: &str = r#"[
        {"id":"a1","name":"refactor the suggest registry","status":"RUNNING","repository":"pleme-io/mado"},
        {"id":"a2","name":"bump deps","status":"FINISHED","repository":"pleme-io/tear"},
        {"id":"a3","name":"draft docs","status":"QUEUED","repository":"standalone"}
    ]"#;

    #[test]
    fn surfaces_in_flight_agents_in_their_repos() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado")
            .cmd("kurage list-agents --json", FIXTURE);
        let mut cfg = SourceConfig::for_kind(TestKind::KurageAgents);
        cfg.max_items = 10;
        let PollOutcome::Fetched(out) = KurageAgents::new(TestKind::KurageAgents).poll(&env, &cfg)
        else {
            panic!("an observed kurage run is Fetched");
        };
        assert_eq!(out.len(), 2, "finished agent excluded");
        let work = out.iter().find(|s| s.title.contains("refactor")).unwrap();
        assert!(work.title.contains("[RUNNING]"));
        assert_eq!(work.urgency, Urgency::Normal);
        assert_eq!(
            work.spawn.cwd().to_str().unwrap(),
            "/code/github/pleme-io/mado"
        );
        // A repository without an `owner/repo` slash falls back to the code root.
        let docs = out.iter().find(|s| s.title.contains("draft docs")).unwrap();
        assert!(docs.title.contains("[QUEUED]"));
        assert_eq!(docs.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No fixture registered → run() returns None → Error (kurage
        // missing/failing must never read as "no agents in flight" — keep
        // the last-known rows).
        let cfg = SourceConfig::for_kind(TestKind::KurageAgents);
        assert_eq!(
            KurageAgents::new(TestKind::KurageAgents).poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse(TestKind::KurageAgents, "not json", &MockEnvironment::new()).is_empty());
    }
}
