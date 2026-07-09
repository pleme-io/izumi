//! The session-spawn action payload — mado's original
//! [`Payload`](crate::payload::Payload) (see [`crate::payload`]), kept in the
//! substrate because "Enter opens a
//! session at a cwd with an optional kickoff command" is the common shape
//! terminal-board consumers share.

use std::path::{Path, PathBuf};

use crate::prewarm::{reject_injection, PrewarmSpec, PrewarmStep};

/// Everything needed to turn an item into a live session — the
/// always-spawnable contract.
///
/// Two ingresses, both validated: [`SpawnSpec::new`] rejects an empty cwd/name,
/// and deserialization routes through `#[serde(try_from = "SpawnSpecWire")]` —
/// the same `new` check — so a persisted snapshot or config can't reintroduce an
/// un-spawnable target. The fields are private, so the only unchecked path is a
/// struct literal *inside this crate*; outside it there is none.
///
/// Tier (per UNREPRESENTABILITY): **parse-time-rejected** on the deserialize
/// boundary + sealed construction in-crate — not truly-unrepresentable (a
/// crate-internal struct literal could still build a blank one), but no
/// board-reachable row can be shown-but-not-acted-on.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "SpawnSpecWire")]
pub struct SpawnSpec {
    cwd: PathBuf,
    name: String,
    initial_command: Option<String>,
    /// An ordered prewarm strategy — the multi-step generalization of
    /// `initial_command` (kube-context / describe / open-link on session
    /// birth). Additive + wire-invisible when empty: `#[serde(default)]` loads
    /// pre-prewarm snapshots unchanged, and `skip_serializing_if` emits NO
    /// `prewarm` key for an empty spec, so a SpawnSpec with no strategy
    /// serializes byte-identically to before this field existed (golden
    /// fixtures unchanged). `initial_command` stays a back-compat convenience
    /// that [`Self::prewarm`] folds in as the first `RunCommand`.
    #[serde(default, skip_serializing_if = "PrewarmSpec::is_empty")]
    prewarm: PrewarmSpec,
}

/// Untrusted wire shape for [`SpawnSpec`]. The `TryFrom` runs the same
/// validation as [`SpawnSpec::new`], so deserialization can't bypass the
/// always-spawnable invariant.
#[derive(serde::Deserialize)]
struct SpawnSpecWire {
    cwd: PathBuf,
    name: String,
    #[serde(default)]
    initial_command: Option<String>,
    /// Additive — old snapshots (no `prewarm` key) default to an empty spec, so
    /// the wire form stays byte-compatible.
    #[serde(default)]
    prewarm: PrewarmSpec,
}

impl TryFrom<SpawnSpecWire> for SpawnSpec {
    type Error = String;
    fn try_from(w: SpawnSpecWire) -> Result<Self, Self::Error> {
        let mut spec = SpawnSpec::new(w.cwd, w.name)
            .ok_or_else(|| String::from("SpawnSpec: cwd and name must be non-empty"))?;
        if let Some(c) = w.initial_command {
            spec = spec.with_command(c);
        }
        spec.prewarm = w.prewarm;
        Ok(spec)
    }
}

impl SpawnSpec {
    /// Build a spawn target. `None` if `name` is blank or `cwd` is empty — the
    /// only ingress, so a constructed `SpawnSpec` is always actionable.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, name: impl Into<String>) -> Option<Self> {
        let cwd = cwd.into();
        let name = name.into();
        if name.trim().is_empty() || cwd.as_os_str().is_empty() {
            return None;
        }
        Some(Self {
            cwd,
            name,
            initial_command: None,
            prewarm: PrewarmSpec::default(),
        })
    }

    /// Attach a command to type into the fresh session (e.g. `gh pr checkout
    /// 1234`). A blank command is ignored — and so is any command carrying a
    /// control byte (`\n`, `\r`, ESC, …): the kickoff is delivered as PTY
    /// keystrokes + one Enter, so an embedded newline in upstream data (a
    /// ticket summary, an alert label) would EXECUTE a second command. That
    /// injection is rejected at this typed border (the shared
    /// [`reject_injection`](crate::prewarm::reject_injection) guard), not
    /// per-provider.
    #[must_use]
    pub fn with_command(mut self, cmd: impl Into<String>) -> Self {
        self.initial_command = reject_injection(&cmd.into());
        self
    }

    /// Attach an ordered prewarm strategy — the multi-step generalization of
    /// [`Self::with_command`] (set kube-context, describe the subject, open a
    /// deep-link on session birth).
    #[must_use]
    pub fn with_prewarm(mut self, prewarm: PrewarmSpec) -> Self {
        self.prewarm = prewarm;
        self
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn initial_command(&self) -> Option<&str> {
        self.initial_command.as_deref()
    }

    /// The full ordered prewarm strategy to run at session birth: the legacy
    /// `initial_command` (if any) folded in as the FIRST `RunCommand`, then the
    /// richer [`Self::with_prewarm`] steps. So a consumer runs ONE ordered list
    /// whether the item carries a single command, a rich strategy, or both.
    #[must_use]
    pub fn prewarm(&self) -> PrewarmSpec {
        let mut spec = self.prewarm.clone();
        if let Some(cmd) = &self.initial_command {
            if let Some(step) = PrewarmStep::run(cmd) {
                spec.push_front(step);
            }
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn spawnspec_rejects_empty() {
        assert!(SpawnSpec::new("", "name").is_none());
        assert!(SpawnSpec::new("/x", "  ").is_none());
        assert!(SpawnSpec::new("/x", "ok").is_some());
    }

    #[test]
    fn with_command_rejects_control_bytes_pty_injection() {
        let spec = || SpawnSpec::new("/x", "s").unwrap();
        // A clean command sticks.
        assert_eq!(
            spec().with_command("gh pr checkout 1").initial_command(),
            Some("gh pr checkout 1")
        );
        // An embedded newline would execute a SECOND command through the
        // kickoff keystrokes — the typed border drops the whole command.
        assert_eq!(
            spec().with_command("ls\nrm -rf /").initial_command(),
            None,
            "newline injection is unconstructible"
        );
        assert_eq!(spec().with_command("a\rb").initial_command(), None);
        assert_eq!(spec().with_command("a\u{1b}[2Jb").initial_command(), None);
        // The deserialize ingress runs the same border.
        let wire = r#"{"cwd":"/x","name":"s","initial_command":"ls\nrm -rf /"}"#;
        let s: SpawnSpec = serde_json::from_str(wire).unwrap();
        assert_eq!(s.initial_command(), None, "wire path can't smuggle control bytes");
    }

    #[test]
    fn spawnspec_deserialize_enforces_the_invariant() {
        // A valid wire shape round-trips through the try_from validation.
        let ok: Result<SpawnSpec, _> =
            serde_json::from_str(r#"{"cwd":"/code","name":"work","initial_command":null}"#);
        assert!(ok.is_ok());
        // A blank name on the wire is REJECTED — deserialization can no longer
        // bypass `new` and reintroduce an un-spawnable target.
        let bad_name: Result<SpawnSpec, _> =
            serde_json::from_str(r#"{"cwd":"/code","name":"   "}"#);
        assert!(bad_name.is_err(), "blank name must fail to deserialize");
        // An empty cwd is likewise rejected.
        let bad_cwd: Result<SpawnSpec, _> =
            serde_json::from_str(r#"{"cwd":"","name":"work"}"#);
        assert!(bad_cwd.is_err(), "empty cwd must fail to deserialize");
        // Round-trip: serialize a built spec, deserialize it back unchanged.
        let spec = SpawnSpec::new("/code", "work").unwrap().with_command("ls");
        let json = serde_json::to_string(&spec).unwrap();
        let back: SpawnSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    proptest! {
        #[test]
        fn spawnspec_some_iff_cwd_and_name_nonblank(cwd in ".*", name in ".*") {
            let made = SpawnSpec::new(cwd.clone(), name.clone()).is_some();
            let expect = !name.trim().is_empty() && !cwd.is_empty();
            prop_assert_eq!(made, expect);
        }
    }
}
