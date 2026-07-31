//! The CLI verbs — socket-first, snapshot-fallback for the read-only pair.
//!
//! `list` / `json` dial the live daemon's control socket and, when no daemon
//! answers, degrade to a READ-ONLY parse of the persisted snapshot through
//! the catalog-erased [`izumi::raw`] reader (row-lenient: a row from a newer
//! catalog still shows; a corrupt row drops alone). The degradation is
//! NAMED on stderr via `tracing` — a stale board never masquerades as live.
//!
//! The degraded ordering is the item's STATIC rank (urgency ≫ score — the
//! [`izumi::Item::rank_key`] shape): the living-board aging + recurrence
//! escalations and the accepted-row soft-ack demotion need the typed store,
//! which only the daemon runs. Snoozed rows are hidden wholesale in the
//! fallback (the raw row carries no deadline to re-offer by).
//!
//! `dismiss` / `accept` / `nudge` are lifecycle MUTATIONS and are
//! socket-only: without a live daemon there is no store to mutate — editing
//! the snapshot behind the (possibly about-to-persist) daemon's back would
//! be a write race, so the verbs refuse cleanly instead.

use std::path::{Path, PathBuf};

use crate::protocol::{Request, Response, RowView};

/// A CLI-verb failure — the typed reasons a verb exits non-zero.
#[derive(Debug)]
pub enum CliError {
    /// A mutation verb found no live daemon on the control socket.
    NoDaemon(PathBuf),
    /// The daemon answered a different response variant than the request
    /// contract promises.
    UnexpectedResponse,
    /// The daemon refused the request (unknown or already-decayed id).
    Refused,
    /// Writing the output stream failed.
    Io(std::io::Error),
    /// Serializing the JSON output failed.
    Encode(serde_json::Error),
}

impl core::fmt::Display for CliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CliError::NoDaemon(path) => write!(
                f,
                "no live izumi-board daemon at {} — start `izumi-board serve`",
                path.display()
            ),
            CliError::UnexpectedResponse => {
                write!(f, "the daemon answered an unexpected response variant")
            }
            CliError::Refused => {
                write!(f, "the daemon refused the request (unknown or decayed id?)")
            }
            CliError::Io(err) => write!(f, "could not write output: {err}"),
            CliError::Encode(err) => write!(f, "could not encode output: {err}"),
        }
    }
}

impl std::error::Error for CliError {}

/// `izumi-board list [--max N]` — the ranked board, one typed
/// [`RowView`] line per row ([`Display`](core::fmt::Display) is the render
/// surface).
///
/// # Errors
///
/// [`CliError::Io`] on a broken output stream; [`CliError::UnexpectedResponse`]
/// on a protocol violation.
pub fn list(max: usize) -> Result<(), CliError> {
    use std::io::Write as _;
    let rows = match request(&Request::List { max }) {
        Some(Response::Rows(rows)) => rows,
        Some(_) => return Err(CliError::UnexpectedResponse),
        None => fallback_rows_from(&crate::state::snapshot_path(), max, wall_now_ms()),
    };
    let mut out = std::io::stdout().lock();
    for row in &rows {
        writeln!(out, "{row}").map_err(CliError::Io)?;
    }
    Ok(())
}

/// `izumi-board json [--max N]` — the mado-shaped board JSON
/// (`{"suggestions": […], "health": […]}`) on stdout.
///
/// The degraded (no-daemon) path emits an EMPTY `health`. Note what that
/// does and does not mean now that the snapshot carries a health plane: the
/// plane is on disk, but the degraded path reads through the catalog-erased
/// [`izumi::raw`] reader, which parses rows only. Empty here means *this
/// reader did not look*, and the consumer must read it that way — the same
/// unknown-is-not-ok rule the typed border enforces. Reporting a verdict
/// from a file also means reporting one from a stale record, which is why
/// `health` stays live-daemon-only.
///
/// # Errors
///
/// [`CliError::Io`] / [`CliError::Encode`] on output failure;
/// [`CliError::UnexpectedResponse`] on a protocol violation.
pub fn json(max: usize) -> Result<(), CliError> {
    use std::io::Write as _;
    let board = match request(&Request::Json { max }) {
        Some(Response::Board(board)) => board,
        Some(_) => return Err(CliError::UnexpectedResponse),
        None => {
            let rows = fallback_rows_from(&crate::state::snapshot_path(), max, wall_now_ms());
            serde_json::json!({ "suggestions": rows, "health": [] })
        }
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &board).map_err(CliError::Encode)?;
    writeln!(out).map_err(CliError::Io)
}

/// `izumi-board health` — per-source poll health, worst first, with
/// "never once succeeded" called out in words.
///
/// Returns the number of BLIND sources so the caller can exit non-zero.
/// That return value is the point of the verb: a source misconfigured on
/// the day it was wired reads `erroring`, which is exactly what a healthy
/// source reads during a transient blip — so the data that distinguishes
/// them (`ever_ok`) existed, was carried in the protocol, was answered by
/// the daemon, and was never rendered anywhere a human looks. Seven
/// sources stayed dead for 118+ consecutive polls behind that gap.
///
/// Live-daemon only, deliberately: the degraded (`json`) path reads the
/// persisted snapshot through the catalog-erased reader, which carries rows
/// and not health, and reporting "0 blind" from a source that cannot know
/// would rebuild the exact blindness this verb exists to remove.
///
/// WHAT EXIT 2 MEANS, and why it is narrower than it was: BLIND is a
/// *finding* — polled, and never once observed to succeed, over a window the
/// persisted health plane keeps across restarts. A source whose instrument
/// never ran is [`HealthVerdict::Unknown`](izumi::HealthVerdict::Unknown): it
/// is reported, in words, and it does NOT gate. Before the health plane was
/// persisted the two were indistinguishable, so a restart could turn a
/// merely-degraded source into a BLIND one and fail a CI step on a healthy
/// fleet. The count returned is the blind count only.
///
/// # Errors
///
/// [`CliError::NoDaemon`] without a live daemon; [`CliError::Io`] on a
/// broken output stream; [`CliError::UnexpectedResponse`] on a protocol
/// violation.
pub fn health() -> Result<usize, CliError> {
    use std::io::Write as _;
    let mut views = match request(&Request::Health) {
        Some(Response::Health(views)) => views,
        Some(_) => return Err(CliError::UnexpectedResponse),
        None => return Err(CliError::NoDaemon(crate::state::socket_path())),
    };

    // Worst first — what needs a human goes at the top, where a scrolling
    // operator actually sees it. Unknown sorts above Ok (it is a hole in the
    // instrument, worth a look) but below Degraded (which is at least a
    // measurement).
    views.sort_by_key(|v| match v.verdict() {
        crate::protocol::HealthVerdict::Blind => 0,
        crate::protocol::HealthVerdict::Degraded => 1,
        crate::protocol::HealthVerdict::Unknown => 2,
        crate::protocol::HealthVerdict::Ok => 3,
    });

    let blind = views
        .iter()
        .filter(|v| v.verdict().needs_intervention())
        .count();
    let unknown = views
        .iter()
        .filter(|v| v.verdict() == crate::protocol::HealthVerdict::Unknown)
        .count();

    let mut out = std::io::stdout().lock();
    for v in &views {
        writeln!(out, "{v}").map_err(CliError::Io)?;
    }
    if blind > 0 {
        writeln!(
            out,
            "\n{blind} source(s) have NEVER succeeded — these are declarations to fix, not outages to wait out."
        )
        .map_err(CliError::Io)?;
    }
    if unknown > 0 {
        writeln!(
            out,
            "\n{unknown} source(s) have never been polled — no verdict is available for them, so they do not gate."
        )
        .map_err(CliError::Io)?;
    }
    Ok(blind)
}

/// `izumi-board dismiss <id> [--snooze <secs>]` — socket-only lifecycle
/// mutation (dismiss forever, or snooze for `secs`).
///
/// # Errors
///
/// [`CliError::NoDaemon`] without a live daemon; [`CliError::Refused`] on an
/// unknown/decayed id.
pub fn dismiss(id: &str, snooze: Option<u64>) -> Result<(), CliError> {
    let req = match snooze {
        Some(secs) => Request::Snooze { id: id.to_owned(), secs },
        None => Request::Dismiss { id: id.to_owned() },
    };
    mutate(&req)
}

/// `izumi-board accept <id> <session>` — mark a row in-progress under
/// `session` (soft-ack: demoted, badged, never removed). Socket-only.
///
/// # Errors
///
/// [`CliError::NoDaemon`] without a live daemon; [`CliError::Refused`] on an
/// unknown/decayed id.
pub fn accept(id: &str, session: &str) -> Result<(), CliError> {
    mutate(&Request::Accept {
        id: id.to_owned(),
        session: session.to_owned(),
    })
}

/// `izumi-board nudge` — fire the freshness nudge so every watcher whose
/// data is older than its pacing gap re-polls right now. Socket-only.
///
/// # Errors
///
/// [`CliError::NoDaemon`] without a live daemon.
pub fn nudge() -> Result<(), CliError> {
    mutate(&Request::Nudge)
}

/// Send a mutation request and demand the `Done` contract.
fn mutate(req: &Request) -> Result<(), CliError> {
    match request(req) {
        Some(Response::Done { ok: true }) => Ok(()),
        Some(Response::Done { ok: false }) => Err(CliError::Refused),
        Some(_) => Err(CliError::UnexpectedResponse),
        None => Err(CliError::NoDaemon(crate::state::socket_path())),
    }
}

/// Wall-clock unix milliseconds — the CLI's single clock read (the daemon
/// reads through its `Environment`; the degraded path has none).
fn wall_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One request/response exchange over the control socket. `None` covers
/// every "no live daemon" shape (absent socket, refused connect, dead peer,
/// malformed answer) — the caller picks fallback vs refusal.
#[cfg(unix)]
fn request(req: &Request) -> Option<Response> {
    use std::io::{BufRead as _, BufReader, Write as _};
    let path = crate::state::socket_path();
    let stream = std::os::unix::net::UnixStream::connect(&path).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let mut writer = stream.try_clone().ok()?;
    let mut buf = serde_json::to_vec(req).ok()?;
    buf.push(b'\n');
    writer.write_all(&buf).ok()?;
    writer.flush().ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim_end()).ok()
}

/// The control socket is unix-only; elsewhere every verb sees "no daemon"
/// (reads degrade to the snapshot, mutations refuse — typed, never silent).
#[cfg(not(unix))]
fn request(_req: &Request) -> Option<Response> {
    None
}

/// The degraded read path: parse the persisted snapshot READ-ONLY through
/// the catalog-erased [`izumi::raw`] reader under the board magic. A
/// missing/torn/foreign snapshot is an EMPTY board (typed start-empty, the
/// persist contract), never garbage rows. Filters the dismissed + the
/// snoozed, orders by static rank (see the module docs for what the
/// degraded ordering gives up).
fn fallback_rows_from(snapshot: &Path, max: usize, now_ms: u64) -> Vec<RowView> {
    tracing::warn!(
        snapshot = %snapshot.display(),
        "no live izumi-board daemon — reading the persisted snapshot read-only (rows may be stale)"
    );
    let Some(snap) = izumi::raw::read_raw_snapshot_file(snapshot, crate::state::SNAPSHOT_MAGIC)
    else {
        return Vec::new();
    };
    let mut rows: Vec<&izumi::raw::RawStoredItem> = snap
        .entries
        .iter()
        .filter(|r| r.state_kind != "dismissed" && r.state_kind != "snoozed")
        .collect();
    rows.sort_by(|a, b| {
        raw_rank_key(b)
            .cmp(&raw_rank_key(a))
            .then(a.first_seen_ms.cmp(&b.first_seen_ms))
            .then(a.id.cmp(&b.id))
    });
    rows.truncate(max.clamp(1, 200));
    rows.into_iter().map(|r| RowView::from_raw(r, now_ms)).collect()
}

/// The static composite rank of a raw row — urgency weight in the high bits
/// dominates, score in the low bits breaks ties (the [`izumi::Item::rank_key`]
/// shape over the wire slugs; an unknown urgency slug ranks Idle).
fn raw_rank_key(r: &izumi::raw::RawStoredItem) -> u64 {
    (u64::from(urgency_weight(&r.urgency)) << 20) | u64::from(r.score.min(1000))
}

/// The wire-slug twin of [`izumi::Urgency::weight`] — pinned by test so the
/// degraded ordering can never drift from the typed scale.
fn urgency_weight(slug: &str) -> u32 {
    match slug {
        "critical" => 1000,
        "high" => 750,
        "normal" => 500,
        "low" => 250,
        _ => 0, // idle + anything a future catalog invents
    }
}

#[cfg(test)]
mod tests {
    use crate::catalog::BoardKind;
    use izumi::{Item, ItemId, SpawnSpec, Store, Urgency};

    use super::*;

    /// The slug-keyed weight table matches the typed scale exactly.
    #[test]
    fn urgency_weight_matches_the_typed_scale() {
        for u in [
            Urgency::Idle,
            Urgency::Low,
            Urgency::Normal,
            Urgency::High,
            Urgency::Critical,
        ] {
            assert_eq!(
                urgency_weight(crate::protocol::urgency_slug(u)),
                u.weight(),
                "{u:?}"
            );
        }
        assert_eq!(urgency_weight("some-future-tier"), 0, "unknown ranks Idle");
    }

    /// The degraded read: a typed store persists under the board magic; the
    /// raw fallback re-reads it, hides the dismissed, and orders by static
    /// rank — all without a daemon.
    #[test]
    fn fallback_reads_the_persisted_snapshot_read_only() {
        struct TestDir(PathBuf);
        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let mut name = String::from("izumi-board-fallback-test-");
        name.push_str(&std::process::id().to_string());
        let dir = TestDir(std::env::temp_dir().join(name));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        let snapshot = dir.0.join("board.snapshot");

        let mk = |key: &str, u: Urgency| {
            Item::new(
                BoardKind::TendRepos,
                key,
                key,
                SpawnSpec::new("/code", key).unwrap(),
            )
            .urgent(u)
        };
        let store: Store<BoardKind, SpawnSpec> = Store::new();
        store.ingest(
            BoardKind::TendRepos,
            vec![
                mk("calm", Urgency::Low),
                mk("hot", Urgency::Critical),
                mk("dead", Urgency::Critical),
            ],
            1_000,
        );
        assert!(store.dismiss(ItemId::derive(BoardKind::TendRepos, "dead")));
        store.persist_file(&snapshot, crate::state::SNAPSHOT_MAGIC, 2_000);

        let rows = fallback_rows_from(&snapshot, 10, 2_000);
        assert_eq!(rows.len(), 2, "the dismissed row is hidden");
        assert_eq!(rows[0].title, "hot", "static rank: critical first");
        assert_eq!(rows[1].title, "calm");
        assert_eq!(
            rows[0].id,
            ItemId::derive(BoardKind::TendRepos, "hot").0.to_string(),
            "decimal-string ids survive the raw ingress"
        );

        // A missing snapshot is an EMPTY board, never an error.
        assert!(fallback_rows_from(&dir.0.join("nope"), 10, 2_000).is_empty());
    }
}
