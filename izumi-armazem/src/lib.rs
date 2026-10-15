#![forbid(unsafe_code)]

pub mod armazem;
pub mod config;
pub mod shelf;
#[cfg(feature = "vigy")]
pub mod vigy;
pub mod zelador;

pub use armazem::{Armazem, BoxFuture, Feed, Interesse, KeyState, Source};
pub use config::{ArmazemConfig, ConfigError, Mode, PolicyEngine};
pub use shelf::{Change, Phase, Record, Seq, Since, Snapshot};
pub use zelador::{native_decide, Action, DemandRow, NativePolicy, Policy, ZELADOR_PROGRAM};
