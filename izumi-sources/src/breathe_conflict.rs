//! `breathe-conflict` — breathe resource-homeostasis bands stuck in a Conflict
//! phase (field-ownership contention on the workload's limits), surfaced as
//! "go untangle this" items. Local CLI, no auth.
//!
//! Live wiring: `kubectl get breathe -A -o json` → `{items:[…]}` — the `breathe`
//! CATEGORY every breathe CRD declares. There is NO `breathebands` kind: the
//! per-dimension plurals are memorybands/cpubands/storagebands/…, and asking
//! for the aggregate name that reads most naturally cost this lane every poll
//! it ever made (`the server doesn't have a resource type "breathebands"` →
//! `Unavailable(Error)`, ever_ok=false since it was wired). The category is one
//! authored token that also auto-covers dimensions added later.
//!
//! The listing spans kinds, so items are keyed `namespace/kind/name` (the
//! `flux_failing` idiom, which widens the same way): breathe routinely enrolls
//! ONE workload across dimensions under ONE name, so a `namespace/name` key
//! would collide and silently drop a Conflict. A band whose `status.phase` is
//! `Conflict`, or which carries a `Conflict` condition with `status: "True"`,
//! becomes an item that drops you in the code root.
//! Config params: `context` (optional kubeconfig context the poll is scoped
//! to). Honesty contract: a failed `kubectl` run is `Unavailable(Error)` —
//! only an OBSERVED listing is `Fetched` (garbage JSON parses to empty: the
//! upstream WAS observed), so a dead kubeconfig never reads as "no conflicts".
//! The breathe CRD shape is assumed (kind + metadata.name/namespace +
//! status.phase/conditions) — assumed, which is exactly why the resource name
//! could be wrong for 1407 polls. The destination is a typed API seam where
//! identity comes from `#[kube(group, version, kind)]` on breathe-crd's own
//! structs, so a wrong name is a compile error rather than a silent lane.

use izumi::{Catalog, Cmd, Environment, Item, PollOutcome, SourceConfig, Source, SpawnSpec, Urgency};

pub struct BreatheConflict<K: Catalog> {
    kind: K,
}

impl<K: Catalog> BreatheConflict<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for BreatheConflict<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let mut cmd = Cmd::new("kubectl")
            .arg("get")
            .arg("breathe")
            .arg("-A")
            .arg("-o")
            .arg("json")
            .slow();
        // Optional `context` param scopes the poll to a named kubeconfig context.
        if let Some(ctx) = cfg.param("context") {
            cmd = cmd.arg("--context").arg(ctx);
        }
        let Some(json) = env.run(&cmd) else {
            return PollOutcome::error();
        };
        let mut items = parse(self.kind, &json, env);
        items.truncate(cfg.max_items.max(1));
        PollOutcome::Fetched(items)
            .tagged(crate::util::origin_of("breathe", cfg, None).maybe_account(cfg.param("context")))
    }
}

/// Parse `kubectl get breathe -o json` into items for Conflict bands. The
/// category spans kinds, so `kind` is load-bearing in the key AND the title.
/// Pure — the unit the source is tested through.
fn parse<K: Catalog>(kind: K, json: &str, env: &dyn Environment) -> Vec<Item<K, SpawnSpec>> {
    let Ok(list) = serde_json::from_str::<BandList>(json) else {
        return Vec::new();
    };
    let cwd = env.code_root();
    list.items
        .into_iter()
        .filter(|band| {
            band.status.phase == "Conflict"
                || band
                    .status
                    .conditions
                    .iter()
                    .any(|c| c.cond_type == "Conflict" && c.status == "True")
        })
        .filter_map(|band| {
            let name = band.metadata.name;
            let namespace = band.metadata.namespace;
            let kind_name = band.kind;
            if name.is_empty() {
                return None;
            }
            let mut session = String::from("\u{1F4A8} "); // 💨
            session.push_str(&name);
            let spawn = SpawnSpec::new(cwd.clone(), session)?;
            // `namespace/kind/name`, NOT `namespace/name`. The category query
            // spans kinds and breathe enrolls one workload across dimensions
            // under one name (MemoryBand/ntfy + CpuBand/ntfy, both in ns
            // `ntfy`). ItemId is fnv1a over `slug:key` and the store is a
            // BTreeMap<ItemId, _> whose insert OVERWRITES, so a two-field key
            // would silently drop one of the two Conflicts.
            let mut key = String::new();
            key.push_str(&namespace);
            key.push('/');
            key.push_str(&kind_name);
            key.push('/');
            key.push_str(&name);
            // Kind-qualified: the category also returns BreatheNodePool /
            // BreathePosture / BreatheOverview, for which "breathe band X"
            // would be a false label.
            let mut title = String::new();
            title.push_str(&kind_name);
            title.push('/');
            title.push_str(&name);
            title.push_str(" in Conflict");
            Some(
                Item::new(kind, &key, title, spawn)
                    .detail(namespace)
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct BandList {
    #[serde(default)]
    items: Vec<BandRow>,
}

#[derive(serde::Deserialize, Default)]
struct BandRow {
    /// The per-item `kind` kubectl preserves inside an aggregated v1.List —
    /// load-bearing since the query is a multi-kind category, not one plural.
    #[serde(default)]
    kind: String,
    #[serde(default)]
    metadata: Meta,
    #[serde(default)]
    status: Status,
}

#[derive(serde::Deserialize, Default)]
struct Meta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
}

#[derive(serde::Deserialize, Default)]
struct Status {
    #[serde(default)]
    phase: String,
    #[serde(default)]
    conditions: Vec<Condition>,
}

#[derive(serde::Deserialize, Default)]
struct Condition {
    #[serde(default, rename = "type")]
    cond_type: String,
    #[serde(default)]
    status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    // Two rows share `observability/ntfy` and differ only by kind — the exact
    // shape breathe produces when one workload is enrolled on two dimensions.
    const FIXTURE: &str = r#"{
        "items": [
            {"kind":"MemoryBand","metadata":{"name":"ntfy","namespace":"observability"},"status":{"phase":"Conflict","conditions":[]}},
            {"kind":"CpuBand","metadata":{"name":"ntfy","namespace":"observability"},"status":{"phase":"Conflict","conditions":[]}},
            {"kind":"CpuBand","metadata":{"name":"grafana","namespace":"monitoring"},"status":{"phase":"Ready","conditions":[{"type":"Conflict","status":"True"}]}},
            {"kind":"MemoryBand","metadata":{"name":"loki","namespace":"monitoring"},"status":{"phase":"Ready","conditions":[{"type":"Ready","status":"True"}]}}
        ]
    }"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get breathe -A -o json", FIXTURE)
    }

    #[test]
    fn surfaces_only_bands_in_conflict() {
        let cfg = SourceConfig::for_kind(TestKind::BreatheConflict);
        let PollOutcome::Fetched(out) =
            BreatheConflict::new(TestKind::BreatheConflict).poll(&env(), &cfg)
        else {
            panic!("an observed listing is Fetched");
        };
        // both ntfy dimensions (phase Conflict) + grafana (Conflict condition);
        // loki excluded.
        assert_eq!(out.len(), 3, "healthy band excluded");
        let ntfy = out
            .iter()
            .find(|s| s.title.starts_with("MemoryBand/ntfy"))
            .unwrap();
        assert!(ntfy.title.contains("in Conflict"));
        assert_eq!(ntfy.detail.as_deref(), Some("observability"));
        assert_eq!(ntfy.urgency, Urgency::Normal);
        assert_eq!(ntfy.spawn.cwd().to_str().unwrap(), "/code");
        // A band whose phase is Ready but carries a Conflict=True condition.
        let grafana = out
            .iter()
            .find(|s| s.title.starts_with("CpuBand/grafana"))
            .unwrap();
        assert!(grafana.title.contains("in Conflict"));
        assert_eq!(grafana.detail.as_deref(), Some("monitoring"));
    }

    #[test]
    fn two_dimensions_of_one_workload_are_two_distinct_items() {
        // The seal for the category widening: MemoryBand/ntfy and CpuBand/ntfy
        // live in ONE namespace under ONE name. ItemId is derived from
        // `slug:key` and the store's map insert overwrites, so equal ids would
        // silently drop one Conflict. Keying on kind keeps both.
        let cfg = SourceConfig::for_kind(TestKind::BreatheConflict);
        let PollOutcome::Fetched(out) =
            BreatheConflict::new(TestKind::BreatheConflict).poll(&env(), &cfg)
        else {
            panic!("an observed listing is Fetched");
        };
        let ntfy: Vec<_> = out.iter().filter(|s| s.title.contains("ntfy")).collect();
        assert_eq!(ntfy.len(), 2, "both dimensions survive");
        assert_ne!(
            ntfy[0].id, ntfy[1].id,
            "distinct ItemId — no store collision"
        );
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No required params/secret here — the only unavailability tier is a
        // failed kubectl run (no fixture → run() returns None → Error, so a
        // dead kubeconfig keeps last-known rows instead of "no conflicts").
        let cfg = SourceConfig::for_kind(TestKind::BreatheConflict);
        assert_eq!(
            BreatheConflict::new(TestKind::BreatheConflict).poll(&MockEnvironment::new(), &cfg),
            PollOutcome::error()
        );
    }

    #[test]
    fn context_param_scopes_the_kubectl_call() {
        // With `context` set, the --context args land in the typed argv (and
        // thus the mock key): only the contexted fixture answers.
        let mut cfg = SourceConfig::for_kind(TestKind::BreatheConflict);
        cfg.params.insert("context".into(), "rio".into());
        assert_eq!(
            BreatheConflict::new(TestKind::BreatheConflict).poll(&env(), &cfg),
            PollOutcome::error(),
            "the un-contexted fixture must not answer a contexted poll"
        );
        let ctx_env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .cmd("kubectl get breathe -A -o json --context rio", FIXTURE);
        let PollOutcome::Fetched(out) =
            BreatheConflict::new(TestKind::BreatheConflict).poll(&ctx_env, &cfg)
        else {
            panic!("the contexted listing is Fetched");
        };
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse(TestKind::BreatheConflict, "not json", &MockEnvironment::new()).is_empty());
    }
}
