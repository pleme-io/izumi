//! izumi (泉, *spring/fountain*) — the fresh-source board substrate:
//! continuously-refreshing, ranked, cache-fresh, actionable data sources.
//!
//! izumi is the generalized port of mado's Ctrl-S suggestion plane. Where the
//! mado original was hard-wired to one 29-variant `SourceKind` catalog and one
//! `SpawnSpec` action payload, izumi is generic over BOTH axes:
//!
//! * **`K: Catalog`** — the consumer's source-kind enum, authored through the
//!   [`catalog!`] macro (slug + emoji + label + urgency + auth + cadence per
//!   variant, catalog reflection + compile-time slug uniqueness for free); and
//! * **`A: Payload`** — the consumer's per-item action payload (mado uses
//!   [`SpawnSpec`]; another consumer may carry any serde-able typed action).
//!
//! The algebra is unchanged from the proven mado implementation:
//!
//! * [`Item`] — a latent, always-actionable row with a content-addressed
//!   [`ItemId`], an [`Urgency`]-dominant [`Rank`], and an optional
//!   cross-source [`CorrKey`];
//! * [`Store`] — the ephemeral ranked living board: per-source slice
//!   ownership, tombstone recurrence, aging escalation, lifecycle
//!   ([`ItemState`]), source health, reactive change broadcast, and a
//!   BLAKE3-framed warm-restart snapshot;
//! * [`Source`] / [`PollOutcome`] — the honest provider border ("observed
//!   empty" is typed differently from "could not observe");
//! * [`Engine`] — one paced watcher task per enabled source, with an optional
//!   freshness nudge;
//! * [`Environment`] — the one mockable side-effect boundary (typed argv
//!   subprocess, typed-curl HTTP with per-host [`pace`] + 429 cooldown, files,
//!   secrets, clock);
//! * [`persist`] / [`raw`] / [`writer`] / [`maintain`] — the shared snapshot
//!   framing, the catalog-erased cross-process reader, the single-writer
//!   election, and the shared decay+gc tick.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod engine;
pub mod env;
pub mod item;
pub mod maintain;
pub mod pace;
pub mod payload;
pub mod persist;
pub mod raw;
pub mod refresh;
pub mod source;
pub mod spawn;
pub mod store;
pub mod writer;

pub use catalog::Catalog;
pub use engine::Engine;
pub use env::{Cmd, Environment, HttpReq, MockEnvironment, RealEnvironment};
pub use item::{fnv1a, CorrKey, Item, ItemId, PickerLabel, Rank, SourceStatus, Urgency};
pub use payload::Payload;
pub use source::{apply_poll, refresh_once, EngineConfig, PollOutcome, Source, SourceConfig};
pub use spawn::SpawnSpec;
pub use store::{
    effective_rank_key, shade_ramp, ItemState, SourceHealth, Store, StoreSnapshot, StoredItem,
};

/// The in-crate test catalog — slugs / urgencies / cadences MATCH the mado
/// `SourceKind` variants the ported tests exercise, so fnv1a-derived id
/// expectations and rank expectations are unchanged from the mado suite.
#[cfg(test)]
pub(crate) mod testkit {
    crate::catalog! {
        /// The test catalog (a faithful subset of mado's `SourceKind`).
        pub enum TestKind {
            /// Local git branches correlated to their open PR titles.
            GitBranchPr { slug: "git-branch-pr", emoji: "\u{1F33F}", label: "git branch ↔ PR", urgency: Low, needs_auth: false, interval_secs: 30 },
            /// `tend` workspace repos that are dirty / unsynced / missing.
            TendRepos { slug: "tend-repos", emoji: "\u{1F9F9}", label: "tend dirty repos", urgency: Low, needs_auth: false, interval_secs: 30 },
            /// Recently-visited directories.
            RecentDirs { slug: "recent-dirs", emoji: "\u{1F4C1}", label: "recent directories", urgency: Idle, needs_auth: false, interval_secs: 30 },
            /// PRs awaiting your review.
            GithubReviewRequested { slug: "github-review-requested", emoji: "\u{1F50D}", label: "GitHub review-requested", urgency: High, needs_auth: true, interval_secs: 180 },
            /// Jira issues in your active sprint.
            JiraSprint { slug: "jira-sprint", emoji: "\u{1F3AB}", label: "Jira sprint", urgency: Normal, needs_auth: true, interval_secs: 300 },
            /// Jira issues assigned to you.
            JiraAssigned { slug: "jira-assigned", emoji: "\u{1F4CB}", label: "Jira assigned", urgency: Normal, needs_auth: true, interval_secs: 300 },
            /// grafana alerts firing.
            GrafanaAlerts { slug: "grafana-alerts", emoji: "\u{1F525}", label: "grafana alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// grafana incidents open.
            GrafanaIncidents { slug: "grafana-incidents", emoji: "\u{1F6A9}", label: "grafana incidents", urgency: Critical, needs_auth: true, interval_secs: 90 },
        }
    }
}
