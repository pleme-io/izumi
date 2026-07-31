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

/// Whether a source is working, broken, has never worked at all, or has not
/// been looked at.
///
/// **Re-exported, not redefined.** This type and its blind/degraded/ok rule
/// live in `izumi` next to the record they describe
/// ([`izumi::SourceHealth::verdict`]), because the rule had grown a second
/// copy here and a third at mado's typed border. Two copies of one rule
/// drift; three is a promise to drift. Everything below renders the verdict —
/// nothing decides it.
pub use izumi::HealthVerdict;

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
    /// Whether the source has EVER observed its upstream — now across every
    /// process that watched it, since the health plane is persisted. Kept on
    /// the wire for compatibility; it is a projection of `evidence`, and a
    /// DECISION must read `evidence` instead (a bool cannot tell "never
    /// worked" from "never looked").
    pub ever_ok: bool,
    /// The typed success history this row's verdict derives from.
    /// `#[serde(default)]` → a peer that predates the field sends none, and
    /// none reads as [`izumi::OkEvidence::Unobserved`] (unknown), never as a
    /// success.
    #[serde(default = "unobserved")]
    pub evidence: izumi::OkEvidence,
    /// How long this source has been under observation, in seconds — the
    /// evidence behind a BLIND verdict, so the line can state its grounds
    /// rather than assert its conclusion. 0 when never polled.
    #[serde(default)]
    pub observed_secs: u64,
}

fn unobserved() -> izumi::OkEvidence {
    izumi::OkEvidence::Unobserved
}

impl HealthView {
    /// Build from the store's typed health record (field mapping verbatim
    /// from mado's `board_json` health block, plus the typed evidence the
    /// verdict is derived from).
    #[must_use]
    pub fn from_health<K: izumi::Catalog>(kind: K, h: &izumi::SourceHealth, now_ms: u64) -> Self {
        Self {
            source: kind.slug().to_owned(),
            status: h.status.label().to_owned(),
            last_poll_secs_ago: now_ms.saturating_sub(h.last_poll_ms) / 1000,
            ever_ok: h.ever_ok(),
            evidence: h.ok_evidence(),
            observed_secs: h.observed_for_ms(now_ms) / 1000,
        }
    }

    /// The operator-facing verdict, which is NOT the same question as
    /// `status`.
    ///
    /// `status` answers "is this source erroring *right now*". That is the
    /// question that hid seven dead sources for 118+ consecutive polls: a
    /// lane misconfigured since the day it was wired reads `erroring`,
    /// which is exactly what a healthy lane reads during a transient
    /// upstream blip. Same word, opposite meaning — one is weather, the
    /// other is a build defect.
    ///
    /// [`HealthVerdict::Blind`] separates them by asking the question the
    /// data could always answer and nothing ever rendered: has this source
    /// EVER observed its upstream?
    ///
    /// DELEGATES. The wire form carries a label and a typed evidence; this
    /// recovers the typed status and hands both to [`HealthVerdict::of`],
    /// the one place the rule is written. An unparseable label falls to
    /// `Error` — the safe direction, since nothing may invent an `Ok`.
    #[must_use]
    pub fn verdict(&self) -> HealthVerdict {
        let status = izumi::SourceStatus::from_label(&self.status)
            .unwrap_or(izumi::SourceStatus::Error);
        HealthVerdict::of(status, self.evidence)
    }
}

/// Render a duration in the coarsest unit that still reads honestly — the
/// grounds of a BLIND verdict belong on the line, in words a human parses at
/// a glance.
fn humanize_secs(secs: u64) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let (n, unit) = match secs {
        s if s >= 86_400 => (s / 86_400, "d"),
        s if s >= 3_600 => (s / 3_600, "h"),
        s if s >= 60 => (s / 60, "m"),
        s => (s, "s"),
    };
    let _ = write!(out, "{n}{unit}");
    out
}

impl core::fmt::Display for HealthView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let verdict = self.verdict();
        write!(
            f,
            "{}  {:<26}  {:<12}  polled {}s ago",
            verdict.label(),
            self.source,
            self.status,
            self.last_poll_secs_ago
        )?;
        match verdict {
            // The whole point of the verb. Say it in words, on the line,
            // not as a flag the reader has to decode — and say how long we
            // watched, so the claim carries its own evidence.
            HealthVerdict::Blind => write!(
                f,
                "  — never once succeeded in {}",
                humanize_secs(self.observed_secs)
            ),
            // The state that used to masquerade as Blind. Naming it stops a
            // gate from firing on an instrument that never ran.
            HealthVerdict::Unknown => f.write_str("  — never polled; no verdict available"),
            HealthVerdict::Ok | HealthVerdict::Degraded => Ok(()),
        }
    }
}

#[cfg(test)]
mod health_verdict_tests {
    use super::*;

    fn view(status: &str, ever_ok: bool) -> HealthView {
        HealthView {
            source: "grafana-alerts".to_owned(),
            status: status.to_owned(),
            last_poll_secs_ago: 12,
            ever_ok,
            // A polled source, either with the latch set or without it —
            // the two cells these tests are about. `Unobserved` (never
            // polled) is a third state, covered separately.
            evidence: if ever_ok {
                izumi::OkEvidence::Succeeded { at_ms: 1 }
            } else {
                izumi::OkEvidence::NeverSucceeded { since_ms: 1 }
            },
            observed_secs: 3_600,
        }
    }

    /// THE REGRESSION. These two are byte-identical in `status` — the only
    /// field anything rendered before this verb existed — and mean opposite
    /// things. The first is a broken declaration nobody will ever fix by
    /// waiting; the second is weather. Collapsing them is what let seven
    /// sources sit dead for 118+ consecutive polls.
    #[test]
    fn erroring_forever_is_not_the_same_as_erroring_now() {
        let never = view("erroring", false);
        let was_fine = view("erroring", true);

        assert_eq!(never.status, was_fine.status, "the premise: status cannot tell them apart");

        assert_eq!(never.verdict(), HealthVerdict::Blind);
        assert_eq!(was_fine.verdict(), HealthVerdict::Degraded);
        assert!(never.verdict().needs_intervention());
        assert!(!was_fine.verdict().needs_intervention());
    }

    /// ALL-VARIANTS. Every (status × ever_ok) cell is asserted, so a new
    /// status label cannot land without a decision about which verdict it
    /// carries.
    #[test]
    fn verdict_matrix_is_exhaustive() {
        for status in ["ok", "erroring", "needs auth", "needs config"] {
            for ever_ok in [true, false] {
                let v = view(status, ever_ok).verdict();
                let expected = match (status, ever_ok) {
                    ("ok", _) => HealthVerdict::Ok,
                    (_, true) => HealthVerdict::Degraded,
                    (_, false) => HealthVerdict::Blind,
                };
                assert_eq!(v, expected, "status={status} ever_ok={ever_ok}");
            }
        }
    }

    /// The words must reach the line. A verdict the renderer knows and the
    /// operator cannot read is the same failure one layer up.
    #[test]
    fn blind_says_so_in_words() {
        let rendered = view("erroring", false).to_string();
        assert!(rendered.contains("BLIND"), "{rendered}");
        assert!(rendered.contains("never once succeeded"), "{rendered}");

        let degraded = view("erroring", true).to_string();
        assert!(degraded.contains("DEGRADED"), "{degraded}");
        assert!(
            !degraded.contains("never once succeeded"),
            "a source that worked before must not be labelled never-succeeded: {degraded}"
        );
    }

    /// Labels are fixed-width so a column scans preattentively — an agent
    /// reads the field, but a human catches the shape.
    #[test]
    fn labels_are_column_aligned() {
        let widths: Vec<usize> = [HealthVerdict::Ok, HealthVerdict::Degraded, HealthVerdict::Blind]
            .iter()
            .map(|v| v.label().len())
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "labels must share one width, got {widths:?}"
        );
    }

    /// ONE DEFINITION. This border must AGREE with the store record it was
    /// built from for every cell — not because two implementations were
    /// written to match, but because there is only one implementation. This
    /// test is what stops a future edit here from quietly forking the rule
    /// again, which is exactly how it came to exist in three places.
    #[test]
    fn the_view_verdict_is_the_store_verdict_for_every_cell() {
        let now = 100_000;
        let statuses = [
            izumi::SourceStatus::Ok,
            izumi::SourceStatus::Unconfigured,
            izumi::SourceStatus::AuthMissing,
            izumi::SourceStatus::TimedOut,
            izumi::SourceStatus::Error,
        ];
        // (never polled, polled-never-succeeded, succeeded-then-broke).
        let histories = [(0, 0), (10_000, 0), (10_000, 20_000)];
        for status in statuses {
            for (first_poll_ms, last_ok_ms) in histories {
                let h = izumi::SourceHealth {
                    status,
                    last_poll_ms: first_poll_ms,
                    last_ok_ms,
                    first_poll_ms,
                };
                let v = HealthView::from_health(crate::catalog::BoardKind::TendRepos, &h, now);
                assert_eq!(
                    v.verdict(),
                    h.verdict(),
                    "the wire border and the typed record must never disagree: {h:?}"
                );
                assert_eq!(v.ever_ok, h.ever_ok());
            }
        }
    }

    /// The state the old boolean could not name. A source whose instrument
    /// has never run is UNKNOWN, and unknown does not gate — that is what
    /// keeps `izumi-board health`'s exit 2 meaning "go fix a declaration".
    #[test]
    fn never_polled_is_unknown_and_does_not_gate() {
        let unrun = HealthView {
            source: "grafana-alerts".to_owned(),
            status: "needs config".to_owned(),
            last_poll_secs_ago: 0,
            ever_ok: false,
            evidence: izumi::OkEvidence::Unobserved,
            observed_secs: 0,
        };
        assert_eq!(unrun.verdict(), HealthVerdict::Unknown);
        assert!(
            !unrun.verdict().needs_intervention(),
            "an instrument that never ran is not a broken declaration"
        );
        assert_ne!(unrun.verdict(), HealthVerdict::Ok, "nor is it healthy");
        let rendered = unrun.to_string();
        assert!(rendered.contains("never polled"), "{rendered}");
        assert!(
            !rendered.contains("never once succeeded"),
            "a source never polled must not be accused of never succeeding: {rendered}"
        );
    }

    /// A BLIND line carries its grounds: how long we watched. A verdict that
    /// states its conclusion without its evidence is the shape of claim this
    /// whole change exists to stop.
    #[test]
    fn blind_states_how_long_it_watched() {
        let mut v = view("erroring", false);
        v.observed_secs = 4 * 86_400;
        let rendered = v.to_string();
        assert!(rendered.contains("never once succeeded in 4d"), "{rendered}");
    }

    /// A peer that predates the `evidence` field sends none — and none must
    /// decode to unknown, never to a success. Absence is not evidence.
    #[test]
    fn a_wire_row_without_evidence_decodes_to_unknown() {
        let row: HealthView = serde_json::from_str(
            r#"{"source":"grafana-alerts","status":"erroring","last_poll_secs_ago":9,"ever_ok":true}"#,
        )
        .expect("a pre-evidence row still decodes");
        assert_eq!(row.evidence, izumi::OkEvidence::Unobserved);
        assert_eq!(row.verdict(), HealthVerdict::Unknown);
        assert_ne!(row.verdict(), HealthVerdict::Ok, "absence never reads as ok");
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
                evidence: izumi::OkEvidence::Succeeded { at_ms: 1_000 },
                observed_secs: 60,
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
