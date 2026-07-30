//! The latency-budget gate.
//!
//! A source that shells out to a broad fleet aggregator (`tend status`, a
//! cluster-wide `kubectl … -o json`) must declare [`LatencyBudget::Slow`] via
//! `Cmd::slow()`. Without it the command is capped at the 20 s `Quick` default,
//! gets killed against a real cluster, and the source reports `error()` forever
//! — the failure mode `Cmd::slow()`'s own doc comment exists to prevent.
//!
//! Nothing observed this before: `MockEnvironment` keys commands by
//! [`Cmd::key`] and discards the budget, so all 131 existing tests passed with
//! four k8s sources silently on the wrong cap. This file closes that gap by
//! spying on the `Cmd` itself.
//!
//! Fail-once evidence (2026-07-30): deleting `.slow()` from any row below turns
//! this red with `budget for <slug> is Quick, expected Slow`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use izumi::{
    Catalog, Cmd, Environment, HttpReq, LatencyBudget, PollOutcome, Source, SourceConfig, SpawnSpec,
};
use izumi_sources::{BreatheConflict, EngenhoNodes, FluxFailing, K8sUnhealthy, TendRepos};

izumi::catalog! {
    /// Kinds for the budget probe — slugs only need to be distinct.
    pub enum BudgetKind {
        K8sUnhealthy { slug: "k8s-unhealthy", emoji: "\u{2638}", label: "k8s unhealthy pods", urgency: Critical, needs_auth: false, interval_secs: 60 },
        FluxFailing { slug: "flux-failing", emoji: "\u{1F501}", label: "Flux failing", urgency: High, needs_auth: false, interval_secs: 60 },
        BreatheConflict { slug: "breathe-conflict", emoji: "\u{1F4A8}", label: "breathe Conflict bands", urgency: High, needs_auth: false, interval_secs: 60 },
        EngenhoNodes { slug: "engenho-nodes", emoji: "\u{1F5A5}", label: "engenho nodes", urgency: Normal, needs_auth: false, interval_secs: 60 },
        TendRepos { slug: "tend-repos", emoji: "\u{1F9F9}", label: "tend dirty repos", urgency: Low, needs_auth: false, interval_secs: 30 },
    }
}

/// An `Environment` that records the budget of every `Cmd` a source builds and
/// then fails the call. Returning `None` is deliberate: the gate only cares
/// that the command was *constructed* with the right cap, so no fixture is
/// needed and the assertion cannot be satisfied by fixture-shaped luck.
#[derive(Default)]
struct BudgetSpy {
    seen: Mutex<Vec<(String, LatencyBudget)>>,
}

impl Environment for BudgetSpy {
    fn run(&self, cmd: &Cmd) -> Option<String> {
        self.seen.lock().unwrap().push((cmd.key(), cmd.budget()));
        None
    }
    fn http_get(&self, _req: &HttpReq) -> Option<String> {
        None
    }
    fn read_file(&self, _path: &Path) -> Option<String> {
        None
    }
    fn path_exists(&self, _path: &Path) -> bool {
        false
    }
    fn secret(&self, _key: &str) -> Option<String> {
        None
    }
    fn now_unix(&self) -> u64 {
        0
    }
    fn code_root(&self) -> PathBuf {
        PathBuf::from("/nonexistent")
    }
    fn home(&self) -> PathBuf {
        PathBuf::from("/nonexistent")
    }
}

/// Poll `src` against a spy and return the budget of the first `Cmd` it built.
fn first_budget(src: &dyn Source<BudgetKind, SpawnSpec>, kind: BudgetKind) -> LatencyBudget {
    let spy = BudgetSpy::default();
    let _: PollOutcome<BudgetKind, SpawnSpec> = src.poll(&spy, &SourceConfig::for_kind(kind));
    let seen = spy.seen.lock().unwrap();
    assert!(
        !seen.is_empty(),
        "source {} issued no Cmd, so its budget cannot be gated",
        kind.slug()
    );
    seen[0].1
}

/// The control arm. If this ever reads `Slow`, the assertions below are
/// vacuous — every command would pass regardless of `.slow()`.
#[test]
fn default_budget_is_quick() {
    assert_eq!(
        Cmd::new("kubectl").arg("get").arg("pods").budget(),
        LatencyBudget::Quick,
        "an undeclared Cmd must default to Quick, else this gate proves nothing"
    );
}

#[test]
fn fleet_aggregators_declare_slow() {
    let rows: Vec<(BudgetKind, Box<dyn Source<BudgetKind, SpawnSpec>>)> = vec![
        (
            BudgetKind::K8sUnhealthy,
            Box::new(K8sUnhealthy::new(BudgetKind::K8sUnhealthy)),
        ),
        (
            BudgetKind::FluxFailing,
            Box::new(FluxFailing::new(BudgetKind::FluxFailing)),
        ),
        (
            BudgetKind::BreatheConflict,
            Box::new(BreatheConflict::new(BudgetKind::BreatheConflict)),
        ),
        (
            BudgetKind::EngenhoNodes,
            Box::new(EngenhoNodes::new(BudgetKind::EngenhoNodes)),
        ),
        (
            BudgetKind::TendRepos,
            Box::new(TendRepos::new(BudgetKind::TendRepos)),
        ),
    ];

    for (kind, src) in &rows {
        assert_eq!(
            first_budget(src.as_ref(), *kind),
            LatencyBudget::Slow,
            "budget for {} is Quick, expected Slow — a cluster-wide or \
             fleet-wide command needs Cmd::slow() or it is killed at 20 s \
             and the source errors forever",
            kind.slug()
        );
    }
}
