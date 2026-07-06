//! The side-effect boundary every source reads through — the
//! TYPED-SPEC+INTERPRETER triplet's `Environment` trait. A source's `poll`
//! does ALL its I/O through a `&dyn Environment`, so:
//!
//! * the real impl ([`RealEnvironment`]) shells typed [`Cmd`]s (NO shell —
//!   `std::process::Command` argv), does HTTP via a typed `curl` argv, reads
//!   files, and resolves sops-rendered secrets; and
//! * tests drive a [`MockEnvironment`] of canned fixtures, so EVERY source is
//!   unit-tested without touching the network, a subprocess, or the cluster.
//!
//! Every method returns `Option` and is best-effort: a missing binary, a
//! non-2xx response, an absent secret, or an unauthed CLI all yield `None`,
//! never a panic — an unconfigured source simply contributes nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A typed external command — program + argv (+ optional per-invocation env
/// vars), never a shell string. The one sanctioned subprocess shape (per the
/// NO SHELL law: a typed wrapper that constructs `Command` from typed pieces).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

impl Cmd {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for a in it {
            self.args.push(a.into());
        }
        self
    }

    /// Set an env var for this invocation only (e.g. `GH_TOKEN` from the
    /// sops-rendered secret so a GUI-launched process's `gh` is authed even
    /// though the GUI process env carries no token). Never logged.
    #[must_use]
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.envs.push((k.into(), v.into()));
        self
    }

    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }
    #[must_use]
    pub fn args_slice(&self) -> &[String] {
        &self.args
    }
    #[must_use]
    pub fn envs_slice(&self) -> &[(String, String)] {
        &self.envs
    }

    /// Stable lookup key (program + space-joined args) for [`MockEnvironment`].
    /// Env vars are deliberately excluded — fixtures key on WHAT runs, and a
    /// secret must never appear in a test-fixture key.
    #[must_use]
    pub fn key(&self) -> String {
        let mut k = String::new();
        k.push_str(&self.program);
        for a in &self.args {
            k.push(' ');
            k.push_str(a);
        }
        k
    }
}

/// A typed HTTP GET — url + headers. The real impl runs it through `curl -s`
/// (a typed argv, not a shell line); tests mock it by url.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpReq {
    url: String,
    headers: Vec<(String, String)>,
    basic: Option<(String, String)>,
}

impl HttpReq {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            basic: None,
        }
    }

    /// HTTP Basic auth (e.g. Atlassian Cloud: email + API token). curl
    /// computes the header so we need no base64 dependency.
    #[must_use]
    pub fn basic_auth(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.basic = Some((user.into(), pass.into()));
        self
    }

    #[must_use]
    pub fn basic(&self) -> Option<&(String, String)> {
        self.basic.as_ref()
    }

    #[must_use]
    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    /// Convenience: `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(self, token: &str) -> Self {
        let mut v = String::from("Bearer ");
        v.push_str(token);
        self.header("Authorization", v)
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

/// The mockable I/O boundary. Implemented once for real, once for tests.
pub trait Environment: Send + Sync {
    /// Run a typed command; `Some(stdout)` on exit-0, `None` otherwise.
    fn run(&self, cmd: &Cmd) -> Option<String>;
    /// HTTP GET; `Some(body)` on 2xx, `None` otherwise.
    fn http_get(&self, req: &HttpReq) -> Option<String>;
    /// Read a file to a string, `None` if absent/unreadable.
    fn read_file(&self, path: &Path) -> Option<String>;
    /// Whether a path exists (a source prefers a real cwd for its payload).
    fn path_exists(&self, path: &Path) -> bool;
    /// Resolve a sops-style secret by `category/name` (e.g.
    /// `atlassian/api-token`), `None` if not materialized.
    fn secret(&self, key: &str) -> Option<String>;
    /// Current unix seconds (injected for determinism + decay).
    fn now_unix(&self) -> u64;
    /// The operator's code root (`~/code`) for cwd resolution.
    fn code_root(&self) -> PathBuf;
    /// The operator's home directory.
    fn home(&self) -> PathBuf;
}

/// The production environment: real subprocesses, `curl` HTTP, filesystem,
/// sops-rendered secrets under `~/.config/<category>/<name>`. All upstream
/// calls are paced per host through one shared [`crate::pace::HostPacer`]
/// (ONE `RealEnvironment` is shared as an `Arc` across every watcher, so
/// the buckets genuinely serialize the fan-out).
pub struct RealEnvironment {
    code_root: PathBuf,
    home: PathBuf,
    pacer: crate::pace::HostPacer,
    /// The session-augmented `PATH` every child gets (see [`augmented_path`]).
    /// A GUI/Dock-launched consumer inherits the bare system `PATH`
    /// (`/usr/bin:/bin:/usr/sbin:/sbin`), where `gh`/`kubectl`/`tend`/… do
    /// not resolve — the same launch mode the `GH_TOKEN` secret fallback in
    /// the sources exists for. Discovered once; a per-[`Cmd`] `PATH` env
    /// still wins.
    path: String,
    /// The kubeconfig chain to hand children when the process env carries no
    /// `KUBECONFIG` (again the GUI-launch case). Follows the workspace
    /// convention `~/.kube/configs/* : ~/.kube/config : ~/.kube/credentials`.
    kubeconfig: Option<String>,
}

impl RealEnvironment {
    /// Discover roots from the environment (`PLEME_CODE_ROOT` override, else
    /// `~/code`) plus the session env a GUI-launched process lacks (`PATH`
    /// augmentation, kubeconfig chain).
    #[must_use]
    pub fn discover() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let code_root = std::env::var_os("PLEME_CODE_ROOT")
            .map_or_else(|| home.join("code"), PathBuf::from);
        let current_path = std::env::var("PATH").ok();
        let path = augmented_path(current_path.as_deref(), &home);
        let kubeconfig = if std::env::var_os("KUBECONFIG").is_some() {
            // The process already carries a chain — children inherit it.
            None
        } else {
            kubeconfig_chain(&kube_config_entries(&home.join(".kube")))
        };
        Self {
            code_root,
            home,
            pacer: crate::pace::HostPacer::gentle(),
            path,
            kubeconfig,
        }
    }
}

/// Well-known bin dirs a login shell puts on `PATH` but a Dock/GUI launch
/// does not (nix profiles, homebrew, the classic locals). Appended AFTER the
/// inherited `PATH` so an operator's own ordering always wins; harmless when
/// a dir is absent (resolution just skips it).
fn session_path_candidates(home: &Path) -> Vec<PathBuf> {
    let mut v = vec![home.join(".nix-profile/bin")];
    if let Some(user) = home.file_name() {
        v.push(
            PathBuf::from("/etc/profiles/per-user")
                .join(user)
                .join("bin"),
        );
    }
    v.extend(
        [
            "/run/current-system/sw/bin",
            "/nix/var/nix/profiles/default/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .map(PathBuf::from),
    );
    v
}

/// Compose the child `PATH`: the inherited entries first (order preserved),
/// then every [`session_path_candidates`] dir not already present. Pure —
/// unit-tested without touching the process env.
fn augmented_path(current: Option<&str>, home: &Path) -> String {
    let mut entries: Vec<String> = current
        .unwrap_or_default()
        .split(':')
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .collect();
    for cand in session_path_candidates(home) {
        let s = cand.to_string_lossy().into_owned();
        if !entries.contains(&s) {
            entries.push(s);
        }
    }
    entries.join(":")
}

/// The existing kubeconfig files under `~/.kube`, workspace-convention
/// order: `configs/*` (sorted) then `config` then `credentials`. The impure
/// half feeding [`kubeconfig_chain`].
fn kube_config_entries(kube: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(kube.join("configs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    for tail in ["config", "credentials"] {
        let p = kube.join(tail);
        if p.exists() {
            v.push(p);
        }
    }
    v
}

/// Compose the `KUBECONFIG` colon-chain from the discovered entries; `None`
/// when nothing was found (children then fall back to kubectl's own
/// default). Pure over its input.
fn kubeconfig_chain(entries: &[PathBuf]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let parts: Vec<String> = entries
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Some(parts.join(":"))
}

/// Wall-clock cap on a watcher subprocess — mirrors the `curl --max-time 10`
/// contract so a hung `kubectl`/`gh` (VPN down, unreachable cluster) can't wedge
/// its watcher or leak a blocking-pool thread for the process lifetime.
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl Environment for RealEnvironment {
    fn run(&self, cmd: &Cmd) -> Option<String> {
        use std::io::Read;
        use std::process::Stdio;
        // Every `gh` invocation hits the GitHub API even though no URL is
        // visible here — bill it against the synthetic GH_HOST bucket so
        // the CLI sources and the raw-HTTP sources share one budget.
        if cmd.program() == "gh"
            && self.pacer.admit(crate::pace::GH_HOST) == crate::pace::Admit::CoolingDown
        {
            tracing::debug!("skipping gh call — GitHub API cooling down");
            return None;
        }
        // Session env the child needs even under a GUI/Dock launch: the
        // augmented PATH (program resolution) and — when the process itself
        // has none — the conventional KUBECONFIG chain. Applied BEFORE the
        // Cmd's own envs so a per-invocation override always wins.
        let mut builder = Command::new(cmd.program());
        builder.env("PATH", &self.path);
        if let Some(kc) = &self.kubeconfig {
            builder.env("KUBECONFIG", kc);
        }
        let mut child = builder
            .args(cmd.args_slice())
            .envs(cmd.envs_slice().iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Drain stdout on a thread so a full pipe buffer (a large `kubectl … -o
        // json`) can't deadlock the wait, and so the read finishes when the
        // child exits or is killed.
        let mut out = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        });
        let deadline = std::time::Instant::now() + RUN_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        tracing::debug!(program = cmd.program(), "source command timed out");
                        break None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let buf = reader.join().unwrap_or_default();
        match status {
            Some(s) if s.success() => Some(String::from_utf8_lossy(&buf).into_owned()),
            _ => None,
        }
    }

    fn http_get(&self, req: &HttpReq) -> Option<String> {
        // Per-host pacing BEFORE the request; a host inside its 429
        // cooldown short-circuits to None, which the PollOutcome border
        // reports as Unavailable(Error) — last-known rows stay up.
        let host = crate::pace::host_of(req.url()).map(str::to_owned);
        if let Some(h) = host.as_deref()
            && self.pacer.admit(h) == crate::pace::Admit::CoolingDown
        {
            tracing::debug!(host = h, "skipping http_get — host cooling down");
            return None;
        }
        // curl as a typed argv: `-s` plus an explicit status trailer
        // (`-w "\n%{http_code}"` — the status is the guaranteed last
        // line) instead of `-f`, so 429/403 are VISIBLE and classified
        // rather than folded into a generic exit≠0. Network failures
        // still exit≠0 → run() → None. Bounded by max-time so a wedged
        // endpoint can't stall the watcher. No shell — argv only.
        let mut c = Cmd::new("curl")
            .arg("-s")
            .arg("-w")
            .arg("\n%{http_code}")
            .arg("--max-time")
            .arg("10");
        if let Some((user, pass)) = req.basic() {
            let mut up = String::new();
            up.push_str(user);
            up.push(':');
            up.push_str(pass);
            c = c.arg("-u").arg(up);
        }
        for (k, v) in req.headers() {
            let mut h = String::new();
            h.push_str(k);
            h.push_str(": ");
            h.push_str(v);
            c = c.arg("-H").arg(h);
        }
        c = c.arg(req.url());
        let raw = self.run(&c)?;
        let (body, status_line) = raw.rsplit_once('\n')?;
        let status: u16 = status_line.trim().parse().ok()?;
        match status {
            200..=299 => Some(body.to_owned()),
            429 => {
                if let Some(h) = host.as_deref() {
                    self.pacer.report_rate_limited(h, status);
                }
                None
            }
            // GitHub answers secondary rate limits with 403; on any other
            // host a 403 is authz (health reports it, no cooldown).
            403 if host.as_deref() == Some(crate::pace::GH_HOST) => {
                self.pacer.report_rate_limited(crate::pace::GH_HOST, status);
                None
            }
            _ => {
                tracing::debug!(status, url = req.url(), "http_get non-success status");
                None
            }
        }
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn secret(&self, key: &str) -> Option<String> {
        // sops-nix renders secrets to ~/.config/<category>/<name>.
        let p = self.home.join(".config").join(key);
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn code_root(&self) -> PathBuf {
        self.code_root.clone()
    }
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
}

/// A canned environment for unit tests — every source is tested by feeding it
/// fixture command output / HTTP bodies / files / secrets and asserting the
/// items it produces.
#[derive(Default, Clone)]
pub struct MockEnvironment {
    cmds: BTreeMap<String, String>,
    https: BTreeMap<String, String>,
    files: BTreeMap<PathBuf, String>,
    secrets: BTreeMap<String, String>,
    paths: std::collections::BTreeSet<PathBuf>,
    now: u64,
    code_root: PathBuf,
    home: PathBuf,
}

impl MockEnvironment {
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: 1_000_000,
            code_root: PathBuf::from("/code"),
            home: PathBuf::from("/home/op"),
            ..Self::default()
        }
    }

    /// Register stdout for a command keyed by [`Cmd::key`] (program + args).
    #[must_use]
    pub fn cmd(mut self, key: impl Into<String>, out: impl Into<String>) -> Self {
        self.cmds.insert(key.into(), out.into());
        self
    }
    #[must_use]
    pub fn http(mut self, url: impl Into<String>, body: impl Into<String>) -> Self {
        self.https.insert(url.into(), body.into());
        self
    }
    #[must_use]
    pub fn file(mut self, p: impl Into<PathBuf>, c: impl Into<String>) -> Self {
        self.files.insert(p.into(), c.into());
        self
    }
    /// Mark a path as existing (for `path_exists`).
    #[must_use]
    pub fn path(mut self, p: impl Into<PathBuf>) -> Self {
        self.paths.insert(p.into());
        self
    }
    #[must_use]
    pub fn secret_val(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.secrets.insert(k.into(), v.into());
        self
    }
    #[must_use]
    pub fn at(mut self, now: u64) -> Self {
        self.now = now;
        self
    }
    #[must_use]
    pub fn roots(mut self, code_root: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        self.code_root = code_root.into();
        self.home = home.into();
        self
    }
}

impl Environment for MockEnvironment {
    fn run(&self, cmd: &Cmd) -> Option<String> {
        self.cmds.get(&cmd.key()).cloned()
    }
    fn http_get(&self, req: &HttpReq) -> Option<String> {
        self.https.get(req.url()).cloned()
    }
    fn read_file(&self, path: &Path) -> Option<String> {
        self.files.get(path).cloned()
    }
    fn path_exists(&self, path: &Path) -> bool {
        self.paths.contains(path) || self.files.contains_key(path)
    }
    fn secret(&self, key: &str) -> Option<String> {
        self.secrets.get(key).cloned()
    }
    fn now_unix(&self) -> u64 {
        self.now
    }
    fn code_root(&self) -> PathBuf {
        self.code_root.clone()
    }
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_key_is_program_plus_args() {
        let c = Cmd::new("gh").arg("pr").arg("list").args(["--json", "title"]);
        assert_eq!(c.key(), "gh pr list --json title");
    }

    #[test]
    fn http_bearer_sets_auth_header() {
        let r = HttpReq::new("https://x").bearer("tok");
        assert_eq!(r.headers()[0].0, "Authorization");
        assert_eq!(r.headers()[0].1, "Bearer tok");
    }

    #[test]
    fn augmented_path_keeps_operator_order_and_appends_session_dirs() {
        let home = Path::new("/home/op");
        let p = augmented_path(Some("/custom/bin:/usr/bin"), home);
        let entries: Vec<&str> = p.split(':').collect();
        // Inherited entries first, order preserved.
        assert_eq!(entries[0], "/custom/bin");
        assert_eq!(entries[1], "/usr/bin");
        // Session dirs appended (nix profiles, per-user profile, brew, base).
        assert!(entries.contains(&"/home/op/.nix-profile/bin"));
        assert!(entries.contains(&"/etc/profiles/per-user/op/bin"));
        assert!(entries.contains(&"/run/current-system/sw/bin"));
        assert!(entries.contains(&"/opt/homebrew/bin"));
        assert!(entries.contains(&"/bin"));
        // No duplicates — /usr/bin was inherited, not re-appended.
        assert_eq!(entries.iter().filter(|e| **e == "/usr/bin").count(), 1);
    }

    #[test]
    fn augmented_path_from_the_bare_gui_env_still_resolves_everything() {
        // The Dock-launch case: bare system PATH in, nix profiles appended.
        let p = augmented_path(
            Some("/usr/bin:/bin:/usr/sbin:/sbin"),
            Path::new("/Users/op"),
        );
        assert!(p.starts_with("/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(p.contains("/etc/profiles/per-user/op/bin"));
        assert!(p.contains("/run/current-system/sw/bin"));
        // And a missing PATH entirely is safe.
        let bare = augmented_path(None, Path::new("/Users/op"));
        assert!(bare.contains("/usr/bin"));
        assert!(!bare.starts_with(':'));
    }

    #[test]
    fn kubeconfig_chain_joins_in_order_and_is_none_when_empty() {
        assert_eq!(kubeconfig_chain(&[]), None);
        let chain = kubeconfig_chain(&[
            PathBuf::from("/h/.kube/configs/rio"),
            PathBuf::from("/h/.kube/config"),
            PathBuf::from("/h/.kube/credentials"),
        ]);
        assert_eq!(
            chain.as_deref(),
            Some("/h/.kube/configs/rio:/h/.kube/config:/h/.kube/credentials")
        );
    }

    #[test]
    fn mock_env_serves_fixtures_and_none_otherwise() {
        let env = MockEnvironment::new()
            .cmd("gh pr list", "[]")
            .http("https://api/x", "{}")
            .secret_val("atlassian/api-token", "abc")
            .at(42);
        assert_eq!(env.run(&Cmd::new("gh").arg("pr").arg("list")).as_deref(), Some("[]"));
        assert_eq!(env.run(&Cmd::new("nope")), None);
        assert_eq!(env.http_get(&HttpReq::new("https://api/x")).as_deref(), Some("{}"));
        assert_eq!(env.http_get(&HttpReq::new("https://api/missing")), None);
        assert_eq!(env.secret("atlassian/api-token").as_deref(), Some("abc"));
        assert_eq!(env.secret("missing/key"), None);
        assert_eq!(env.now_unix(), 42);
    }
}
