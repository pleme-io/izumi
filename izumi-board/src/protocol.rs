//! The typed control-socket protocol — newline-delimited JSON, one
//! externally-tagged [`Request`] per line in, one [`Response`] per line out.
//!
//! The row shape ([`RowView`]) mirrors mado's agent-facing `board_json` row
//! field-for-field (id as a DECIMAL STRING — a `u64` does not survive JSON
//! number precision — source slug, title, detail, urgency, lifecycle state,
//! recurrence count, waiting seconds, and the spawn target's cwd / session
//! name / kickoff command), so an agent that reads a mado board reads an
//! izumi board with the same code.
//!
//! [`RowView`] is buildable from BOTH ingresses: the daemon's live typed
//! store ([`RowView::from_stored`]) and the CLI's degraded catalog-erased
//! snapshot read ([`RowView::from_raw`]) — one shape, two constructors,
//! pinned equal by test.

use serde::{Deserialize, Serialize};

/// One control request — a single JSON line on the socket. Externally tagged
/// kebab-case wire form: `{"list":{"max":20}}`, `{"dismiss":{"id":"42"}}`,
/// `"health"`, `"nudge"`, ….
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Request {
    /// The ranked offerable rows, typed ([`Response::Rows`]).
    List { max: usize },
    /// The full board JSON — rows + per-source health ([`Response::Board`]),
    /// shaped exactly like mado's `board_json`.
    Json { max: usize },
    /// Dismiss a row by its decimal-string id — never offered again.
    Dismiss { id: String },
    /// Snooze a row for `secs` seconds — hidden until the deadline.
    Snooze { id: String, secs: u64 },
    /// Mark a row accepted (in progress) under `session` — soft-acked to the
    /// Idle tier, badged instead of removed.
    Accept { id: String, session: String },
    /// Per-source poll health ([`Response::Health`]).
    Health,
    /// Fire the freshness nudge — every watcher whose data is older than its
    /// pacing gap re-polls right now (paced; a nudge storm cannot hammer an
    /// API).
    Nudge,
}

/// One control response — a single JSON line back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Response {
    /// The ranked offerable rows (living-board order).
    Rows(Vec<RowView>),
    /// The mado-shaped board JSON: `{"suggestions": [...], "health": [...]}`.
    Board(serde_json::Value),
    /// A lifecycle/nudge acknowledgement. `ok: false` means the id was
    /// unknown (it may have decayed) or the request was malformed.
    Done { ok: bool },
    /// Per-source poll health.
    Health(Vec<HealthView>),
}

/// The kebab wire slug of an [`izumi::Urgency`] — pinned to the serde wire
/// form by test, so the row view can never drift from the snapshot codec.
#[must_use]
pub fn urgency_slug(u: izumi::Urgency) -> &'static str {
    match u {
        izumi::Urgency::Idle => "idle",
        izumi::Urgency::Low => "low",
        izumi::Urgency::Normal => "normal",
        izumi::Urgency::High => "high",
        izumi::Urgency::Critical => "critical",
    }
}

/// The kebab `kind` tag of an [`izumi::ItemState`] — same pinning as
/// [`urgency_slug`] (the raw snapshot reader distills state to this tag, so
/// the two [`RowView`] ingresses agree by construction).
#[must_use]
pub fn state_kind(state: &izumi::ItemState) -> &'static str {
    match state {
        izumi::ItemState::Offered => "offered",
        izumi::ItemState::Accepted { .. } => "accepted",
        izumi::ItemState::Snoozed { .. } => "snoozed",
        izumi::ItemState::Dismissed => "dismissed",
    }
}

/// One board row as the agent/CLI surface renders it — the typed twin of a
/// mado `board_json` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowView {
    /// The item id as a DECIMAL `u64` STRING (JSON-number-precision safe).
    pub id: String,
    /// The source's kebab slug (unknown slugs from a newer producer survive
    /// the raw ingress verbatim).
    pub source: String,
    /// The task itself (the row's primary text).
    pub title: String,
    /// Optional secondary context shown dimmer.
    pub detail: Option<String>,
    /// The urgency's kebab slug (`idle` … `critical`).
    pub urgency: String,
    /// The lifecycle state's kebab kind tag (`offered` / `accepted` /
    /// `snoozed` / `dismissed`).
    pub state: String,
    /// How many times this issue has (re)appeared (anomaly-recurrence ×N).
    pub times_seen: u32,
    /// Seconds since this id first appeared — the aging-escalation clock.
    pub waiting_secs: u64,
    /// The spawn target's working directory.
    pub cwd: String,
    /// The spawn target's session name.
    pub session_name: String,
    /// The spawn target's optional kickoff command.
    pub initial_command: Option<String>,
}

impl RowView {
    /// Build from the daemon's live typed store row — the primary ingress
    /// (field mapping verbatim from mado's `board_json`).
    #[must_use]
    pub fn from_stored<K: izumi::Catalog>(
        st: &izumi::StoredItem<K, izumi::SpawnSpec>,
        now_ms: u64,
    ) -> Self {
        Self {
            id: st.item.id.0.to_string(),
            source: st.item.source.slug().to_owned(),
            title: st.item.title.clone(),
            detail: st.item.detail.clone(),
            urgency: urgency_slug(st.item.urgency).to_owned(),
            state: state_kind(&st.state).to_owned(),
            times_seen: st.times_seen,
            waiting_secs: now_ms.saturating_sub(st.first_seen_ms) / 1000,
            cwd: st.item.spawn.cwd().to_string_lossy().into_owned(),
            session_name: st.item.spawn.name().to_owned(),
            initial_command: st.item.spawn.initial_command().map(str::to_owned),
        }
    }

    /// Build from a catalog-erased raw snapshot row — the CLI's degraded
    /// read-only ingress (no live daemon). The opaque payload is walked for
    /// the [`izumi::SpawnSpec`] wire fields (`cwd` / `name` /
    /// `initial_command`); a foreign payload shape simply yields empty spawn
    /// fields rather than dropping the row.
    #[must_use]
    pub fn from_raw(raw: &izumi::raw::RawStoredItem, now_ms: u64) -> Self {
        Self {
            id: raw.id.clone(),
            source: raw.source.clone(),
            title: raw.title.clone(),
            detail: raw.detail.clone(),
            urgency: raw.urgency.clone(),
            state: raw.state_kind.clone(),
            times_seen: raw.times_seen,
            waiting_secs: now_ms.saturating_sub(raw.first_seen_ms) / 1000,
            cwd: raw
                .payload
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            session_name: raw
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            initial_command: raw
                .payload
                .get("initial_command")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        }
    }
}

/// The typed board line — `Display` is the ONE render surface `list` prints
/// through (TYPED EMISSION: a render impl, not a `String` factory):
/// `<id>  [<urgency>] <source>: <title>` plus, when present, the trimmed
/// detail, an `×N` recurrence stamp, a non-`offered` lifecycle badge, and a
/// `waited <m>m` aging stamp.
impl core::fmt::Display for RowView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}  [{}] {}: {}",
            self.id,
            self.urgency,
            self.source,
            self.title.trim()
        )?;
        if let Some(d) = &self.detail {
            let d = d.trim();
            if !d.is_empty() {
                write!(f, "  {d}")?;
            }
        }
        if self.times_seen > 1 {
            write!(f, "  \u{d7}{}", self.times_seen)?;
        }
        if self.state != "offered" {
            write!(f, "  ({})", self.state)?;
        }
        if self.waiting_secs >= 60 {
            write!(f, "  waited {}m", self.waiting_secs / 60)?;
        }
        Ok(())
    }
}

/// One source's poll health as the agent/CLI surface renders it — the typed
/// twin of a mado `board_json` health row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthView {
    /// The source's kebab slug.
    pub source: String,
    /// The operator-facing status label (`ok` / `needs config` / `needs
    /// auth` / `erroring`).
    pub status: String,
    /// Seconds since the source last completed a poll attempt.
    pub last_poll_secs_ago: u64,
    /// Whether the source has EVER observed its upstream this process
    /// lifetime — distinguishes "calm" from "blind since boot".
    pub ever_ok: bool,
}

impl HealthView {
    /// Build from the store's typed health record (field mapping verbatim
    /// from mado's `board_json` health block).
    #[must_use]
    pub fn from_health<K: izumi::Catalog>(
        kind: K,
        h: &izumi::SourceHealth,
        now_ms: u64,
    ) -> Self {
        Self {
            source: kind.slug().to_owned(),
            status: h.status.label().to_owned(),
            last_poll_secs_ago: now_ms.saturating_sub(h.last_poll_ms) / 1000,
            ever_ok: h.last_ok_ms > 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::catalog::BoardKind;
    use izumi::{Item, ItemState, SpawnSpec, StoredItem, Urgency};

    use super::*;

    fn stored(state: ItemState) -> StoredItem<BoardKind, SpawnSpec> {
        let spawn = SpawnSpec::new("/code/mado", "\u{1F9F9} mado")
            .unwrap()
            .with_command("git status");
        StoredItem {
            item: Item::new(BoardKind::TendRepos, "mado", "mado — dirty", spawn)
                .detail("dirty")
                .urgent(Urgency::Low),
            first_seen_ms: 100_000,
            last_seen_ms: 200_000,
            times_seen: 3,
            state,
        }
    }

    /// Every request variant survives the wire round-trip, and the wire form
    /// is the documented externally-tagged kebab shape.
    #[test]
    fn request_wire_round_trips_every_variant() {
        let reqs = [
            Request::List { max: 12 },
            Request::Json { max: 50 },
            Request::Dismiss { id: String::from("42") },
            Request::Snooze { id: String::from("42"), secs: 900 },
            Request::Accept { id: String::from("42"), session: String::from("🔥 fix") },
            Request::Health,
            Request::Nudge,
        ];
        for req in &reqs {
            let wire = serde_json::to_string(req).unwrap();
            let back: Request = serde_json::from_str(&wire).unwrap();
            assert_eq!(&back, req, "round-trip: {wire}");
        }
        // Pin the exact wire shapes an external client would hand-write.
        assert_eq!(
            serde_json::to_string(&Request::List { max: 12 }).unwrap(),
            r#"{"list":{"max":12}}"#
        );
        assert_eq!(serde_json::to_string(&Request::Nudge).unwrap(), "\"nudge\"");
        let parsed: Request = serde_json::from_str(r#"{"snooze":{"id":"7","secs":60}}"#).unwrap();
        assert_eq!(parsed, Request::Snooze { id: String::from("7"), secs: 60 });
    }

    /// Every response variant survives the wire round-trip.
    #[test]
    fn response_wire_round_trips_every_variant() {
        let resps = [
            Response::Rows(vec![RowView::from_stored(&stored(ItemState::Offered), 200_000)]),
            Response::Board(serde_json::json!({"suggestions": [], "health": []})),
            Response::Done { ok: true },
            Response::Health(vec![HealthView {
                source: String::from("tend-repos"),
                status: String::from("ok"),
                last_poll_secs_ago: 3,
                ever_ok: true,
            }]),
        ];
        for resp in &resps {
            let wire = serde_json::to_string(resp).unwrap();
            let back: Response = serde_json::from_str(&wire).unwrap();
            assert_eq!(&back, resp, "round-trip: {wire}");
        }
    }

    /// The urgency/state slug helpers are pinned to the serde wire forms —
    /// the row view can never drift from the snapshot codec.
    #[test]
    fn slug_helpers_match_the_serde_wire_forms() {
        for u in [
            Urgency::Idle,
            Urgency::Low,
            Urgency::Normal,
            Urgency::High,
            Urgency::Critical,
        ] {
            assert_eq!(
                serde_json::to_value(u).unwrap(),
                serde_json::Value::String(urgency_slug(u).to_owned())
            );
        }
        for st in [
            ItemState::Offered,
            ItemState::Accepted { session: String::from("s") },
            ItemState::Snoozed { until_ms: 9 },
            ItemState::Dismissed,
        ] {
            assert_eq!(
                serde_json::to_value(&st).unwrap()["kind"],
                serde_json::Value::String(state_kind(&st).to_owned())
            );
        }
    }

    /// The two ingresses (live typed store / raw snapshot read) land on the
    /// SAME row view — one shape, mechanically pinned.
    #[test]
    fn from_stored_and_from_raw_agree() {
        let st = stored(ItemState::Accepted { session: String::from("s") });
        let typed = RowView::from_stored(&st, 200_000);
        let raw = izumi::raw::RawStoredItem {
            id: st.item.id.0.to_string(),
            source: String::from("tend-repos"),
            title: String::from("mado — dirty"),
            detail: Some(String::from("dirty")),
            urgency: String::from("low"),
            score: 500,
            state_kind: String::from("accepted"),
            times_seen: 3,
            first_seen_ms: 100_000,
            last_seen_ms: 200_000,
            payload: serde_json::json!({
                "cwd": "/code/mado",
                "name": "\u{1F9F9} mado",
                "initial_command": "git status",
            }),
        };
        assert_eq!(RowView::from_raw(&raw, 200_000), typed);
    }

    /// The board line render: id, urgency, source, trimmed title + detail,
    /// the ×N recurrence stamp, the lifecycle badge, the aging stamp.
    #[test]
    fn row_view_display_renders_the_board_line() {
        let st = stored(ItemState::Accepted { session: String::from("s") });
        let line = RowView::from_stored(&st, 200_000).to_string();
        assert!(line.starts_with(&st.item.id.0.to_string()), "leads with the id: {line}");
        assert!(line.contains("[low] tend-repos: mado — dirty"), "{line}");
        assert!(line.contains("  dirty"), "detail rides dimmer: {line}");
        assert!(line.contains("\u{d7}3"), "recurrence stamp: {line}");
        assert!(line.contains("(accepted)"), "lifecycle badge: {line}");
        assert!(line.contains("waited 1m"), "aging stamp (100s → 1m): {line}");

        // A fresh Offered row renders none of the optional stamps.
        let fresh = RowView::from_stored(&stored(ItemState::Offered), 100_000);
        let fresh = RowView { times_seen: 1, ..fresh };
        let line = fresh.to_string();
        assert!(!line.contains('\u{d7}'), "{line}");
        assert!(!line.contains('('), "{line}");
        assert!(!line.contains("waited"), "{line}");
    }
}
