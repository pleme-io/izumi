//! `cargo-warnings` — a nudge to clean up compiler warnings in a workspace.
//! Local tooling (`cargo`), no auth, no network.
//!
//! Live wiring: `cargo check --message-format json` → count the
//! `compiler-message` records whose `level` is `warning` AND that carry at
//! least one span. The non-empty-span filter is what makes the count exact:
//! cargo also emits a summary record (`"... generated N warnings"`) with an
//! empty `spans` array, and the old `--message-format short` substring scan
//! double-counted it (plus the per-line ` warning:` markers). If any real
//! warnings remain, emit a single "go fix these" item. Config params:
//! `workspace` (optional dir whose `Cargo.toml` is checked via
//! `--manifest-path` — required for GUI-launched consumers whose process cwd
//! is `/`; the item then drops you in that workspace). Without the param the
//! check runs against the process cwd, and a cwd with no manifest reports
//! `Unavailable(Unconfigured)` ("needs config") instead of erroring forever.
//! Honesty contract: a failed `cargo` run is `Unavailable(Error)`; a clean
//! OBSERVED build is `Fetched` of an empty set (the nudge genuinely resolves).

use izumi::{Catalog, Cmd, Environment, Item, PollOutcome, SourceConfig, Source, SpawnSpec, Urgency};

pub struct CargoWarnings<K: Catalog> {
    kind: K,
}

impl<K: Catalog> CargoWarnings<K> {
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self { kind }
    }
}

impl<K: Catalog> Source<K, SpawnSpec> for CargoWarnings<K> {
    fn kind(&self) -> K {
        self.kind
    }

    fn poll(&self, env: &dyn Environment, cfg: &SourceConfig) -> PollOutcome<K, SpawnSpec> {
        let mut cmd = Cmd::new("cargo").arg("check");
        // Optional `workspace` param pins the checked workspace explicitly —
        // the shape a GUI-launched consumer needs (its process cwd is `/`,
        // which is no cargo workspace). Without the param, keep the
        // cwd-workspace behavior, but report "needs config" instead of
        // erroring forever when the cwd carries no manifest.
        let workspace = cfg.param("workspace");
        if let Some(ws) = workspace {
            let manifest = std::path::Path::new(ws).join("Cargo.toml");
            if !env.path_exists(&manifest) {
                return PollOutcome::unconfigured();
            }
            cmd = cmd
                .arg("--manifest-path")
                .arg(manifest.to_string_lossy().into_owned());
        } else if !env.path_exists(std::path::Path::new("Cargo.toml")) {
            return PollOutcome::unconfigured();
        }
        let cmd = cmd.arg("--message-format").arg("json");
        let Some(out) = env.run(&cmd) else {
            // A non-zero cargo exit (compile errors, missing cargo) is
            // unobservable through this seam — Error keeps the last-known
            // warnings on the board until the TTL ages them out.
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(self.kind, &out, env, workspace))
    }
}

/// One `cargo --message-format json` record (only the fields we discriminate on).
#[derive(serde::Deserialize)]
struct CargoRecord {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    message: Option<CargoDiagnostic>,
}

#[derive(serde::Deserialize)]
struct CargoDiagnostic {
    #[serde(default)]
    level: String,
    /// Real diagnostics point at source; the aggregate summary has `spans: []`.
    #[serde(default)]
    spans: Vec<serde_json::Value>,
}

/// Count real warning diagnostics in `cargo check --message-format json` output
/// and, if any, emit one item seated in the checked workspace (falling back
/// to the code root when the check ran against the process cwd). Pure — the
/// unit the source is tested through. Lines that aren't JSON (cargo's human
/// progress) are skipped.
fn parse<K: Catalog>(
    kind: K,
    output: &str,
    env: &dyn Environment,
    workspace: Option<&str>,
) -> Vec<Item<K, SpawnSpec>> {
    let count = output
        .lines()
        .filter_map(|l| serde_json::from_str::<CargoRecord>(l).ok())
        .filter(|r| r.reason == "compiler-message")
        .filter_map(|r| r.message)
        .filter(|m| m.level == "warning" && !m.spans.is_empty())
        .count();
    if count == 0 {
        return Vec::new();
    }
    let cwd = workspace.map_or_else(|| env.code_root(), std::path::PathBuf::from);
    let name = String::from("\u{1F980} cargo"); // 🦀
    let Some(spawn) = SpawnSpec::new(cwd, name) else {
        return Vec::new();
    };
    let mut title = String::from("fix ");
    title.push_str(&count.to_string());
    title.push_str(" cargo warnings");
    vec![
        Item::new(kind, "cargo-warnings", title, spawn)
            .detail("cargo check")
            .urgent(Urgency::Low),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestKind;
    use izumi::MockEnvironment;

    // Two real warnings (non-empty spans) + the aggregate summary record
    // (empty spans, `level":"warning"`) that the old substring scan miscounted.
    // The non-empty-span filter must yield exactly 2.
    const FIXTURE: &str = concat!(
        r#"{"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/lib.rs"}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/lib.rs"}]}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","message":"`mado` (lib) generated 2 warnings","spans":[]}}"#,
        "\n",
        r#"{"reason":"compiler-artifact","target":{"name":"mado"}}"#,
        "\n",
        r#"{"reason":"build-finished","success":true}"#,
        "\n",
    );

    const CLEAN: &str = concat!(
        r#"{"reason":"compiler-artifact","target":{"name":"mado"}}"#,
        "\n",
        r#"{"reason":"build-finished","success":true}"#,
        "\n",
    );

    #[test]
    fn counts_warning_diagnostics_and_emits_one_item() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("Cargo.toml")
            .cmd("cargo check --message-format json", FIXTURE);
        let cfg = SourceConfig::for_kind(TestKind::CargoWarnings);
        let PollOutcome::Fetched(out) = CargoWarnings::new(TestKind::CargoWarnings).poll(&env, &cfg)
        else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 1);
        assert!(
            out[0].title.contains('2'),
            "title carries the warning count, summary record excluded: {}",
            out[0].title
        );
        assert_eq!(out[0].detail.as_deref(), Some("cargo check"));
        assert_eq!(out[0].urgency, Urgency::Low);
        assert_eq!(out[0].spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn workspace_param_pins_the_manifest_and_the_spawn_cwd() {
        // With `workspace` set, the check runs via --manifest-path (no cwd
        // assumption — the GUI-launch case) and the item lands in that
        // workspace instead of the code root.
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado/Cargo.toml")
            .cmd(
                "cargo check --manifest-path /code/github/pleme-io/mado/Cargo.toml --message-format json",
                FIXTURE,
            );
        let mut cfg = SourceConfig::for_kind(TestKind::CargoWarnings);
        cfg.params
            .insert("workspace".into(), "/code/github/pleme-io/mado".into());
        let PollOutcome::Fetched(out) = CargoWarnings::new(TestKind::CargoWarnings).poll(&env, &cfg)
        else {
            panic!("an observed run is Fetched");
        };
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].spawn.cwd().to_str().unwrap(),
            "/code/github/pleme-io/mado"
        );
        // A workspace param pointing at no manifest is a config mistake,
        // not a fetch failure.
        let mut bad = SourceConfig::for_kind(TestKind::CargoWarnings);
        bad.params.insert("workspace".into(), "/nowhere".into());
        assert_eq!(
            CargoWarnings::new(TestKind::CargoWarnings).poll(&env, &bad),
            PollOutcome::unconfigured()
        );
    }

    #[test]
    fn clean_build_yields_observed_empty() {
        // A clean OBSERVED build is Fetched of an empty set — the nudge
        // genuinely resolves, unlike a failed run.
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("Cargo.toml")
            .cmd("cargo check --message-format json", CLEAN);
        let cfg = SourceConfig::for_kind(TestKind::CargoWarnings);
        let PollOutcome::Fetched(out) = CargoWarnings::new(TestKind::CargoWarnings).poll(&env, &cfg)
        else {
            panic!("an observed clean build is Fetched");
        };
        assert!(out.is_empty());
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No workspace param + no manifest at the process cwd → the source
        // needs config (the GUI-launch cwd is `/`), never an eternal Error.
        let cfg = SourceConfig::for_kind(TestKind::CargoWarnings);
        assert_eq!(
            CargoWarnings::new(TestKind::CargoWarnings).poll(&MockEnvironment::new(), &cfg),
            PollOutcome::unconfigured()
        );
        // Manifest present but cargo missing/failing (compile errors exit
        // non-zero through this seam) → Error, never an empty Fetched — the
        // last-known warnings stay and age out by TTL.
        let env = MockEnvironment::new().path("Cargo.toml");
        assert_eq!(
            CargoWarnings::new(TestKind::CargoWarnings).poll(&env, &cfg),
            PollOutcome::error()
        );
    }
}
