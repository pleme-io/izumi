//! The provider registry — wire every izumi-sources provider to its
//! [`BoardKind`], one line per catalog row.
//!
//! The registry is the engine's ONLY source list; the wiring test below runs
//! [`izumi_sources::assert_registry_wiring`] against [`BoardKind`]'s
//! [`Catalog::ALL`](izumi::Catalog::ALL), so
//! adding a catalog variant without registering its provider (or registering
//! one twice) fails the build's test gate — the catalog can never claim a
//! source the engine can't actually run (CLOSED-LOOP MASS-SYNTHESIS rule 1,
//! the same mechanical proof mado's registry ships).

use std::sync::Arc;

use izumi::{Source, SpawnSpec};

use crate::catalog::BoardKind;

/// Every provider this board runs, each constructed with its catalog kind.
/// Which ones actually POLL is the engine config's concern (enablement,
/// cadence, params) — the registry is the complete capability surface.
#[must_use]
pub fn registry() -> Vec<Arc<dyn Source<BoardKind, SpawnSpec>>> {
    vec![
        Arc::new(izumi_sources::GitBranchPr::new(BoardKind::GitBranchPr)),
        Arc::new(izumi_sources::TendRepos::new(BoardKind::TendRepos)),
        Arc::new(izumi_sources::CargoWarnings::new(BoardKind::CargoWarnings)),
        Arc::new(izumi_sources::TodoBacklog::new(BoardKind::TodoBacklog)),
        Arc::new(izumi_sources::GithubReviewRequested::new(BoardKind::GithubReviewRequested)),
        Arc::new(izumi_sources::GithubAssignedIssues::new(BoardKind::GithubAssignedIssues)),
        Arc::new(izumi_sources::GithubActionsFailing::new(BoardKind::GithubActionsFailing)),
        Arc::new(izumi_sources::JiraSprint::new(BoardKind::JiraSprint)),
        Arc::new(izumi_sources::JiraAssigned::new(BoardKind::JiraAssigned)),
        Arc::new(izumi_sources::ConfluenceMentions::new(BoardKind::ConfluenceMentions)),
        Arc::new(izumi_sources::FluxFailing::new(BoardKind::FluxFailing)),
        Arc::new(izumi_sources::K8sUnhealthy::new(BoardKind::K8sUnhealthy)),
        Arc::new(izumi_sources::BreatheConflict::new(BoardKind::BreatheConflict)),
        Arc::new(izumi_sources::EngenhoNodes::new(BoardKind::EngenhoNodes)),
        Arc::new(izumi_sources::GrafanaAlerts::new(BoardKind::GrafanaAlerts)),
        Arc::new(izumi_sources::GrafanaIncidents::new(BoardKind::GrafanaIncidents)),
        Arc::new(izumi_sources::GrafanaOncall::new(BoardKind::GrafanaOncall)),
        Arc::new(izumi_sources::DatadogMonitors::new(BoardKind::DatadogMonitors)),
        Arc::new(izumi_sources::OpsgenieAlerts::new(BoardKind::OpsgenieAlerts)),
        Arc::new(izumi_sources::KurageAgents::new(BoardKind::KurageAgents)),
        Arc::new(izumi_sources::AwsHealth::new(BoardKind::AwsHealth)),
        Arc::new(izumi_sources::CloudflareDeployments::new(BoardKind::CloudflareDeployments)),
        Arc::new(izumi_sources::GoogleTasks::new(BoardKind::GoogleTasks)),
        Arc::new(izumi_sources::GoogleCalendar::new(BoardKind::GoogleCalendar)),
        Arc::new(izumi_sources::SecretAge::new(BoardKind::SecretAge)),
    ]
}

#[cfg(test)]
mod tests {
    use izumi::Catalog as _;

    use super::*;

    /// The registry invariant: one provider per catalog kind, no kind
    /// registered twice, no kind left providerless — the mechanical proof the
    /// substrate's `assert_registry_wiring` helper exists for.
    #[test]
    fn registry_covers_the_whole_catalog_exactly_once() {
        let sources = registry();
        assert_eq!(sources.len(), BoardKind::ALL.len(), "one provider per catalog row");
        izumi_sources::assert_registry_wiring(&sources, BoardKind::ALL);
    }

    /// Every provider reports the kind it was constructed with — the generic
    /// `kind: K` field's one invariant, checked through this consumer's wiring.
    #[test]
    fn every_provider_reports_its_wired_kind() {
        let mut kinds: Vec<BoardKind> = registry().iter().map(|s| s.kind()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, BoardKind::ALL, "registry order-independent kind coverage");
    }
}
