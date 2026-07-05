//! The config→engine bridge — translate the typed shikumi [`BoardConfig`]
//! into an [`izumi::EngineConfig`] over a consumer's typed catalog.
//!
//! The generalized port of mado's `engine_config_from`: reads the MERGED
//! source list (prescribed arm-list ⊕ operator overrides, see
//! [`BoardConfig::effective_sources`]), so a params-only yaml override never
//! disarms the rest of the surface. Slugs resolve through the consumer's
//! [`izumi::Catalog`]; unknown slugs are ignored here — exactly mado's
//! `SourceKind::from_slug` gate.

use std::collections::BTreeMap;

use crate::BoardConfig;

/// Translate the typed board config into an [`izumi::EngineConfig`] for a
/// consumer catalog `K`. `prescribed` is the consumer's prescribed arm-list
/// (mado passes its 25-kind workflow surface; izumi-board passes its own) —
/// operator `sources` overrides merge over it by slug, and `sources_replace`
/// turns the operator list into an explicit allow-list.
#[must_use]
pub fn to_engine_config<K: izumi::Catalog>(
    cfg: &BoardConfig,
    prescribed: &[K],
) -> izumi::EngineConfig<K> {
    let slugs: Vec<&str> = prescribed.iter().map(|k| k.slug()).collect();
    let mut ec = izumi::EngineConfig {
        per_source: BTreeMap::new(),
        default_enabled: cfg.default_enabled,
    };
    for s in &cfg.effective_sources(&slugs) {
        if let Some(kind) = K::from_slug(&s.kind) {
            let mut sc = izumi::SourceConfig::for_kind(kind);
            sc.enabled = s.enabled;
            if let Some(iv) = s.interval_secs {
                sc.interval = std::time::Duration::from_secs(iv.max(1));
            }
            if let Some(mx) = s.max_items {
                sc.max_items = mx;
            }
            sc.params = s.params.clone();
            ec.per_source.insert(kind, sc);
        }
    }
    ec
}
