use serde::{Deserialize, Serialize};

use crate::config::{ArmazemConfig, ConfigError, Mode, PolicyEngine};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DemandRow {
    pub key: String,
    pub interest: usize,
    pub live: bool,
    pub idle_ms: Option<u64>,
    pub dormant_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "op", content = "key", rename_all = "kebab-case")]
pub enum Action {
    Start(String),
    Park(String),
    Evict(String),
}

pub trait Policy: Send + Sync {
    fn decide(&self, demand: &[DemandRow], cfg: &ArmazemConfig) -> Result<Vec<Action>, String>;
}

pub struct NativePolicy;

impl Policy for NativePolicy {
    fn decide(&self, demand: &[DemandRow], cfg: &ArmazemConfig) -> Result<Vec<Action>, String> {
        Ok(native_decide(demand, cfg))
    }
}

#[must_use]
pub fn native_decide(demand: &[DemandRow], cfg: &ArmazemConfig) -> Vec<Action> {
    let mut out = Vec::new();
    if cfg.mode == Mode::Direct {
        return out;
    }
    let mut live_after = 0usize;
    let mut idle_live: Vec<&DemandRow> = Vec::new();
    for row in demand {
        if row.interest > 0 && !row.live {
            out.push(Action::Start(row.key.clone()));
            live_after += 1;
        } else if row.live {
            let idle = row.idle_ms.unwrap_or(0);
            if cfg.mode == Mode::Lazy && row.interest == 0 && idle >= cfg.linger_ms {
                out.push(Action::Park(row.key.clone()));
            } else {
                live_after += 1;
                if row.interest == 0 {
                    idle_live.push(row);
                }
            }
        } else if row.interest == 0
            && row.dormant_ms.is_some_and(|d| d >= cfg.evict_dormant_after_ms)
        {
            out.push(Action::Evict(row.key.clone()));
        }
    }
    if cfg.mode == Mode::Lazy && live_after > cfg.max_live {
        idle_live.sort_by(|a, b| b.idle_ms.cmp(&a.idle_ms).then_with(|| a.key.cmp(&b.key)));
        for row in idle_live.into_iter().take(live_after - cfg.max_live) {
            out.push(Action::Park(row.key.clone()));
        }
    }
    out.sort();
    out
}

pub const ZELADOR_PROGRAM: &str = include_str!("zelador.tlisp");

pub fn policy_for(cfg: &ArmazemConfig) -> Result<Box<dyn Policy>, ConfigError> {
    cfg.validate()?;
    match cfg.policy {
        PolicyEngine::Native => Ok(Box::new(NativePolicy)),
        #[cfg(feature = "vigy")]
        PolicyEngine::Vigy => Ok(Box::new(crate::vigy::VigyPolicy::new(ZELADOR_PROGRAM))),
        #[cfg(not(feature = "vigy"))]
        PolicyEngine::Vigy => Err(ConfigError::VigyNotCompiled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, interest: usize, live: bool, idle: Option<u64>, dormant: Option<u64>) -> DemandRow {
        DemandRow {
            key: key.into(),
            interest,
            live,
            idle_ms: idle,
            dormant_ms: dormant,
        }
    }

    #[test]
    fn interest_starts_idle_parks_after_linger_and_old_dormancy_evicts() {
        let cfg = ArmazemConfig::default();
        let got = native_decide(
            &[
                row("wanted", 1, false, None, None),
                row("fresh-idle", 0, true, Some(10), None),
                row("stale-idle", 0, true, Some(cfg.linger_ms), None),
                row("ancient", 0, false, None, Some(cfg.evict_dormant_after_ms)),
                row("young-dormant", 0, false, None, Some(1)),
            ],
            &cfg,
        );
        assert_eq!(
            got,
            [
                Action::Start("wanted".into()),
                Action::Park("stale-idle".into()),
                Action::Evict("ancient".into()),
            ]
        );
    }

    #[test]
    fn over_the_live_ceiling_the_longest_idle_are_parked_and_wanted_keys_never_are() {
        let cfg = ArmazemConfig {
            max_live: 2,
            ..ArmazemConfig::default()
        };
        let got = native_decide(
            &[
                row("a", 1, true, None, None),
                row("b", 0, true, Some(5), None),
                row("c", 0, true, Some(9), None),
                row("d", 2, true, None, None),
            ],
            &cfg,
        );
        assert_eq!(got, [Action::Park("b".into()), Action::Park("c".into())]);
    }

    #[test]
    fn eager_never_parks_and_direct_never_acts() {
        let eager = ArmazemConfig {
            mode: Mode::Eager,
            ..ArmazemConfig::default()
        };
        let idle = [row("x", 0, true, Some(u64::MAX), None)];
        assert!(native_decide(&idle, &eager).is_empty());
        let direct = ArmazemConfig {
            mode: Mode::Direct,
            ..ArmazemConfig::default()
        };
        assert!(native_decide(&[row("y", 3, false, None, None)], &direct).is_empty());
    }
}
