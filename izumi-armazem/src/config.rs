use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Lazy,
    Eager,
    Direct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyEngine {
    #[default]
    Vigy,
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct ArmazemConfig {
    pub mode: Mode,
    pub policy: PolicyEngine,
    pub linger_ms: u64,
    pub max_live: usize,
    pub evict_dormant_after_ms: u64,
    pub journal_cap: usize,
    pub tick_ms: u64,
}

impl Default for ArmazemConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Lazy,
            policy: PolicyEngine::Vigy,
            linger_ms: 30_000,
            max_live: 12,
            evict_dormant_after_ms: 3_600_000,
            journal_cap: 4096,
            tick_ms: 1000,
        }
    }
}

impl ArmazemConfig {
    #[must_use]
    pub fn tick(&self) -> Duration {
        Duration::from_millis(self.tick_ms.max(1))
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.journal_cap == 0 {
            return Err(ConfigError::ZeroJournal);
        }
        if self.max_live == 0 && self.mode != Mode::Direct {
            return Err(ConfigError::ZeroLive);
        }
        if self.policy == PolicyEngine::Vigy && !cfg!(feature = "vigy") {
            return Err(ConfigError::VigyNotCompiled);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    ZeroJournal,
    ZeroLive,
    VigyNotCompiled,
    Policy(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroJournal => f.write_str("journal-cap must be at least 1"),
            Self::ZeroLive => f.write_str("max-live must be at least 1 unless mode is direct"),
            Self::VigyNotCompiled => f.write_str(
                "policy is vigy but izumi-armazem was built without the `vigy` feature; set policy: native or enable the feature",
            ),
            Self::Policy(e) => write!(f, "zelador policy: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}
