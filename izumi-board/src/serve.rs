//! The daemon face — engine + maintenance loop + (unix) control socket.
//!
//! `izumi-board serve` builds a multi-thread tokio runtime and runs three
//! planes over ONE shared [`izumi::Store`]:
//!
//! * the [`izumi::Engine`] — one paced watcher task per enabled source in
//!   the [`crate::registry`], with the shared freshness
//!   [`Notify`](tokio::sync::Notify) nudge
//!   (the `nudge` verb / socket request re-verifies the whole board NOW,
//!   paced per-watcher);
//! * the **maintenance loop** — the single owner of decay + gc + persist,
//!   off the watcher hot path (port of mado's suggest maintenance loop):
//!   every debounce tick (1s minimum) runs
//!   [`izumi::maintain::maintenance_tick`], then persists the snapshot to
//!   [`crate::state::snapshot_path`] under the [`crate::state::SNAPSHOT_MAGIC`]
//!   frame — gated on BOTH the store's change-generation (a heartbeat tick
//!   never touches disk) AND the [`izumi::writer::WriterElection`] (only the
//!   elected single writer persists; a loser re-contests every tick, so the
//!   writer seat re-fills the moment it frees);
//! * the **control socket** (unix only) — a tokio `UnixListener` at
//!   [`crate::state::socket_path`] serving the typed newline-delimited-JSON
//!   [`Request`]/[`Response`] protocol.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use izumi::{Catalog as _, Environment, ItemId, Store};
use izumi_config::BoardConfig;

use crate::catalog::BoardKind;
use crate::protocol::{HealthView, Request, Response, RowView};

/// The board's concrete store type — the one shared cache all three planes
/// touch.
pub type BoardStore = Store<BoardKind, izumi::SpawnSpec>;

/// A serve-time failure — the typed reasons the daemon refuses to start.
#[derive(Debug)]
pub enum ServeError {
    /// The tokio runtime could not be built.
    Runtime(std::io::Error),
    /// Another live daemon already answers on the control socket.
    AlreadyServing(PathBuf),
    /// The control socket could not be bound.
    Bind {
        path: PathBuf,
        err: std::io::Error,
    },
}

impl core::fmt::Display for ServeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ServeError::Runtime(err) => write!(f, "could not build the tokio runtime: {err}"),
            ServeError::AlreadyServing(path) => write!(
                f,
                "another izumi-board daemon is already serving {}",
                path.display()
            ),
            ServeError::Bind { path, err } => {
                write!(f, "could not bind the control socket {}: {err}", path.display())
            }
        }
    }
}

impl std::error::Error for ServeError {}

/// Config-derived maintenance knobs — the board daemon's copy of mado's
/// `LoopKnobs` (ported verbatim, generalized to [`BoardKind`] +
/// [`BoardConfig`]).
struct LoopKnobs {
    /// Per-source TTL: a source's items live for max(3× its poll interval,
    /// the global floor) — so a slow (e.g. hourly) source never flickers
    /// under a fast global TTL.
    ttl_map: BTreeMap<BoardKind, u64>,
    global_ttl_ms: u64,
    /// WRITING is additionally re-decided every maintenance tick via the
    /// single-writer election; this knob is the config half of that AND.
    persist: bool,
    max_entries: usize,
    /// 0 = "persist on every change" → a 1s minimum tick (tokio rejects a
    /// 0 interval); otherwise coalesce writes to this cadence.
    debounce: Duration,
}

impl LoopKnobs {
    fn from_config(cfg: &BoardConfig, engine_cfg: &izumi::EngineConfig<BoardKind>) -> Self {
        let global_ttl_ms = cfg.ttl_secs.saturating_mul(1000);
        let mut ttl_map: BTreeMap<BoardKind, u64> = BTreeMap::new();
        for &kind in BoardKind::ALL {
            let interval_ttl = engine_cfg
                .config_for(kind)
                .interval
                .as_secs()
                .saturating_mul(3)
                .saturating_mul(1000);
            ttl_map.insert(kind, interval_ttl.max(global_ttl_ms));
        }
        Self {
            ttl_map,
            global_ttl_ms,
            persist: cfg.persist,
            max_entries: cfg.max_entries,
            debounce: Duration::from_secs(cfg.persist_debounce_secs.max(1)),
        }
    }

    /// A source's effective TTL (the per-source map, falling back to the
    /// global floor for a kind that somehow missed the map).
    fn ttl_for(&self, kind: BoardKind) -> u64 {
        self.ttl_map.get(&kind).copied().unwrap_or(self.global_ttl_ms)
    }
}

/// The socket-facing service — the ONE handler the daemon's unix socket and
/// the in-process tests both drive (one border, two ingresses — the mado
/// `board_json`/`dismiss` idiom).
pub struct BoardService {
    store: Arc<BoardStore>,
    env: Arc<dyn Environment>,
    nudge: Arc<tokio::sync::Notify>,
}

impl BoardService {
    #[must_use]
    pub fn new(
        store: Arc<BoardStore>,
        env: Arc<dyn Environment>,
        nudge: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self { store, env, nudge }
    }

    /// The service's single clock read (ms).
    fn now_ms(&self) -> u64 {
        self.env.now_unix().saturating_mul(1000)
    }

    /// Handle one typed request — pure w.r.t. the store + nudge, so the
    /// whole protocol is testable without a socket.
    #[must_use]
    pub fn handle(&self, req: Request) -> Response {
        match req {
            Request::List { max } => Response::Rows(self.rows(max)),
            Request::Json { max } => Response::Board(self.board(max)),
            Request::Dismiss { id } => Response::Done {
                ok: parse_id(&id).is_some_and(|id| self.store.dismiss(id)),
            },
            Request::Snooze { id, secs } => Response::Done {
                ok: parse_id(&id).is_some_and(|id| {
                    self.store
                        .snooze(id, self.now_ms().saturating_add(secs.saturating_mul(1000)))
                }),
            },
            Request::Accept { id, session } => Response::Done {
                ok: parse_id(&id).is_some_and(|id| self.store.mark_accepted(id, &session)),
            },
            Request::Health => Response::Health(self.health()),
            Request::Nudge => {
                self.nudge.notify_waiters();
                Response::Done { ok: true }
            }
        }
    }

    /// The ranked offerable rows (living-board order; the clamp is mado's
    /// `board_json` bound).
    fn rows(&self, max: usize) -> Vec<RowView> {
        let now_ms = self.now_ms();
        self.store
            .ranked_stored(max.clamp(1, 200), now_ms)
            .iter()
            .map(|st| RowView::from_stored(st, now_ms))
            .collect()
    }

    /// Per-source poll health, catalog-ordered.
    fn health(&self) -> Vec<HealthView> {
        let now_ms = self.now_ms();
        self.store
            .health()
            .into_iter()
            .map(|(kind, h)| HealthView::from_health(kind, &h, now_ms))
            .collect()
    }

    /// The mado-shaped board JSON: rows under the wire key `suggestions`
    /// (the v1 agent-surface legacy name) + per-source health.
    fn board(&self, max: usize) -> serde_json::Value {
        serde_json::json!({
            "suggestions": self.rows(max),
            "health": self.health(),
        })
    }
}

/// Parse a board row id from its decimal-string wire form.
fn parse_id(id: &str) -> Option<ItemId> {
    id.trim().parse::<u64>().ok().map(ItemId)
}

/// Build the runtime and serve forever. Blocks the calling thread.
///
/// # Errors
///
/// [`ServeError::Runtime`] when the tokio runtime cannot be built;
/// [`ServeError::AlreadyServing`] / [`ServeError::Bind`] when the control
/// socket is owned by a live daemon or cannot be bound.
pub fn run() -> Result<(), ServeError> {
    let cfg = crate::config::load_board_config();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("izumi-board")
        .build()
        .map_err(ServeError::Runtime)?;
    rt.block_on(serve(cfg))
}

/// The async serve body: warm restart → engine → control socket →
/// maintenance loop (never returns while healthy).
async fn serve(cfg: BoardConfig) -> Result<(), ServeError> {
    let env: Arc<dyn Environment> = Arc::new(izumi::RealEnvironment::discover());
    let store: Arc<BoardStore> = Arc::new(Store::new());
    let nudge = Arc::new(tokio::sync::Notify::new());
    let engine_cfg = crate::config::engine_config(&cfg);
    let knobs = LoopKnobs::from_config(&cfg, &engine_cfg);
    let state_dir = crate::state::state_dir();
    let snapshot = crate::state::snapshot_path();

    // Warm restart: re-surface the last-known rows INSTANTLY (ages rebased
    // to the snapshot's save time), then age out anything already stale AT
    // SAVE, before the watchers re-poll. Deliberately NOT election-gated: a
    // loser instance loads too, so a second daemon boots with the live board
    // and the persisted dismissal stickiness intact.
    if knobs.persist {
        let now_ms = env.now_unix().saturating_mul(1000);
        store.load_file(&snapshot, crate::state::SNAPSHOT_MAGIC, now_ms);
        store.decay_per_source(now_ms, |k| knobs.ttl_for(k));
        store.gc(knobs.max_entries, now_ms);
    }

    // The master switch gates the ENGINE, not the daemon: a disabled board
    // still serves reads (last-known snapshot rows) + lifecycle over the
    // socket, it just never polls.
    let _engine = if cfg.enabled {
        let engine = izumi::Engine::start(
            crate::registry::registry(),
            Arc::clone(&env),
            Arc::clone(&store),
            engine_cfg,
            Some(Arc::clone(&nudge)),
        );
        tracing::info!(
            watchers = engine.active_watchers(),
            persist = knobs.persist,
            "izumi-board engine live"
        );
        Some(engine)
    } else {
        tracing::warn!("board disabled (enabled = false) — serving reads only, no watchers");
        None
    };

    #[cfg(unix)]
    {
        let service = Arc::new(BoardService::new(
            Arc::clone(&store),
            Arc::clone(&env),
            Arc::clone(&nudge),
        ));
        let sock = crate::state::socket_path();
        let listener = bind_control_socket(&sock)?;
        tracing::info!(socket = %sock.display(), "control socket live");
        tokio::spawn(serve_socket(listener, service));
    }

    let election = izumi::writer::WriterElection::new(&state_dir);
    maintenance_loop(&knobs, &store, env.as_ref(), &snapshot, &election).await;
    Ok(())
}

/// The maintenance loop — the SINGLE owner of decay + gc + the debounced,
/// generation-gated, election-gated snapshot persist (mado's suggest
/// maintenance loop, ported over [`izumi::maintain::maintenance_tick`]).
/// Never returns.
async fn maintenance_loop(
    knobs: &LoopKnobs,
    store: &BoardStore,
    env: &dyn Environment,
    snapshot: &std::path::Path,
    election: &izumi::writer::WriterElection,
) {
    let mut last_gen = store.generation();
    let mut tick = tokio::time::interval(knobs.debounce);
    loop {
        tick.tick().await;
        let now_ms = env.now_unix().saturating_mul(1000);
        izumi::maintain::maintenance_tick(store, |k| knobs.ttl_for(k), knobs.max_entries, now_ms);
        let current_gen = store.generation();
        // The writer election is RE-CHECKED every tick (a cheap non-blocking
        // flock attempt when not already held), so a surviving instance picks
        // up the writer role when the previous winner exits — persistence
        // never silently dies with the first process.
        if knobs.persist && current_gen != last_gen && election.check().is_writer() {
            store.persist_file(snapshot, crate::state::SNAPSHOT_MAGIC, now_ms);
            last_gen = current_gen;
        }
    }
}

/// Bind the control socket, refusing to steal it from a live daemon: a live
/// owner ANSWERS a connect (→ [`ServeError::AlreadyServing`]); a dead one
/// leaves a stale file we reclaim before binding.
#[cfg(unix)]
fn bind_control_socket(path: &std::path::Path) -> Result<tokio::net::UnixListener, ServeError> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(ServeError::AlreadyServing(path.to_owned()));
    }
    let _ = std::fs::remove_file(path);
    tokio::net::UnixListener::bind(path).map_err(|err| ServeError::Bind {
        path: path.to_owned(),
        err,
    })
}

/// Accept loop — one spawned task per connection.
#[cfg(unix)]
pub(crate) async fn serve_socket(
    listener: tokio::net::UnixListener,
    service: Arc<BoardService>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let svc = Arc::clone(&service);
                tokio::spawn(async move { handle_conn(stream, &svc).await });
            }
            Err(err) => {
                tracing::warn!(err = %err, "control-socket accept failed");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

/// One connection: newline-delimited JSON requests in, one JSON response
/// line per request out. A malformed line answers `Done{ok:false}` (typed,
/// never a silent drop); a write failure ends the connection.
#[cfg(unix)]
async fn handle_conn(stream: tokio::net::UnixStream, service: &BoardService) {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let (read, mut write) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => service.handle(req),
            Err(err) => {
                tracing::debug!(err = %err, "malformed control request");
                Response::Done { ok: false }
            }
        };
        let Ok(mut buf) = serde_json::to_vec(&resp) else {
            break;
        };
        buf.push(b'\n');
        if write.write_all(&buf).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use izumi::{Item, MockEnvironment, SpawnSpec};
    use shikumi::TieredConfig as _;

    use super::*;

    fn item(key: &str, title: &str) -> Item<BoardKind, SpawnSpec> {
        Item::new(
            BoardKind::TendRepos,
            key,
            title,
            SpawnSpec::new("/code/mado", "\u{1F9F9} mado")
                .unwrap()
                .with_command("git status"),
        )
    }

    fn service(store: &Arc<BoardStore>) -> BoardService {
        let env: Arc<dyn Environment> = Arc::new(MockEnvironment::new().at(2));
        BoardService::new(
            Arc::clone(store),
            env,
            Arc::new(tokio::sync::Notify::new()),
        )
    }

    /// The maintenance knobs mirror mado's: per-source TTL = max(3× poll
    /// interval, the global floor); the debounce floor is 1s.
    #[test]
    fn loop_knobs_derive_per_source_ttls_and_the_debounce_floor() {
        let mut cfg = BoardConfig::prescribed_default();
        cfg.ttl_secs = 900;
        cfg.persist_debounce_secs = 0;
        let knobs = LoopKnobs::from_config(&cfg, &crate::config::engine_config(&cfg));
        // tend-repos polls every 30s → 3× = 90s < the 900s floor → floor wins.
        assert_eq!(knobs.ttl_for(BoardKind::TendRepos), 900_000);
        // secret-age polls hourly → 3× = 3h > the floor → never flickers.
        assert_eq!(knobs.ttl_for(BoardKind::SecretAge), 3 * 3600 * 1000);
        assert_eq!(knobs.debounce, Duration::from_secs(1), "0 = every change → 1s tick");
    }

    /// The service handler end-to-end without a socket: list → lifecycle →
    /// health, ids as decimal strings throughout.
    #[test]
    fn service_handles_list_lifecycle_and_health() {
        let store: Arc<BoardStore> = Arc::new(Store::new());
        let row = item("mado", "mado — dirty");
        let id = row.id;
        store.ingest(BoardKind::TendRepos, vec![row], 1_000);
        store.record_poll(BoardKind::TendRepos, izumi::SourceStatus::Ok, 1_000);
        let svc = service(&store);

        let Response::Rows(rows) = svc.handle(Request::List { max: 10 }) else {
            panic!("list answers Rows");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id.0.to_string(), "decimal-string id");

        let Response::Board(board) = svc.handle(Request::Json { max: 10 }) else {
            panic!("json answers Board");
        };
        assert_eq!(board["suggestions"].as_array().map(Vec::len), Some(1));
        assert_eq!(board["health"][0]["status"], "ok");

        // Accept soft-acks; snooze hides; a bogus id is a typed refusal.
        assert_eq!(
            svc.handle(Request::Accept { id: id.0.to_string(), session: String::from("s") }),
            Response::Done { ok: true }
        );
        assert_eq!(
            svc.handle(Request::Dismiss { id: String::from("not-a-number") }),
            Response::Done { ok: false }
        );
        assert_eq!(
            svc.handle(Request::Dismiss { id: String::from("424242") }),
            Response::Done { ok: false },
            "an unknown id refuses (it may have decayed)"
        );

        let Response::Health(health) = svc.handle(Request::Health) else {
            panic!("health answers Health");
        };
        assert_eq!(health[0].source, "tend-repos");
        assert!(health[0].ever_ok);
    }

    /// A nudge wakes a waiter on the shared notify — the freshness path the
    /// engine's watchers select on. `notify_waiters` wakes only
    /// ALREADY-parked waiters, so the test nudges repeatedly until the
    /// waiter has provably parked and woken (no yield-timing assumption).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nudge_wakes_a_parked_waiter() {
        let store: Arc<BoardStore> = Arc::new(Store::new());
        let svc = service(&store);
        let nudge = Arc::clone(&svc.nudge);
        let waiter = tokio::spawn(async move { nudge.notified().await });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !waiter.is_finished() {
            assert!(std::time::Instant::now() < deadline, "the nudge never woke the waiter");
            assert_eq!(svc.handle(Request::Nudge), Response::Done { ok: true });
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        waiter.await.expect("waiter task");
    }

    /// The full socket round-trip: an in-process serve loop on a tempdir
    /// socket, a `Dismiss` over the wire, and the STORE actually mutates —
    /// the whole typed pipeline (frame → parse → handle → respond) in one
    /// test.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn control_socket_round_trips_and_mutates_the_store() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

        struct TestDir(PathBuf);
        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let mut name = String::from("izumi-board-sock-test-");
        name.push_str(&std::process::id().to_string());
        let dir = TestDir(std::env::temp_dir().join(name));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        let sock = dir.0.join("board.sock");

        let store: Arc<BoardStore> = Arc::new(Store::new());
        let row = item("mado", "mado — dirty");
        let id = row.id;
        store.ingest(BoardKind::TendRepos, vec![row], 1_000);
        let svc = Arc::new(service(&store));
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(serve_socket(listener, svc));

        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(read).lines();
        let send = |req: Request| {
            let mut buf = serde_json::to_vec(&req).unwrap();
            buf.push(b'\n');
            buf
        };

        // List → the ingested row, id as a decimal string.
        write.write_all(&send(Request::List { max: 10 })).await.unwrap();
        let line = lines.next_line().await.unwrap().unwrap();
        let Response::Rows(rows) = serde_json::from_str(&line).unwrap() else {
            panic!("list answers Rows: {line}");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id.0.to_string());

        // Dismiss over the wire → acknowledged AND the store mutated.
        write
            .write_all(&send(Request::Dismiss { id: id.0.to_string() }))
            .await
            .unwrap();
        let line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(resp, Response::Done { ok: true });
        assert!(store.is_dismissed(id), "the socket dismiss reached the store");

        // The dismissed row no longer lists; a malformed line answers a
        // typed refusal instead of hanging the connection.
        write.write_all(&send(Request::List { max: 10 })).await.unwrap();
        let line = lines.next_line().await.unwrap().unwrap();
        let Response::Rows(rows) = serde_json::from_str(&line).unwrap() else {
            panic!("list answers Rows: {line}");
        };
        assert!(rows.is_empty(), "dismissed rows never surface");
        write.write_all(b"{\"not\":\"a request\"}\n").await.unwrap();
        let line = lines.next_line().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Response>(&line).unwrap(),
            Response::Done { ok: false }
        );
    }
}
