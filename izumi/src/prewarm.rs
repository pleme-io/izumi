//! Session pre-warming — the ordered generalization of [`SpawnSpec`]'s single
//! `initial_command`.
//!
//! A [`PrewarmSpec`] is an ordered list of typed [`PrewarmStep`]s that a board
//! consumer runs **at session birth** so choosing an item's session lands the
//! operator already set up for it: point a tool at the right context, surface
//! the subject, open a deep-link. It lives in the substrate (beside
//! [`SpawnSpec`](crate::spawn::SpawnSpec)) because "Enter opens a session and
//! does terminal setup" is the shared terminal-board shape — every izumi source
//! (github / jira / grafana / k8s / flux) can attach one.
//!
//! # Injection defense (the same border as [`SpawnSpec::with_command`])
//!
//! Each step that reaches a shell is delivered as PTY keystrokes + Enter, so an
//! embedded control byte (`\n`, ESC, …) in upstream data would EXECUTE a second
//! command. [`reject_injection`] — the shared guard `SpawnSpec::with_command`
//! also uses — rejects that at construction, so a [`PrewarmStep`] carrying an
//! un-runnable / injection-bearing command is **unrepresentable** (its
//! constructor returns `None`). The wire ingress
//! ([`PrewarmStepWire`]) runs the same check, so a persisted snapshot can't
//! smuggle one in.
//!
//! Tier (per UNREPRESENTABILITY): **parse-time-rejected** on the deserialize
//! boundary + sealed construction in-crate — the same grade as [`SpawnSpec`].
//!
//! [`SpawnSpec`]: crate::spawn::SpawnSpec
//! [`SpawnSpec::with_command`]: crate::spawn::SpawnSpec::with_command

/// Reject a command string that is blank or carries a control byte — the shared
/// PTY-newline-injection guard. Returns the trimmed-safe command, or `None` if
/// it must not reach a shell. Used by every keystroke-lowering [`PrewarmStep`]
/// constructor AND by [`SpawnSpec::with_command`](crate::spawn::SpawnSpec::with_command),
/// so the injection border is defined exactly once.
#[must_use]
pub fn reject_injection(cmd: &str) -> Option<String> {
    let c = cmd.trim();
    if c.is_empty() || c.chars().any(char::is_control) {
        None
    } else {
        Some(c.to_string())
    }
}

/// One step of a prewarm strategy. Each variant that reaches a shell is
/// **valid-by-construction** — the smart constructors reject injection, so an
/// un-runnable step has no code path.
///
/// Serialization uses [`PrewarmStepWire`] as the untrusted ingress
/// (`#[serde(try_from)]`), so a persisted step re-runs the same validation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "PrewarmStepWire", into = "PrewarmStepWire")]
pub enum PrewarmStep {
    /// Run a command in the session's shell (keystrokes + Enter). Built only via
    /// [`PrewarmStep::run`]; the command is injection-checked.
    RunCommand(String),
    /// Set an environment variable — applied **pre-spawn** (folded into the
    /// spawn env before the session exists; env can't be cleanly injected into
    /// a live shell). A `PrewarmSpec` is therefore NOT a uniform post-spawn
    /// sequence; the consumer partitions these out first.
    SetEnv { key: String, value: String },
    /// Set the kube-context. Typed sugar that lowers to `kubectl config
    /// use-context <ctx>`; the ctx flows through the same injection guard.
    KubeContext(String),
    /// Open a URL (a runbook / dashboard deep-link). `url::Url`-typed, so it is
    /// parse-time-rejected before it lands here.
    OpenUrl(url::Url),
}

impl PrewarmStep {
    /// Build a [`PrewarmStep::RunCommand`], rejecting blank / injection-bearing
    /// commands (`None`). The only way to construct one.
    #[must_use]
    pub fn run(cmd: &str) -> Option<Self> {
        reject_injection(cmd).map(PrewarmStep::RunCommand)
    }

    /// Build a [`PrewarmStep::KubeContext`], rejecting an unusable context name.
    #[must_use]
    pub fn kube_context(ctx: &str) -> Option<Self> {
        reject_injection(ctx).map(PrewarmStep::KubeContext)
    }

    /// Build a [`PrewarmStep::SetEnv`]. The key must be a non-empty, non-control
    /// identifier; the value is rejected only for control bytes (an env value
    /// may legitimately be empty).
    #[must_use]
    pub fn set_env(key: &str, value: &str) -> Option<Self> {
        let k = key.trim();
        if k.is_empty() || k.chars().any(char::is_control) || value.chars().any(char::is_control) {
            return None;
        }
        Some(PrewarmStep::SetEnv { key: k.to_string(), value: value.to_string() })
    }

    /// The shell command this step delivers as keystrokes, if any. `SetEnv`
    /// (pre-spawn) and `OpenUrl` (browser) return `None`.
    #[must_use]
    pub fn shell_command(&self) -> Option<String> {
        match self {
            PrewarmStep::RunCommand(c) => Some(c.clone()),
            PrewarmStep::KubeContext(ctx) => {
                let mut s = String::with_capacity(28 + ctx.len());
                s.push_str("kubectl config use-context ");
                s.push_str(ctx);
                Some(s)
            }
            PrewarmStep::SetEnv { .. } | PrewarmStep::OpenUrl(_) => None,
        }
    }
}

/// Untrusted wire shape for [`PrewarmStep`] — the `TryFrom` re-runs the smart
/// constructors so a persisted step can't bypass the injection guard.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PrewarmStepWire {
    RunCommand { command: String },
    SetEnv { key: String, value: String },
    KubeContext { context: String },
    OpenUrl { url: url::Url },
}

impl TryFrom<PrewarmStepWire> for PrewarmStep {
    type Error = String;
    fn try_from(w: PrewarmStepWire) -> Result<Self, Self::Error> {
        match w {
            PrewarmStepWire::RunCommand { command } => {
                PrewarmStep::run(&command).ok_or_else(|| String::from("PrewarmStep::RunCommand: blank or control-byte command"))
            }
            PrewarmStepWire::KubeContext { context } => {
                PrewarmStep::kube_context(&context).ok_or_else(|| String::from("PrewarmStep::KubeContext: blank or control-byte context"))
            }
            PrewarmStepWire::SetEnv { key, value } => {
                PrewarmStep::set_env(&key, &value).ok_or_else(|| String::from("PrewarmStep::SetEnv: blank/control key or control value"))
            }
            PrewarmStepWire::OpenUrl { url } => Ok(PrewarmStep::OpenUrl(url)),
        }
    }
}

impl From<PrewarmStep> for PrewarmStepWire {
    fn from(s: PrewarmStep) -> Self {
        match s {
            PrewarmStep::RunCommand(command) => PrewarmStepWire::RunCommand { command },
            PrewarmStep::SetEnv { key, value } => PrewarmStepWire::SetEnv { key, value },
            PrewarmStep::KubeContext(context) => PrewarmStepWire::KubeContext { context },
            PrewarmStep::OpenUrl(url) => PrewarmStepWire::OpenUrl { url },
        }
    }
}

/// An ordered prewarm strategy — the multi-step generalization of a single
/// `initial_command`. Runs once, eagerly, at session birth.
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PrewarmSpec {
    steps: Vec<PrewarmStep>,
}

impl PrewarmSpec {
    /// A spec from an already-validated step list.
    #[must_use]
    pub fn new(steps: Vec<PrewarmStep>) -> Self {
        Self { steps }
    }

    /// A single command lowered to a one-step spec — the bridge that keeps
    /// [`SpawnSpec`](crate::spawn::SpawnSpec)'s `initial_command` working when
    /// no richer spec is set. Empty if the command is unusable.
    #[must_use]
    pub fn from_initial_command(cmd: &str) -> Self {
        Self { steps: PrewarmStep::run(cmd).into_iter().collect() }
    }

    /// The steps, in order.
    #[must_use]
    pub fn steps(&self) -> &[PrewarmStep] {
        &self.steps
    }

    /// True if there is nothing to prewarm (a bare session).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Prepend a step (used to fold a legacy `initial_command` in at the front).
    pub fn push_front(&mut self, step: PrewarmStep) {
        self.steps.insert(0, step);
    }

    /// The `SetEnv` steps — applied **pre-spawn**.
    pub fn env_steps(&self) -> impl Iterator<Item = (&str, &str)> {
        self.steps.iter().filter_map(|s| match s {
            PrewarmStep::SetEnv { key, value } => Some((key.as_str(), value.as_str())),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_injection_guards_control_bytes() {
        assert_eq!(reject_injection("kubectl get pods").as_deref(), Some("kubectl get pods"));
        assert_eq!(reject_injection("  trim  ").as_deref(), Some("trim"));
        assert_eq!(reject_injection(""), None);
        assert_eq!(reject_injection("a\nb"), None);
        assert_eq!(reject_injection("a\x1b[2J"), None);
    }

    #[test]
    fn run_and_kube_context_are_injection_safe() {
        assert!(PrewarmStep::run("kubectl describe pod api-0").is_some());
        assert!(PrewarmStep::run("x\nrm -rf /").is_none());
        assert_eq!(
            PrewarmStep::kube_context("rio").unwrap().shell_command().as_deref(),
            Some("kubectl config use-context rio")
        );
        assert!(PrewarmStep::kube_context("bad\nctx").is_none());
    }

    #[test]
    fn set_env_allows_empty_value_but_not_control_bytes() {
        assert!(PrewarmStep::set_env("K", "/v").is_some());
        assert!(PrewarmStep::set_env("EMPTY", "").is_some());
        assert!(PrewarmStep::set_env("", "x").is_none());
        assert!(PrewarmStep::set_env("K", "a\nb").is_none());
    }

    #[test]
    fn wire_round_trip_preserves_and_revalidates() {
        let spec = PrewarmSpec::new(vec![
            PrewarmStep::kube_context("rio").unwrap(),
            PrewarmStep::run("kubectl describe pod api-0").unwrap(),
            PrewarmStep::set_env("NS", "prod").unwrap(),
            PrewarmStep::OpenUrl(url::Url::parse("https://g.example/d/x").unwrap()),
        ]);
        let json = serde_json::to_string(&spec).unwrap();
        let back: PrewarmSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn wire_ingress_rejects_a_smuggled_control_byte() {
        // A persisted RunCommand carrying a newline must fail to deserialize —
        // the wire path runs the same injection border.
        let bad = r#"[{"kind":"run_command","command":"ls\nrm -rf /"}]"#;
        let r: Result<PrewarmSpec, _> = serde_json::from_str(bad);
        assert!(r.is_err(), "wire path can't smuggle control bytes");
    }

    #[test]
    fn env_steps_partition_out() {
        let spec = PrewarmSpec::new(vec![
            PrewarmStep::set_env("A", "1").unwrap(),
            PrewarmStep::run("echo hi").unwrap(),
            PrewarmStep::set_env("B", "2").unwrap(),
        ]);
        assert_eq!(spec.env_steps().collect::<Vec<_>>(), vec![("A", "1"), ("B", "2")]);
    }
}
