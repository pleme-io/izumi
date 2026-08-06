//! The side-effect boundary every source reads through — the
//! TYPED-SPEC+INTERPRETER triplet's `Environment` trait. A source's `poll`
//! does ALL its I/O through a `&dyn Environment`, so:
//!
//! * the real impl ([`RealEnvironment`]) shells typed [`Cmd`]s (NO shell —
//!   `std::process::Command` argv), does HTTP through `todoku` (the fleet's own
//!   HTTP primitive — in-process, NOT a subprocess), reads files, and resolves
//!   sops-rendered secrets; and
//! * tests drive a [`MockEnvironment`] of canned fixtures, so EVERY source is
//!   unit-tested without touching the network, a subprocess, or the cluster.
//!
//! Every method returns `Option` and is best-effort: a missing binary, a
//! non-2xx response, an absent secret, or an unauthed CLI all yield `None`,
//! never a panic — an unconfigured source simply contributes nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The latency class of a command — how long it may legitimately run before a
/// watcher treats it as hung and kills it. A single fixed global cap silently
/// kills a slow-but-working fleet aggregator (`tend status` over 100+ repos
/// ≈ 27 s > the 20 s cap → the source `error()`s on EVERY poll, forever, with
/// `ever_ok == false`). Making the budget a TYPED property of the `Cmd` removes
/// that whole mismatch class: a known-slow tool DECLARES [`Slow`](Self::Slow),
/// so "a 27 s tool under a 20 s cap" is no longer a hidden constant to
/// rediscover after 200 failed polls — it's a one-word choice at the call site,
/// visible in review. Still bounded (a truly-hung tool can never wedge forever).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LatencyBudget {
    /// Interactive / HTTP-ish (the `curl --max-time`-style default): a 20 s cap.
    #[default]
    Quick,
    /// A known-slow fleet aggregator (`tend status`, `kubectl` over many
    /// resources): a 60 s cap. Bounded, but wide enough for a real fleet scan.
    Slow,
}

impl LatencyBudget {
    /// The wall-clock cap this budget grants a subprocess.
    #[must_use]
    pub const fn cap(self) -> std::time::Duration {
        match self {
            Self::Quick => std::time::Duration::from_secs(20),
            Self::Slow => std::time::Duration::from_secs(60),
        }
    }
}

/// The typed outcome of running a [`Cmd`] — WHY it did or didn't produce output.
/// The lossy `Option<String>` of [`Environment::run`] collapsed a slow-tool
/// TIMEOUT into the same `None` as a spawn-failure / non-zero exit, so a source
/// could only report the generic [`SourceStatus::Error`] ("erroring") — the
/// operator couldn't tell "the tool is slow, raise its budget" from "the tool
/// is broken / missing". Making the failure MODE typed is what lets a source
/// report [`SourceStatus::TimedOut`] instead of a misleading `Error`.
///
/// [`SourceStatus::Error`]: crate::item::SourceStatus::Error
/// [`SourceStatus::TimedOut`]: crate::item::SourceStatus::TimedOut
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Ran to completion (exit 0) with this stdout.
    Produced(String),
    /// Exceeded its [`LatencyBudget`] cap and was killed — slow, not broken.
    TimedOut(LatencyBudget),
    /// Couldn't spawn, was rate-limited, or exited non-zero — broken / absent.
    Failed,
}

impl RunOutcome {
    /// The lossy projection to the legacy `Option<String>` (`Produced` → `Some`,
    /// every non-producing mode → `None`). What [`Environment::run`] returns.
    #[must_use]
    pub fn output(self) -> Option<String> {
        match self {
            Self::Produced(s) => Some(s),
            Self::TimedOut(_) | Self::Failed => None,
        }
    }
    /// Whether the command timed out (vs failed / produced) — the distinction
    /// the old `None` collapse erased.
    #[must_use]
    pub const fn timed_out(&self) -> bool {
        matches!(self, Self::TimedOut(_))
    }
}

/// A typed external command — program + argv (+ optional per-invocation env
/// vars + a typed latency budget), never a shell string. The one sanctioned
/// subprocess shape (per the NO SHELL law: a typed wrapper that constructs
/// `Command` from typed pieces).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    budget: LatencyBudget,
}

impl Cmd {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
            budget: LatencyBudget::Quick,
        }
    }

    /// Declare this command a known-slow fleet aggregator ([`LatencyBudget::Slow`]
    /// — a 60 s cap instead of the 20 s default). Use for a tool that scans the
    /// whole fleet (`tend status`, a broad `kubectl … -o json`) and legitimately
    /// runs tens of seconds — without this it times out under the default cap and
    /// the source `error()`s forever.
    #[must_use]
    pub fn slow(mut self) -> Self {
        self.budget = LatencyBudget::Slow;
        self
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
    /// This command's typed latency budget (default [`LatencyBudget::Quick`]).
    #[must_use]
    pub fn budget(&self) -> LatencyBudget {
        self.budget
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

/// A typed HTTP GET — url + headers. The real impl runs it through a cached
/// `todoku` client keyed on this request's (host, auth) fingerprint; tests mock
/// it by url.
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

    /// HTTP Basic auth (e.g. Atlassian Cloud: email + API token).
    /// [`todoku::BasicAuth`] does the base64, so izumi still needs no
    /// encoder of its own.
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
    /// Run a typed command, distinguishing WHY it produced no output — a
    /// slow-tool timeout ([`RunOutcome::TimedOut`]) vs a real failure
    /// ([`RunOutcome::Failed`]). The default is the LOSSY legacy path (`run` →
    /// `Produced`/`Failed`, never `TimedOut`); `RealEnvironment` overrides it so
    /// a slow-but-working tool is a distinct typed state a source can report as
    /// [`SourceStatus::TimedOut`] rather than a misleading `Error`.
    ///
    /// [`SourceStatus::TimedOut`]: crate::item::SourceStatus::TimedOut
    fn run_outcome(&self, cmd: &Cmd) -> RunOutcome {
        match self.run(cmd) {
            Some(s) => RunOutcome::Produced(s),
            None => RunOutcome::Failed,
        }
    }
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

/// The production environment: real subprocesses, in-process `todoku` HTTP,
/// filesystem, sops-rendered secrets under `~/.config/<category>/<name>`. All
/// upstream calls are paced per host through one shared
/// [`crate::pace::HostPacer`] (ONE `RealEnvironment` is shared as an `Arc`
/// across every watcher, so the buckets genuinely serialize the fan-out).
pub struct RealEnvironment {
    code_root: PathBuf,
    home: PathBuf,
    pacer: crate::pace::HostPacer,
    /// One `todoku` client per [`client_fingerprint`] — built on first use and
    /// kept, so a source's TLS handshake + connection pool survive across polls
    /// instead of being paid again on every tick the way a fresh `curl` process
    /// paid it. Shared behind the same `Arc<RealEnvironment>` every watcher
    /// holds, hence the mutex.
    http_clients: parking_lot::Mutex<BTreeMap<String, std::sync::Arc<todoku::BlockingHttpClient>>>,
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
            http_clients: parking_lot::Mutex::new(BTreeMap::new()),
            path,
            kubeconfig,
        }
    }

    /// The `todoku` client for this request's (host, auth) fingerprint — built
    /// on first use, then reused. `None` only if the client fails to build
    /// (a tokio runtime that can't be created), which the caller reports as the
    /// usual best-effort `None`.
    fn http_client(&self, req: &HttpReq) -> Option<std::sync::Arc<todoku::BlockingHttpClient>> {
        let key = client_fingerprint(req);
        if let Some(existing) = self.http_clients.lock().get(&key) {
            return Some(std::sync::Arc::clone(existing));
        }
        // Built OUTSIDE the lock — constructing a client spins up a tokio
        // runtime, and holding the map across that would stall every other
        // host's watcher behind one slow build.
        let built = std::sync::Arc::new(build_http_client(req)?);
        let mut map = self.http_clients.lock();
        // A racing watcher may have inserted this same fingerprint while we
        // built ours; keep whichever landed first so one fingerprint always
        // means exactly one connection pool.
        Some(std::sync::Arc::clone(map.entry(key).or_insert(built)))
    }
}

/// The latency class every [`HttpReq`] runs under.
///
/// [`HttpReq`] carries no per-request budget (27 sources construct it; its shape
/// is frozen), so the HTTP class's typed default IS the budget — which is
/// exactly what [`LatencyBudget::Quick`] documents itself as. This replaces the
/// `--max-time 10` literal the old curl argv carried: that number ignored the
/// typed budget the surrounding [`Cmd`] already declared, so one timeout was a
/// magic constant while the other was a type.
const HTTP_BUDGET: LatencyBudget = LatencyBudget::Quick;

/// The `User-Agent` every izumi HTTP call presents. A compile-time `concat!`,
/// not a `format!` (★★ TYPED EMISSION); upstream operators reading their access
/// logs see izumi and its version rather than `curl/8.x`.
const USER_AGENT: &str = concat!("izumi/", env!("CARGO_PKG_VERSION"));

/// The cache key for the `todoku` client a request needs: its host, then a
/// BLAKE3 digest of the auth material (headers + basic credentials).
///
/// The design problem this solves: todoku sets headers on the CLIENT builder,
/// not per request, while every izumi source carries its own token. One client
/// per distinct (host, auth) pair keeps connection pooling alive AND makes
/// sending source A's token to source B's host impossible — a client can only
/// ever be handed a URL on the host it was keyed to.
///
/// The auth material is HASHED, never embedded: this key is a map key that can
/// land in a debug print, and a bearer token must not. (Same reason
/// [`Cmd::key`] excludes env vars.)
fn client_fingerprint(req: &HttpReq) -> String {
    let mut hasher = blake3::Hasher::new();
    for (name, value) in req.headers() {
        hasher.update(name.as_bytes());
        hasher.update(b": ");
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    if let Some((user, pass)) = req.basic() {
        hasher.update(b"basic ");
        hasher.update(user.as_bytes());
        hasher.update(b":");
        hasher.update(pass.as_bytes());
    }
    let mut key = String::new();
    // A URL with no parseable host (the unpaced case) keys under a name no host
    // can collide with, so those calls still share one client per auth shape.
    key.push_str(crate::pace::host_of(req.url()).unwrap_or("<no-host>"));
    key.push(' ');
    key.push_str(&hasher.finalize().to_hex());
    key
}

/// Build a fresh `todoku` client carrying this request's auth as CLIENT-level
/// defaults — the resolution of todoku's builder-scoped `header()` against
/// izumi's per-request auth (see [`client_fingerprint`]).
///
/// Two deliberate settings:
///
/// * **No retry.** `get_response` takes todoku's `get_raw` path, which has no
///   retry loop; [`todoku::RetryPolicy::none`] says the same thing declaratively
///   so a later switch to `get`/`get_text` can't quietly stack todoku's backoff
///   on top of izumi's. The per-host [`crate::pace::HostPacer`] is izumi's
///   backoff governor, and re-sending a 429 three times is precisely what its
///   60 s cooldown exists to prevent.
/// * **SSRF guard off** (todoku's default). A grafana lane's `base_url` is
///   operator config and legitimately points at a private or tailscale-CGNAT
///   address; the guard rejects `10.0.0.0/8` and `100.64.0.0/10` outright, so
///   turning it on here would silently kill the self-hosted lanes the curl path
///   served. It belongs on a request whose host is untrusted input, which
///   izumi's never is.
fn build_http_client(req: &HttpReq) -> Option<todoku::BlockingHttpClient> {
    let mut builder = todoku::HttpClient::builder()
        .timeout(HTTP_BUDGET.cap())
        .user_agent(USER_AGENT)
        .retry(todoku::RetryPolicy::none());
    if let Some((user, pass)) = req.basic() {
        builder = builder.auth(todoku::BasicAuth::new(user, pass));
    }
    for (name, value) in req.headers() {
        match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
            Ok(typed) => builder = builder.header(typed, value),
            // Dropped rather than fatal: a header name is a source's own
            // literal, so this is a source bug, and the best-effort contract
            // says a broken source contributes nothing rather than panicking.
            Err(_) => tracing::debug!(header = name.as_str(), "dropping invalid header name"),
        }
    }
    todoku::BlockingHttpClient::from_builder(builder).ok()
}

/// What the honesty contract does with one response status. Lifted out of the
/// I/O so all of its tiers are testable without a network — notably the
/// GitHub-403 tier, which is otherwise only reachable by really calling
/// `api.github.com`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusVerdict {
    /// 2xx — the body IS the source's answer.
    Body,
    /// The host told us to back off: report to the pacer, then no body.
    Cooldown,
    /// Any other non-2xx — no body, no cooldown, logged at debug.
    NoBody,
}

/// Classify a response status for `host`.
///
/// The 403 split is the load-bearing case: GitHub signals a SECONDARY RATE
/// LIMIT with 403, so on that host a 403 must cool the bucket down — while on
/// any other host a 403 is authz, where cooling down would punish a
/// misconfigured token by hiding every other source on the same host for a
/// minute.
fn classify_status(status: u16, host: Option<&str>) -> StatusVerdict {
    match status {
        200..=299 => StatusVerdict::Body,
        429 => StatusVerdict::Cooldown,
        403 if host == Some(crate::pace::GH_HOST) => StatusVerdict::Cooldown,
        _ => StatusVerdict::NoBody,
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

impl Environment for RealEnvironment {
    fn run(&self, cmd: &Cmd) -> Option<String> {
        // The lossy legacy projection: Produced → Some, timeout/failure → None.
        self.run_outcome(cmd).output()
    }

    fn run_outcome(&self, cmd: &Cmd) -> RunOutcome {
        use std::io::Read;
        use std::process::Stdio;
        // How the wait loop ended — the typed distinction the old `None` erased.
        enum RunEnd {
            Exited(std::process::ExitStatus),
            TimedOut,
            Killed,
        }
        // Every `gh` invocation hits the GitHub API even though no URL is
        // visible here — bill it against the synthetic GH_HOST bucket so
        // the CLI sources and the raw-HTTP sources share one budget.
        if cmd.program() == "gh"
            && self.pacer.admit(crate::pace::GH_HOST) == crate::pace::Admit::CoolingDown
        {
            tracing::debug!("skipping gh call — GitHub API cooling down");
            return RunOutcome::Failed;
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
        let Ok(mut child) = builder
            .args(cmd.args_slice())
            .envs(cmd.envs_slice().iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return RunOutcome::Failed;
        };
        // Drain stdout on a thread so a full pipe buffer (a large `kubectl … -o
        // json`) can't deadlock the wait, and so the read finishes when the
        // child exits or is killed.
        let Some(mut out) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return RunOutcome::Failed;
        };
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        });
        let deadline = std::time::Instant::now() + cmd.budget().cap();
        let end = loop {
            match child.try_wait() {
                Ok(Some(status)) => break RunEnd::Exited(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        tracing::debug!(
                            program = cmd.program(),
                            budget = ?cmd.budget(),
                            "source command timed out"
                        );
                        break RunEnd::TimedOut;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break RunEnd::Killed;
                }
            }
        };
        let buf = reader.join().unwrap_or_default();
        match end {
            RunEnd::Exited(s) if s.success() => {
                RunOutcome::Produced(String::from_utf8_lossy(&buf).into_owned())
            }
            // Non-zero exit / abnormal kill = a broken/absent tool.
            RunEnd::Exited(_) | RunEnd::Killed => RunOutcome::Failed,
            // Slow-but-alive: killed for exceeding its budget, NOT broken.
            RunEnd::TimedOut => RunOutcome::TimedOut(cmd.budget()),
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
        // The request itself: one in-process todoku GET on the client cached
        // for this (host, auth) fingerprint. `get_response` hands back the
        // status as a typed `u16`, which is what retires the old `-w
        // "\n%{http_code}"` trailer — the status used to be recovered by
        // splitting the last line off stdout and re-parsing it, so every
        // classification below depended on a string convention holding.
        // Timeout is the typed [`HTTP_BUDGET`], not a literal.
        //
        // ONE disclosed delta from the curl path: reqwest follows redirects by
        // default and `curl` without `-L` did not, so a 3xx that used to reach
        // `classify_status` (and become `None`) is now followed to its target.
        // todoku's builder exposes no redirect knob, so this is inherited, not
        // chosen; the 3xx arm below is kept because it still governs a redirect
        // that exhausts reqwest's hop limit.
        let client = self.http_client(req)?;
        let resp = match client.get_response(req.url()) {
            Ok(resp) => resp,
            // A network failure is a `None` exactly as an exit≠0 curl was.
            Err(err) => {
                tracing::debug!(url = req.url(), error = %err, "http_get transport failure");
                return None;
            }
        };
        match classify_status(resp.status, host.as_deref()) {
            // Lossy UTF-8 — the same conversion the curl stdout path made, so a
            // response with a stray non-UTF-8 byte still reaches its parser.
            StatusVerdict::Body => Some(String::from_utf8_lossy(&resp.body).into_owned()),
            StatusVerdict::Cooldown => {
                if let Some(h) = host.as_deref() {
                    self.pacer.report_rate_limited(h, resp.status);
                }
                None
            }
            StatusVerdict::NoBody => {
                tracing::debug!(
                    status = resp.status,
                    url = req.url(),
                    "http_get non-success status"
                );
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
    /// Commands keyed by [`Cmd::key`] that report a [`RunOutcome::TimedOut`]
    /// (a slow-but-alive tool) — so a test can drive the timeout path without a
    /// real slow subprocess.
    timeouts: std::collections::BTreeSet<String>,
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
    /// Register a command (keyed by [`Cmd::key`]) that reports a timeout — its
    /// `run_outcome` is [`RunOutcome::TimedOut`] (and `run` is `None`). Drives
    /// the slow-tool path in tests without a real slow subprocess.
    #[must_use]
    pub fn cmd_timed_out(mut self, key: impl Into<String>) -> Self {
        self.timeouts.insert(key.into());
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
    fn run_outcome(&self, cmd: &Cmd) -> RunOutcome {
        let key = cmd.key();
        if self.timeouts.contains(&key) {
            RunOutcome::TimedOut(cmd.budget())
        } else {
            match self.cmds.get(&key) {
                Some(out) => RunOutcome::Produced(out.clone()),
                None => RunOutcome::Failed,
            }
        }
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
    fn cmd_latency_budget_defaults_quick_and_slow_widens_the_cap() {
        // The seal: a command's timeout budget is a typed property, not a hidden
        // global. Default is Quick (20s); a declared-Slow fleet tool gets 60s —
        // so a 27s `tend status` no longer times out + error()s on every poll.
        let quick = Cmd::new("gh").arg("pr").arg("list");
        assert_eq!(quick.budget(), LatencyBudget::Quick);
        assert_eq!(quick.budget().cap(), std::time::Duration::from_secs(20));

        let slow = Cmd::new("tend").arg("status").arg("--json").slow();
        assert_eq!(slow.budget(), LatencyBudget::Slow);
        assert_eq!(slow.budget().cap(), std::time::Duration::from_secs(60));
        // Slow strictly widens the window — a 27s tool fits Slow, not Quick.
        let tend_runtime = std::time::Duration::from_secs(27);
        assert!(tend_runtime > quick.budget().cap(), "27s tool times out under Quick");
        assert!(tend_runtime < slow.budget().cap(), "27s tool fits under Slow");
    }

    #[test]
    fn run_outcome_distinguishes_timeout_from_failure_and_projects_lossily() {
        // The seal: a timeout is a DISTINCT typed state, never the same value as
        // a failure — so a slow-but-working tool can never masquerade as broken.
        let slow_cmd = Cmd::new("tend").arg("status").slow();
        let env = MockEnvironment::new().cmd_timed_out(slow_cmd.key());
        let outcome = env.run_outcome(&slow_cmd);
        assert!(outcome.timed_out(), "a timed-out command reports TimedOut");
        assert_eq!(outcome, RunOutcome::TimedOut(LatencyBudget::Slow));
        // The lossy projection still collapses to None (backward-compat `run`).
        assert_eq!(env.run(&slow_cmd), None);
        assert_eq!(RunOutcome::TimedOut(LatencyBudget::Slow).output(), None);

        // A produced command is Produced + Some; an unknown one is Failed + None
        // (and NOT timed_out — the distinction the old `None` collapse erased).
        let ok_cmd = Cmd::new("gh").arg("pr").arg("list");
        let env2 = MockEnvironment::new().cmd(ok_cmd.key(), "out");
        assert_eq!(env2.run_outcome(&ok_cmd), RunOutcome::Produced("out".into()));
        assert_eq!(env2.run(&ok_cmd), Some("out".to_owned()));
        let missing = Cmd::new("nope");
        assert_eq!(env2.run_outcome(&missing), RunOutcome::Failed);
        assert!(!RunOutcome::Failed.timed_out());
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

    // ── The HTTP capability: todoku, not a curl subprocess ──────────────
    //
    // `http_get` used to spawn `curl -s -w "\n%{http_code}"` and recover the
    // status by splitting the last line off stdout. The tests below pin the
    // part that must survive the swap byte-for-byte: the honesty contract.

    /// A one-shot loopback HTTP server answering `status` + `body`, then closing.
    /// Real sockets and the real todoku client — so the end-to-end tests
    /// exercise the actual path rather than a stand-in for it.
    fn canned_server(status: u16, body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut sock, _)) = listener.accept() {
                // Drain the request line + headers; the content is irrelevant,
                // but reqwest will not read the response until its write lands.
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let mut resp = String::from("HTTP/1.1 ");
                resp.push_str(&status.to_string());
                resp.push_str(" STATUS\r\nContent-Length: ");
                resp.push_str(&body.len().to_string());
                resp.push_str("\r\nConnection: close\r\n\r\n");
                resp.push_str(body);
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        let mut url = String::from("http://127.0.0.1:");
        url.push_str(&port.to_string());
        url.push_str("/probe");
        url
    }

    #[test]
    fn classify_status_preserves_every_honesty_tier() {
        use StatusVerdict::{Body, Cooldown, NoBody};
        let gh = Some(crate::pace::GH_HOST);
        let other = Some("jira.example.com");

        // 2xx → the body is the answer.
        assert_eq!(classify_status(200, other), Body);
        assert_eq!(classify_status(204, other), Body);
        assert_eq!(classify_status(299, other), Body);

        // 429 → back off, on every host.
        assert_eq!(classify_status(429, other), Cooldown);
        assert_eq!(classify_status(429, gh), Cooldown);
        assert_eq!(classify_status(429, None), Cooldown);

        // 403 splits by host, and this is the whole reason the tier exists:
        // GitHub signals a SECONDARY RATE LIMIT with 403, so it cools down…
        assert_eq!(classify_status(403, gh), Cooldown);
        // …while on any other host a 403 is authz. Cooling down there would
        // hide every OTHER source on that host for a minute over one bad token.
        assert_eq!(classify_status(403, other), NoBody);
        assert_eq!(classify_status(403, None), NoBody);

        // Everything else non-2xx: no body, no cooldown.
        for status in [301u16, 400, 401, 404, 418, 500, 502, 503] {
            assert_eq!(classify_status(status, other), NoBody, "status {status}");
            assert_eq!(classify_status(status, gh), NoBody, "status {status} on gh");
        }
    }

    #[test]
    fn client_fingerprint_separates_host_and_auth_without_embedding_the_token() {
        let gh_a = HttpReq::new("https://api.github.com/issues").bearer("tok-a");
        let gh_a_other_path = HttpReq::new("https://api.github.com/pulls?q=1").bearer("tok-a");
        let gh_b = HttpReq::new("https://api.github.com/issues").bearer("tok-b");
        let elsewhere_a = HttpReq::new("https://api.other.com/issues").bearer("tok-a");

        // The path is NOT part of the key — one pool per host is the point.
        assert_eq!(
            client_fingerprint(&gh_a),
            client_fingerprint(&gh_a_other_path)
        );
        // A different token is a different client (todoku's headers are
        // client-scoped, so sharing would send the wrong Authorization).
        assert_ne!(client_fingerprint(&gh_a), client_fingerprint(&gh_b));
        // And the same token on a second host is a different client, so a token
        // can never ride to a host it wasn't issued for.
        assert_ne!(client_fingerprint(&gh_a), client_fingerprint(&elsewhere_a));
        // The key is a map key that can reach a debug print: hashed, not verbatim.
        assert!(!client_fingerprint(&gh_a).contains("tok-a"));

        // Basic credentials participate on the same terms.
        let basic1 = HttpReq::new("https://jira.example/rest").basic_auth("me@x", "pw-one");
        let basic2 = HttpReq::new("https://jira.example/rest").basic_auth("me@x", "pw-two");
        assert_ne!(client_fingerprint(&basic1), client_fingerprint(&basic2));
        assert!(!client_fingerprint(&basic1).contains("pw-one"));

        // A url with no parseable host still keys (those calls go unpaced).
        assert!(client_fingerprint(&HttpReq::new("not-a-url")).starts_with("<no-host>"));
    }

    #[test]
    fn http_client_cache_holds_one_client_per_fingerprint() {
        // Why this matters: a fresh curl process paid a TLS handshake on every
        // poll. One cached client per (host, auth) keeps the pool warm instead.
        let env = RealEnvironment::discover();
        let first = HttpReq::new("https://api.example/one").bearer("tok-a");
        let second = HttpReq::new("https://api.example/two").bearer("tok-a");
        let other_token = HttpReq::new("https://api.example/one").bearer("tok-b");

        let a = env.http_client(&first).expect("client builds");
        let b = env.http_client(&second).expect("client builds");
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "same host+auth must reuse ONE client, or pooling is dead"
        );
        assert_eq!(env.http_clients.lock().len(), 1);

        let c = env.http_client(&other_token).expect("client builds");
        assert!(
            !std::sync::Arc::ptr_eq(&a, &c),
            "a new token gets its own client"
        );
        assert_eq!(env.http_clients.lock().len(), 2);
    }

    #[test]
    fn pacer_cooldown_short_circuits_before_any_request_is_prepared() {
        // The tier that must fire FIRST: a cooling host never reaches the wire.
        // The evidence is not just the `None` — it's that no client was even
        // built for that fingerprint, which is what proves the check preceded
        // the request rather than a DNS failure standing in for it.
        let env = RealEnvironment::discover();
        env.pacer.report_rate_limited("cooling.example", 429);
        assert_eq!(
            env.http_get(&HttpReq::new("https://cooling.example/x")),
            None
        );
        assert!(
            env.http_clients.lock().is_empty(),
            "a cooling host must short-circuit before the client is built"
        );
    }

    #[test]
    fn http_timeout_is_the_typed_latency_budget_not_a_literal() {
        // The curl argv hardcoded `--max-time 10` while the `Cmd` wrapping it
        // already declared a typed budget — two timeouts, one of them a magic
        // number that ignored the type. HTTP now takes its cap from the type.
        // This pins the derivation only: neither reqwest nor todoku exposes a
        // built client's timeout, so the wire-level cap is not asserted here.
        assert_eq!(HTTP_BUDGET, LatencyBudget::Quick);
        assert_eq!(HTTP_BUDGET.cap(), LatencyBudget::Quick.cap());
        assert_eq!(HTTP_BUDGET.cap(), std::time::Duration::from_secs(20));
    }

    #[test]
    fn http_get_returns_the_body_on_2xx_over_a_real_socket() {
        let env = RealEnvironment::discover();
        let url = canned_server(200, "{\"items\":[]}");
        assert_eq!(
            env.http_get(&HttpReq::new(url)).as_deref(),
            Some("{\"items\":[]}")
        );
    }

    #[test]
    fn http_get_on_500_is_none_and_leaves_the_host_admitted() {
        let env = RealEnvironment::discover();
        let url = canned_server(500, "boom");
        assert_eq!(env.http_get(&HttpReq::new(url)), None);
        assert_eq!(
            env.pacer.admit("127.0.0.1"),
            crate::pace::Admit::Proceed,
            "a 5xx is not a rate-limit signal"
        );
    }

    #[test]
    fn http_get_on_429_reports_the_pacer_then_returns_none() {
        let env = RealEnvironment::discover();
        let url = canned_server(429, "slow down");
        assert_eq!(env.http_get(&HttpReq::new(url)), None);
        assert_eq!(
            env.pacer.admit("127.0.0.1"),
            crate::pace::Admit::CoolingDown,
            "429 must cool the host down for the next 60s"
        );
    }

    #[test]
    fn http_get_on_403_off_github_is_authz_and_never_cools_the_host_down() {
        // The asymmetry, end to end: 127.0.0.1 is not the GitHub host, so this
        // 403 is a token problem — reported as `None`, with the bucket left
        // open so the other sources on that host keep working.
        let env = RealEnvironment::discover();
        let url = canned_server(403, "forbidden");
        assert_eq!(env.http_get(&HttpReq::new(url)), None);
        assert_eq!(
            env.pacer.admit("127.0.0.1"),
            crate::pace::Admit::Proceed,
            "a 403 off the GitHub host must NOT trigger a cooldown"
        );
    }

    #[test]
    fn http_get_on_a_dead_port_is_none() {
        // Network failure → `None`, exactly as an exit≠0 curl was. Port 9
        // (discard) with nothing listening refuses immediately, so this is
        // deterministic and offline.
        let env = RealEnvironment::discover();
        assert_eq!(env.http_get(&HttpReq::new("http://127.0.0.1:9/x")), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_get_works_on_the_refresh_loops_blocking_pool() {
        // The production call shape. `spawn_interval_refresh` runs every source
        // poll inside `tokio::task::spawn_blocking`, so `http_get` executes on a
        // thread that CAN see a runtime handle (`HostPacer::admit` relies on
        // that). todoku's blocking facade drives its own current-thread runtime
        // there, and `block_on` inside a live runtime's *async task* panics —
        // this pins that the blocking pool is not that case, so the swap from a
        // subprocess to an in-process client cannot panic the watcher.
        //
        // Measured 2026-08-06: calling this same body directly in the async test
        // body (no `spawn_blocking`) panics with "Cannot start a runtime from
        // within a runtime" — at `pace.rs:88`, the pacer's OWN `block_on`. So the
        // async-context rule already governed `http_get` before todoku arrived;
        // this change inherits it rather than introducing it.
        let url = canned_server(200, "{\"ok\":true}");
        let out = tokio::task::spawn_blocking(move || {
            assert!(
                tokio::runtime::Handle::try_current().is_ok(),
                "the blocking pool sees the runtime handle"
            );
            RealEnvironment::discover().http_get(&HttpReq::new(url))
        })
        .await
        .expect("the blocking task must not panic");
        assert_eq!(out.as_deref(), Some("{\"ok\":true}"));
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
