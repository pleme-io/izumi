//! `datadog-monitors` — Datadog monitors currently in the `Alert` state,
//! surfaced as "go look at this fire" items. HTTP against Datadog's v1
//! monitor API; auth is the `datadog/api-key` + `datadog/app-key` secrets sent
//! as headers (overridable via the `api_key_secret` / `app_key_secret`
//! params). Enter spawns a session rooted at your code root.
//!
//! Live wiring: `GET <base_url>/api/v1/monitor?group_states=alert` with
//! `DD-API-KEY` + `DD-APPLICATION-KEY` headers. Each monitor whose
//! `overall_state` is `Alert` becomes an item. Honesty contract: either
//! key missing is `Unavailable(AuthMissing)` (no request is fired), a failed
//! fetch `Unavailable(Error)` — only an OBSERVED response is `Fetched` (so an
//! auth outage never reads as "nothing is on fire").

use crate::util::{IncidentSeverity, PriorityScale};
use izumi::{Catalog, Environment, HttpReq, Item, PollOutcome, SourceConfig, Source, SpawnSpec};

pub struct DatadogMonitors<K: Catalog> {
    kind: K,
}

impl<K: Catalog> DatadogMonitors<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for DatadogMonitors<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let api_key_ref = cfg.param("api_key_secret").unwrap_or("datadog/api-key");
        let Some(api_key) = env.secret(api_key_ref) else {
            return PollOutcome::auth_missing();
        };
        let application_key_ref = cfg.param("app_key_secret").unwrap_or("datadog/app-key");
        let Some(application_key) = env.secret(application_key_ref) else {
            return PollOutcome::auth_missing();
        };
        let base = cfg.param("base_url").unwrap_or("https://api.datadoghq.com");
        let max = cfg.max_items.max(1);
        let mut url = String::new();
        url.push_str(base);
        url.push_str("/api/v1/monitor?group_states=alert");
        let req = HttpReq::new(url)
            .header("DD-API-KEY", api_key)
            .header("DD-APPLICATION-KEY", application_key)
            .header("Accept", "application/json");
        let Some(out) = env.http_get(&req) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(self.kind, &out, env, max))
    }
}

/// Parse a Datadog `/api/v1/monitor` response into items for monitors in
/// the `Alert` state. Pure — the unit the source is tested through.
fn parse<K: Catalog>(
    kind: K,
    json: &str,
    env: &dyn Environment,
    max: usize,
) -> Vec<Item<K, SpawnSpec>> {
    let Ok(rows) = serde_json::from_str::<Vec<MonitorRow>>(json) else {
        return Vec::new();
    };
    let mut out: Vec<Item<K, SpawnSpec>> = rows
        .into_iter()
        .filter(|m| m.overall_state == "Alert")
        .filter_map(|m| {
            let cwd = env.code_root();
            // Display name (no emoji) drives both the session name and the
            // title; the picker prepends the source emoji to the title.
            let label: String = m.name.trim().chars().take(24).collect();
            let mut name = String::from("\u{1F415} "); // 🐕
            name.push_str(&label);
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = String::new();
            title.push_str(&label);
            title.push_str(" alerting");
            let key = m.id.to_string();
            // Rank by Datadog's 1–5 monitor priority (P1 highest): a P1 outranks
            // a P3. Absent priority stays Critical (firing-but-unprioritized).
            let level = match m.priority {
                Some(1) => "p1",
                Some(2) => "p2",
                Some(3) => "p3",
                Some(4) => "p4",
                Some(5) => "p5",
                _ => "",
            };
            let rank = IncidentSeverity::rank_of(level);
            let mut detail = String::from("datadog");
            if let Some(p) = m.priority {
                detail.push_str(" \u{00B7} P"); // ·
                detail.push_str(&p.to_string());
            }
            Some(Item::new(kind, &key, title, spawn).detail(detail).ranked(rank))
        })
        .collect();
    // Rank BEFORE the cap: the monitors API returns arbitrary order, so a
    // cut before ranking could drop a P1 while keeping P4s.
    out.sort_by_key(|s| std::cmp::Reverse(s.rank_key()));
    out.truncate(max);
    out
}

#[derive(serde::Deserialize, Default)]
struct MonitorRow {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    overall_state: String,
    /// Datadog monitor priority 1 (highest) .. 5 (lowest); absent = unset.
    #[serde(default)]
    priority: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::{MockEnvironment, Urgency};

    const FIXTURE: &str = r#"[
        {"id":111,"name":"api latency p99","overall_state":"Alert","priority":1},
        {"id":222,"name":"disk usage","overall_state":"OK"},
        {"id":333,"name":"error rate spike","overall_state":"Alert","priority":3}
    ]"#;

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(TestKind::DatadogMonitors);
        cfg.max_items = 5;
        cfg
    }

    fn url() -> String {
        // The default base_url — matches exactly what poll builds.
        String::from("https://api.datadoghq.com/api/v1/monitor?group_states=alert")
    }

    #[test]
    fn produces_an_item_per_alerting_monitor() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("datadog/api-key", "ddk")
            .secret_val("datadog/app-key", "dak")
            .http(url(), FIXTURE);
        let PollOutcome::Fetched(out) =
            DatadogMonitors::new(TestKind::DatadogMonitors).poll(&env, &cfg())
        else {
            panic!("an observed response is Fetched");
        };
        // The OK monitor is excluded; only the two Alert monitors remain.
        assert_eq!(out.len(), 2, "non-alert monitor excluded");
        let api = out.iter().find(|s| s.title.contains("api latency")).unwrap();
        assert!(api.title.contains("alerting"));
        // P1 → Critical, detail carries the priority.
        assert_eq!(api.detail.as_deref(), Some("datadog \u{00B7} P1"));
        assert_eq!(api.urgency, Urgency::Critical);
        assert_eq!(api.spawn.cwd().to_str().unwrap(), "/code");
        // A P3 monitor ranks below the P1 (High, not Critical).
        let p3 = out.iter().find(|s| s.title.contains("error rate spike")).unwrap();
        assert_eq!(p3.urgency, Urgency::High);
        assert!(api.rank_key() > p3.rank_key(), "P1 monitor outranks P3");
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // Neither key present → AuthMissing (needs auth, not "no fires") —
        // and no request is ever fired unauthenticated.
        assert_eq!(
            DatadogMonitors::new(TestKind::DatadogMonitors).poll(&MockEnvironment::new(), &cfg()),
            PollOutcome::auth_missing()
        );
        // Only the api key present → the app key is still missing → AuthMissing.
        let env = MockEnvironment::new().secret_val("datadog/api-key", "ddk");
        assert_eq!(
            DatadogMonitors::new(TestKind::DatadogMonitors).poll(&env, &cfg()),
            PollOutcome::auth_missing()
        );
        // Both keys present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new()
            .secret_val("datadog/api-key", "ddk")
            .secret_val("datadog/app-key", "dak");
        assert_eq!(
            DatadogMonitors::new(TestKind::DatadogMonitors).poll(&env, &cfg()),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse(TestKind::DatadogMonitors, "not json", &MockEnvironment::new(), 5).is_empty());
    }
}
