//! Board-config loading — shikumi tiered discovery over
//! `~/.config/izumi/izumi.yaml`, bridged into the typed engine config.
//!
//! The tier contract (shikumi's fleet convention): `IZUMI_TIER=bare` strips
//! the whole board, `discovered`/`default` resolve the trait tiers, and a
//! missing config file is simply the prescribed default — a fresh
//! workstation gets the full 25-source surface at the fleet cadence with
//! zero YAML. When the YAML exists it loads through a [`shikumi::ConfigStore`]
//! (env overrides under [`ENV_PREFIX`] layered over the file, missing fields
//! taking the prescribed value via the container `serde(default)`), and its
//! `sources:` entries MERGE over the prescribed arm-list per
//! [`izumi_config::BoardConfig::effective_sources`] — a params-only override
//! never disarms the other 24 sources.

use izumi::Catalog as _;
use izumi_config::BoardConfig;
use shikumi::TieredConfig as _;

use crate::catalog::BoardKind;

/// Tier selector env var (shikumi's `<APP>_TIER` convention).
pub const TIER_ENV: &str = "IZUMI_TIER";

/// Explicit config-path override env var (checked before the standard
/// `~/.config/izumi/izumi.yaml` discovery chain).
pub const CONFIG_ENV: &str = "IZUMI_CONFIG";

/// Env prefix for per-knob config overrides (`IZUMI_BOARD_TTL_SECS=600`).
/// Deliberately NOT the bare `IZUMI_` prefix: the process-level vars
/// ([`TIER_ENV`], [`CONFIG_ENV`], [`crate::state::STATE_DIR_ENV`]) would
/// otherwise be swept into the config merge as unknown fields and fail the
/// `deny_unknown_fields` parse.
pub const ENV_PREFIX: &str = "IZUMI_BOARD_";

/// Load the operator's board config: resolve the tier from [`TIER_ENV`]
/// (an explicit non-default tier wins outright — the operator asked for a
/// specific baseline), else discover + load the YAML, else the prescribed
/// default. A malformed file WARNS and falls back to prescribed rather than
/// refusing to serve — the board degrades, never wedges.
#[must_use]
pub fn load_board_config() -> BoardConfig {
    let tier = shikumi::ConfigTier::from_env(TIER_ENV);
    if tier != shikumi::ConfigTier::Default {
        tracing::info!(tier = tier.name(), "explicit config tier requested");
        return BoardConfig::resolve_tier(tier);
    }
    let discovery = shikumi::ConfigDiscovery::new("izumi").env_override(CONFIG_ENV);
    let Ok(path) = discovery.discover() else {
        // Missing file = the prescribed default (the documented contract).
        return BoardConfig::prescribed_default();
    };
    match shikumi::ConfigStore::<BoardConfig>::load(&path, ENV_PREFIX) {
        Ok(store) => {
            let cfg: BoardConfig = store.get().as_ref().clone();
            tracing::info!(
                path = %path.display(),
                enabled = cfg.enabled,
                overrides = cfg.sources.len(),
                "board config loaded"
            );
            cfg
        }
        Err(err) => {
            tracing::warn!(
                err = %err,
                path = %path.display(),
                "board config failed to load — falling back to the prescribed default"
            );
            BoardConfig::prescribed_default()
        }
    }
}

/// Translate the typed board config into the engine config over THIS
/// consumer's catalog — [`BoardKind::ALL`] is the prescribed arm-list the
/// operator's `sources:` overrides merge over (the load-bearing
/// params-only-override-never-disarms semantics live in
/// [`izumi_config::to_engine_config`]).
#[must_use]
pub fn engine_config(cfg: &BoardConfig) -> izumi::EngineConfig<BoardKind> {
    izumi_config::to_engine_config(cfg, BoardKind::ALL)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use izumi_config::SourceEntry;

    use super::*;

    /// The prescribed default arms the WHOLE catalog at each kind's fleet
    /// cadence — a fresh workstation needs zero YAML.
    #[test]
    fn prescribed_config_arms_the_whole_catalog_at_its_cadence() {
        let cfg = BoardConfig::prescribed_default();
        let ec = engine_config(&cfg);
        assert_eq!(
            ec.per_source.len(),
            BoardKind::ALL.len(),
            "one engine entry per catalog kind"
        );
        for &kind in BoardKind::ALL {
            let sc = ec.config_for(kind);
            assert!(sc.enabled, "{} armed by default", kind.slug());
            assert_eq!(
                sc.interval,
                Duration::from_secs(kind.default_interval_secs()),
                "{} at its catalog cadence",
                kind.slug()
            );
        }
    }

    /// The exact failure the merge exists for: a params-only YAML override
    /// for one source must NOT disarm the other 24; an explicit disable
    /// disarms exactly its kind; an unknown slug is ignored at the catalog
    /// gate; `sources_replace` restores allow-list semantics.
    #[test]
    fn operator_overrides_merge_into_the_engine_config() {
        let mut over = SourceEntry::enable(BoardKind::JiraAssigned.slug());
        over.interval_secs = Some(600);
        over.max_items = Some(2);
        over.params
            .insert(String::from("site"), String::from("acme.atlassian.net"));
        let mut off = SourceEntry::enable(BoardKind::TodoBacklog.slug());
        off.enabled = false;
        let cfg = BoardConfig {
            sources: vec![over, off, SourceEntry::enable("no-such-source")],
            ..BoardConfig::prescribed_default()
        };
        let ec = engine_config(&cfg);
        assert_eq!(
            ec.per_source.len(),
            BoardKind::ALL.len(),
            "the unknown slug is ignored at the catalog gate"
        );
        let jira = ec.config_for(BoardKind::JiraAssigned);
        assert!(jira.enabled);
        assert_eq!(jira.interval, Duration::from_secs(600), "cadence override lands");
        assert_eq!(jira.max_items, 2, "cap override lands");
        assert_eq!(jira.param("site"), Some("acme.atlassian.net"), "params land");
        assert!(!ec.config_for(BoardKind::TodoBacklog).enabled, "explicit disable wins");
        assert_eq!(
            ec.per_source.values().filter(|sc| sc.enabled).count(),
            BoardKind::ALL.len() - 1,
            "only the disabled kind is disarmed"
        );

        // The escape hatch: replace mode + default_enabled=false is the
        // explicit allow-list — exactly the listed kinds run, unlisted kinds
        // fall back to default_enabled through `config_for`.
        let cfg = BoardConfig {
            sources: vec![SourceEntry::enable(BoardKind::GrafanaAlerts.slug())],
            sources_replace: true,
            default_enabled: false,
            ..BoardConfig::prescribed_default()
        };
        let ec = engine_config(&cfg);
        assert_eq!(ec.per_source.len(), 1, "replace mode overrides exactly the listed kinds");
        assert!(ec.config_for(BoardKind::GrafanaAlerts).enabled);
        assert!(
            !ec.config_for(BoardKind::TendRepos).enabled,
            "unlisted kinds follow default_enabled"
        );
    }

    /// The bare tier is the stripped floor: master switch off. (Per-source
    /// entries still merge — the master `enabled` gate is the daemon's
    /// engine-construction gate, not a per-source strip.)
    #[test]
    fn bare_tier_strips_the_master_switch() {
        let bare = BoardConfig::resolve_tier(shikumi::ConfigTier::Bare);
        assert!(!bare.enabled, "bare = no engine, no watchers, no rows");
        assert!(!bare.persist, "bare = no disk writes");
    }
}
