use std::sync::Arc;

use tatara_lisp_eval::{MapKey, Value};
use vigy_eval::{
    closure_extension, evaluate_with_extensions, standard_extensions, ExtArity, ExtEvalError,
    Span, VigyHost,
};
use vigy_types::ReconcileAction;

use crate::config::{ArmazemConfig, Mode};
use crate::zelador::{Action, DemandRow, Policy};

pub struct VigyPolicy {
    program: String,
}

impl VigyPolicy {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

fn kw(k: &str) -> MapKey {
    MapKey::Keyword(Arc::from(k))
}

fn int(n: u64) -> Value {
    Value::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn row_value(r: &DemandRow) -> Value {
    let mut entries = vec![
        (kw("key"), Value::string(r.key.as_str())),
        (kw("interest"), int(r.interest as u64)),
        (kw("live"), Value::Bool(r.live)),
    ];
    if let Some(ms) = r.idle_ms {
        entries.push((kw("idle-ms"), int(ms)));
    }
    if let Some(ms) = r.dormant_ms {
        entries.push((kw("dormant-ms"), int(ms)));
    }
    Value::map(entries)
}

fn config_value(cfg: &ArmazemConfig) -> Value {
    let mode = match cfg.mode {
        Mode::Lazy => "lazy",
        Mode::Eager => "eager",
        Mode::Direct => "direct",
    };
    Value::map([
        (kw("mode"), Value::string(mode)),
        (kw("linger-ms"), int(cfg.linger_ms)),
        (kw("max-live"), int(cfg.max_live as u64)),
        (kw("evict-dormant-after-ms"), int(cfg.evict_dormant_after_ms)),
    ])
}

fn emit(op: &'static str) -> impl Fn(&[Value], &mut VigyHost, Span) -> Result<Value, ExtEvalError> {
    move |args: &[Value], host: &mut VigyHost, sp: Span| match args.first() {
        Some(Value::Str(k)) => {
            host.actions
                .push(ReconcileAction::custom(serde_json::json!({ "op": op, "key": k.as_ref() })));
            Ok(Value::Nil)
        }
        Some(other) => Err(ExtEvalError::type_mismatch("string", other.type_name(), sp)),
        None => Err(ExtEvalError::native_fn(op, "expected a key", sp)),
    }
}

fn parse(action: &ReconcileAction) -> Option<Action> {
    let p = action.payload.as_ref()?;
    let key = p.get("key")?.as_str()?.to_owned();
    match p.get("op")?.as_str()? {
        "start" => Some(Action::Start(key)),
        "park" => Some(Action::Park(key)),
        "evict" => Some(Action::Evict(key)),
        _ => None,
    }
}

impl Policy for VigyPolicy {
    fn decide(&self, demand: &[DemandRow], cfg: &ArmazemConfig) -> Result<Vec<Action>, String> {
        let rows = Value::list(demand.iter().map(row_value));
        let config = config_value(cfg);
        let mut extensions = standard_extensions();
        extensions.push(closure_extension(move |interp| {
            let rows = rows.clone();
            interp.register_fn(
                "armazem-demand",
                ExtArity::Exact(0),
                move |_: &[Value], _: &mut VigyHost, _: Span| Ok(rows.clone()),
            );
            let config = config.clone();
            interp.register_fn(
                "armazem-config",
                ExtArity::Exact(0),
                move |_: &[Value], _: &mut VigyHost, _: Span| Ok(config.clone()),
            );
            interp.register_fn("armazem-start", ExtArity::Exact(1), emit("start"));
            interp.register_fn("armazem-park", ExtArity::Exact(1), emit("park"));
            interp.register_fn("armazem-evict", ExtArity::Exact(1), emit("evict"));
        }));
        let host = evaluate_with_extensions(&self.program, VigyHost::default(), &extensions)
            .map_err(|e| e.to_string())?;
        let mut out: Vec<Action> = host.actions.iter().filter_map(parse).collect();
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::zelador::{native_decide, ZELADOR_PROGRAM};

    fn demand_strategy() -> impl Strategy<Value = Vec<DemandRow>> {
        prop::collection::btree_map(
            "[a-f]{1,3}",
            (
                0usize..3,
                any::<bool>(),
                prop::option::of(0u64..100_000),
                prop::option::of(0u64..8_000_000),
            ),
            0..10,
        )
        .prop_map(|m| {
            m.into_iter()
                .map(|(key, (interest, live, idle, dormant))| DemandRow {
                    key,
                    interest,
                    live,
                    idle_ms: if live && interest == 0 { idle } else { None },
                    dormant_ms: if live { None } else { dormant },
                })
                .collect()
        })
    }

    fn config_strategy() -> impl Strategy<Value = ArmazemConfig> {
        (
            prop_oneof![Just(Mode::Lazy), Just(Mode::Eager), Just(Mode::Direct)],
            1usize..5,
            0u64..60_000,
        )
            .prop_map(|(mode, max_live, linger_ms)| ArmazemConfig {
                mode,
                max_live,
                linger_ms,
                ..ArmazemConfig::default()
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn the_vigy_zelador_decides_exactly_as_the_native_policy(
            demand in demand_strategy(),
            cfg in config_strategy(),
        ) {
            let vigy = VigyPolicy::new(ZELADOR_PROGRAM).decide(&demand, &cfg).unwrap();
            prop_assert_eq!(vigy, native_decide(&demand, &cfg));
        }
    }

    #[test]
    fn a_broken_program_is_an_error_not_an_empty_decision() {
        let err = VigyPolicy::new("(armazem-start 7)")
            .decide(&[], &ArmazemConfig::default())
            .unwrap_err();
        assert!(err.contains("string"), "{err}");
    }
}
