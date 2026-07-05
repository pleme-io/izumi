//! # izumi-lisp — the tatara-lisp authoring surface for izumi boards
//!
//! The Rust + Lisp bridge for the izumi board substrate: two
//! `#[derive(TataraDomain)]` forms — [`IzumiSourceForm`]
//! (`(defizumisource …)`) and [`IzumiBoardForm`] (`(defizumiboard …)`) —
//! plus the conversions onto the shikumi-tiered config surface in
//! `izumi-config` ([`SourceEntry`] / [`BoardConfig`]). A board an operator
//! would write as YAML becomes authorable (and macro-composable) as Lisp:
//!
//! ```lisp
//! (defizumiboard :ttl-secs 600)
//! (defizumisource :kind "jira-sprint"
//!                 :interval-secs 300
//!                 :params (:site "acme.atlassian.net"))
//! (defizumisource :kind "grafana-alerts" :enabled #f)
//! ```
//!
//! Every field on both forms except the source `kind` is an `Option<_>`:
//! **`None` = keep the config-side default** — an absent keyword never
//! overrides the prescribed tier, exactly matching the partial-YAML
//! semantics of `izumi-config` (a `(defizumiboard :ttl-secs 600)` changes
//! the TTL floor and nothing else).
//!
//! ## Ceremony
//!
//! The six-line contract from `tatara/docs/rust-lisp.md`, verbatim: derive
//! [`tatara_lisp::DeriveTataraDomain`] + `#[tatara(keyword = "…")]` on a
//! serde struct, then [`register`] both domains with the global dispatcher
//! so polymorphic consumers (`tatara-check`-style dispatchers,
//! `checks.lisp`) can compile the forms without naming the concrete types.
//! Field names map snake\_case ↔ kebab-case (`interval_secs` ↔
//! `:interval-secs`); booleans are `#t` / `#f`.
//!
//! ## Named gaps (pinned by tests, tied to the derive pin in `Cargo.toml`)
//!
//! The crate builds against the *published* tatara-lisp 0.2.4 (crates.io)
//! with the derive pinned to `=0.2.4` — see the `Cargo.toml` comment for
//! the resolution story. Three behaviors of that revision are gaps an
//! author must know about:
//!
//! 1. **`params` map keys are camelized by the bridge.** The Tier-1 serde
//!    bridge (`sexp_to_json`) turns a kwargs sublist into a JSON object and
//!    runs every key through kebab→camel, so `(:token-env "X")` lands in
//!    the [`SourceEntry::params`] map as `"tokenEnv"`, not `"token-env"`.
//!    Single-word keys (`:site`, `:jql`, `:folder`) and snake\_case keys
//!    (`:token_env`) pass through verbatim. Prefer those; the camelization
//!    is pinned by `params_map_keys_are_camelized_by_the_bridge`.
//! 2. **An empty `params` map cannot be authored inline.** `()` reads as
//!    the empty list (a JSON array, not an object) and fails the map
//!    deserialize — omit `:params` entirely; the conversion supplies the
//!    empty map.
//! 3. **Unknown keywords are silently ignored.** The 0.2.4 derive parses
//!    kwargs non-strictly (`parse_kwargs`, no allowed-set gate), so a
//!    typo'd `:ttl-sec` is dropped rather than rejected — unlike the
//!    `deny_unknown_fields` YAML path in `izumi-config`. The strict gate
//!    (`parse_kwargs_strict`) exists only in the unpublished tatara git
//!    HEAD; adopt it when the pin lifts.
//!
//! ## Numeric conversions
//!
//! Lisp integers are `i64`; the config side wants `u64` / `usize`. The
//! conversions treat a **negative authored value as absent** (keep the
//! default) rather than saturating: `0` is meaningful on the config
//! surface (`ttl_secs: 0` removes the floor, `per_source_cap: 0` uncaps),
//! so clamping a negative to `0` would silently mean something the author
//! never wrote.

use std::collections::BTreeMap;

use izumi_config::{BoardConfig, SourceEntry};
use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

/// `(defizumisource …)` — one per-source override row, the Lisp face of
/// [`SourceEntry`]. `kind` is the only required keyword; every other field
/// is `None` = keep the [`SourceEntry`] default (`enabled: true`, default
/// cadence, default cap, empty params).
///
/// ```lisp
/// (defizumisource :kind "jira-sprint"
///                 :interval-secs 300
///                 :max-items 5
///                 :params (:site "acme.atlassian.net"))
/// ```
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[tatara(keyword = "defizumisource")]
pub struct IzumiSourceForm {
    /// Source kind kebab slug (e.g. `git-branch-pr`). Required — a source
    /// override with no kind targets nothing. Unknown slugs ride along and
    /// are ignored downstream, mirroring the YAML surface.
    pub kind: String,
    /// Run this source. `None` = keep the config default (`true`), so a
    /// bare `(defizumisource :kind "x")` arms the source exactly like a
    /// kind-only YAML entry.
    pub enabled: Option<bool>,
    /// Override the poll cadence (seconds). Negative values are treated as
    /// absent by the conversion (see module docs).
    pub interval_secs: Option<i64>,
    /// Override the per-poll item cap. Negative values are treated as
    /// absent by the conversion (see module docs).
    pub max_items: Option<i64>,
    /// Free per-source params (token env override, JQL, grafana folder, …)
    /// authored as a kwargs sublist — `(:site "acme.atlassian.net")`.
    /// Rides the Tier-1 serde bridge; see the module docs for the
    /// key-camelization gap. `None` = empty map.
    pub params: Option<BTreeMap<String, String>>,
}

/// `(defizumiboard …)` — the board-wide knobs, the Lisp face of
/// [`BoardConfig`]. Every field is `None` = keep the prescribed-tier
/// default, so a form only ever *narrows* the delta it names:
///
/// ```lisp
/// (defizumiboard :ttl-secs 600)          ; TTL floor only, rest prescribed
/// (defizumiboard :enabled #f)            ; the whole board off
/// ```
///
/// The per-source override list is deliberately NOT a field here — sources
/// are authored as sibling `(defizumisource …)` forms and merged by the
/// consumer (the same split `izumi-config` keeps between [`BoardConfig`]
/// knobs and the `sources` list).
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[tatara(keyword = "defizumiboard")]
pub struct IzumiBoardForm {
    /// Master switch. `None` = prescribed (`true`).
    pub enabled: Option<bool>,
    /// Whether a source with no explicit override runs by default.
    /// `None` = prescribed (`true`).
    pub default_enabled: Option<bool>,
    /// Cap how many rows a single source may contribute to the visible
    /// band. `0` = no cap. `None` = prescribed (`3`).
    pub per_source_cap: Option<i64>,
    /// Global TTL FLOOR (seconds); `0` removes the floor (per-source
    /// `3× poll interval` fallback stays in force). `None` = prescribed
    /// (`900`).
    pub ttl_secs: Option<i64>,
    /// Lazily persist the cache to disk. `None` = prescribed (`true`).
    pub persist: Option<bool>,
    /// Coalesce disk writes to at most once per this many seconds; `0` =
    /// persist on every change. `None` = prescribed (`5`).
    pub persist_debounce_secs: Option<i64>,
    /// Hard cap on total cached items (memory insurance); `0` = unbounded.
    /// `None` = prescribed (`200`).
    pub max_entries: Option<i64>,
    /// `true` makes the consumer's `sources` list REPLACE the prescribed
    /// arm-list instead of merging over it. `None` = prescribed (`false`).
    pub sources_replace: Option<bool>,
}

/// Lisp `i64` → config `u64` with the module-doc semantics: negative =
/// absent (keep default), never saturate — `0` is meaningful config-side.
fn to_u64(n: Option<i64>) -> Option<u64> {
    n.and_then(|v| u64::try_from(v).ok())
}

/// Lisp `i64` → config `usize`, same negative-is-absent contract as
/// [`to_u64`].
fn to_usize(n: Option<i64>) -> Option<usize> {
    n.and_then(|v| usize::try_from(v).ok())
}

impl From<IzumiSourceForm> for SourceEntry {
    /// Lower the authored form onto the config row. `None` fields take the
    /// [`SourceEntry`] defaults byte-for-byte (`enabled: true`, no cadence
    /// override, no cap, empty params) so a Lisp-authored source and a
    /// YAML-authored source with the same present fields are equal values.
    fn from(form: IzumiSourceForm) -> Self {
        Self {
            kind: form.kind,
            enabled: form.enabled.unwrap_or(true),
            interval_secs: to_u64(form.interval_secs),
            max_items: to_usize(form.max_items),
            params: form.params.unwrap_or_default(),
        }
    }
}

impl IzumiBoardForm {
    /// Overlay this form onto an explicit base config: every `Some` field
    /// overrides, every `None` field keeps the base value. [`From`] uses
    /// the prescribed tier as the base ([`BoardConfig::default`]); reach
    /// for this directly to overlay onto a different base (e.g. a config
    /// already merged from YAML).
    #[must_use]
    pub fn apply_to(self, mut base: BoardConfig) -> BoardConfig {
        if let Some(v) = self.enabled {
            base.enabled = v;
        }
        if let Some(v) = self.default_enabled {
            base.default_enabled = v;
        }
        if let Some(v) = to_usize(self.per_source_cap) {
            base.per_source_cap = v;
        }
        if let Some(v) = to_u64(self.ttl_secs) {
            base.ttl_secs = v;
        }
        if let Some(v) = self.persist {
            base.persist = v;
        }
        if let Some(v) = to_u64(self.persist_debounce_secs) {
            base.persist_debounce_secs = v;
        }
        if let Some(v) = to_usize(self.max_entries) {
            base.max_entries = v;
        }
        if let Some(v) = self.sources_replace {
            base.sources_replace = v;
        }
        base
    }
}

impl From<IzumiBoardForm> for BoardConfig {
    /// Lower the authored form onto the prescribed tier —
    /// `(defizumiboard :ttl-secs 600)` produces exactly
    /// `BoardConfig { ttl_secs: 600, ..prescribed }`.
    fn from(form: IzumiBoardForm) -> Self {
        form.apply_to(Self::default())
    }
}

/// Register both izumi domains with the tatara-lisp global dispatcher, so
/// polymorphic consumers (`checks.lisp`, registry-driven tooling) can
/// compile `(defizumisource …)` / `(defizumiboard …)` without naming the
/// concrete types. Idempotent — repeated registrations overwrite. Call
/// once from the consuming binary's `main`.
pub fn register() {
    tatara_lisp::domain::register::<IzumiSourceForm>();
    tatara_lisp::domain::register::<IzumiBoardForm>();
}

#[cfg(test)]
mod tests {
    use tatara_lisp::compile_typed;

    use super::*;

    #[test]
    fn defizumisource_compiles_and_converts_onto_source_entry() {
        // The canonical authored row: kind + cadence + a params sublist.
        // Kebab keywords land on snake_case fields; the params kwargs
        // sublist rides the Tier-1 serde bridge into the BTreeMap.
        let src = r#"
            (defizumisource :kind "jira-sprint"
                            :interval-secs 300
                            :params (:site "acme.atlassian.net"))
        "#;
        let forms = compile_typed::<IzumiSourceForm>(src).unwrap();
        assert_eq!(forms.len(), 1);
        let form = &forms[0];
        assert_eq!(form.kind, "jira-sprint");
        assert_eq!(form.enabled, None, "absent keyword stays None");
        assert_eq!(form.interval_secs, Some(300));
        assert_eq!(form.max_items, None);
        assert_eq!(
            form.params.as_ref().unwrap().get("site").map(String::as_str),
            Some("acme.atlassian.net"),
        );

        // None = keep default on the conversion: enabled defaults true,
        // absent knobs stay unset, params materialize.
        let entry: SourceEntry = form.clone().into();
        assert_eq!(entry.kind, "jira-sprint");
        assert!(entry.enabled, "absent :enabled arms the source (default_true)");
        assert_eq!(entry.interval_secs, Some(300));
        assert_eq!(entry.max_items, None);
        assert_eq!(
            entry.params.get("site").map(String::as_str),
            Some("acme.atlassian.net"),
        );

        // A Lisp-authored entry equals the typed-constructed one with the
        // same present fields — the two authoring surfaces converge.
        let mut expect = SourceEntry::enable("jira-sprint");
        expect.interval_secs = Some(300);
        expect
            .params
            .insert(String::from("site"), String::from("acme.atlassian.net"));
        assert_eq!(entry, expect);
    }

    #[test]
    fn explicit_disable_and_caps_land() {
        let src = r#"(defizumisource :kind "grafana-alerts" :enabled #f :max-items 5)"#;
        let forms = compile_typed::<IzumiSourceForm>(src).unwrap();
        let entry: SourceEntry = forms[0].clone().into();
        assert!(!entry.enabled, "#f disarms exactly this kind");
        assert_eq!(entry.max_items, Some(5));
        assert_eq!(entry.interval_secs, None);
        assert!(entry.params.is_empty(), "omitted :params = empty map");
    }

    #[test]
    fn params_map_keys_are_camelized_by_the_bridge() {
        // Named gap #1 (module docs): sexp_to_json runs kwargs-sublist
        // keys through kebab→camel, so a dashed param key arrives
        // camelized in the map. Snake_case keys pass through verbatim.
        // Pinned so a future derive/bridge upgrade that changes this
        // surfaces here, not in a consumer.
        let src = r#"
            (defizumisource :kind "jira-sprint"
                            :params (:token-env "IZUMI_JIRA_TOKEN"
                                     :token_env "SNAKE_SURVIVES"))
        "#;
        let forms = compile_typed::<IzumiSourceForm>(src).unwrap();
        let params = forms[0].params.clone().unwrap();
        assert_eq!(
            params.get("tokenEnv").map(String::as_str),
            Some("IZUMI_JIRA_TOKEN"),
            "dashed keyword key lands camelized"
        );
        assert!(
            !params.contains_key("token-env"),
            "the kebab spelling does NOT survive the bridge"
        );
        assert_eq!(
            params.get("token_env").map(String::as_str),
            Some("SNAKE_SURVIVES"),
            "snake_case keys pass through verbatim"
        );
    }

    #[test]
    fn defizumiboard_compiles_and_converts_onto_board_config() {
        // The headline shape from the task: one knob named, everything
        // else stays at the prescribed tier.
        let forms = compile_typed::<IzumiBoardForm>("(defizumiboard :ttl-secs 600)").unwrap();
        assert_eq!(forms.len(), 1);
        assert_eq!(forms[0].ttl_secs, Some(600));
        assert_eq!(forms[0].enabled, None);

        let cfg: BoardConfig = forms[0].clone().into();
        assert_eq!(cfg.ttl_secs, 600, "the named knob wins");
        // Every other field is the prescribed default, byte-for-byte.
        let prescribed = BoardConfig::default();
        assert_eq!(cfg.enabled, prescribed.enabled);
        assert_eq!(cfg.default_enabled, prescribed.default_enabled);
        assert_eq!(cfg.per_source_cap, prescribed.per_source_cap);
        assert_eq!(cfg.persist, prescribed.persist);
        assert_eq!(cfg.persist_debounce_secs, prescribed.persist_debounce_secs);
        assert_eq!(cfg.max_entries, prescribed.max_entries);
        assert_eq!(cfg.sources_replace, prescribed.sources_replace);
        assert!(cfg.sources.is_empty(), "sources are sibling forms, never board knobs");
    }

    #[test]
    fn every_board_knob_overrides_and_zero_is_meaningful() {
        // All eight knobs named at once; 0 must land as 0 (uncapped /
        // floor-removed / write-every-change), never be confused with
        // "absent".
        let src = r"
            (defizumiboard :enabled #f
                           :default-enabled #f
                           :per-source-cap 0
                           :ttl-secs 0
                           :persist #f
                           :persist-debounce-secs 0
                           :max-entries 0
                           :sources-replace #t)
        ";
        let cfg: BoardConfig = compile_typed::<IzumiBoardForm>(src).unwrap()[0].clone().into();
        assert!(!cfg.enabled);
        assert!(!cfg.default_enabled);
        assert_eq!(cfg.per_source_cap, 0);
        assert_eq!(cfg.ttl_secs, 0);
        assert!(!cfg.persist);
        assert_eq!(cfg.persist_debounce_secs, 0);
        assert_eq!(cfg.max_entries, 0);
        assert!(cfg.sources_replace);
    }

    #[test]
    fn negative_numbers_keep_the_default_never_saturate() {
        // The numeric-conversion contract from the module docs: a negative
        // authored value is treated as absent, because clamping to 0 would
        // silently mean "uncapped/no-floor" — something the author never
        // wrote.
        let src = r#"(defizumisource :kind "x" :interval-secs -1 :max-items -5)"#;
        let entry: SourceEntry =
            compile_typed::<IzumiSourceForm>(src).unwrap()[0].clone().into();
        assert_eq!(entry.interval_secs, None);
        assert_eq!(entry.max_items, None);

        let board = "(defizumiboard :ttl-secs -600 :per-source-cap -1)";
        let cfg: BoardConfig = compile_typed::<IzumiBoardForm>(board).unwrap()[0].clone().into();
        let prescribed = BoardConfig::default();
        assert_eq!(cfg.ttl_secs, prescribed.ttl_secs, "negative TTL keeps prescribed 900");
        assert_eq!(cfg.per_source_cap, prescribed.per_source_cap, "negative cap keeps 3");
    }

    #[test]
    fn one_lisp_file_authors_the_board_and_its_sources() {
        // The intended authoring unit: one .lisp file carrying the board
        // knobs plus N source rows; each compile_typed pass picks exactly
        // its own keyword's forms.
        let src = r#"
            (defizumiboard :ttl-secs 600)
            (defizumisource :kind "jira-sprint" :interval-secs 300)
            (defizumisource :kind "git-branch-pr")
        "#;
        let boards = compile_typed::<IzumiBoardForm>(src).unwrap();
        let sources = compile_typed::<IzumiSourceForm>(src).unwrap();
        assert_eq!(boards.len(), 1);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].kind, "jira-sprint");
        assert_eq!(sources[1].kind, "git-branch-pr");
    }

    #[test]
    fn lisp_macros_compose_source_presets() {
        // The Free-middle invariant (rust-lisp.md): a defmacro rewrites
        // into the typed form and re-enters the typed boundary — an
        // "hourly" preset costs one macro, no new Rust.
        let src = r#"
            (defmacro hourly (kind) `(defizumisource :kind ,kind :interval-secs 3600))
            (hourly "aws-health")
        "#;
        let forms = compile_typed::<IzumiSourceForm>(src).unwrap();
        assert_eq!(forms.len(), 1);
        assert_eq!(forms[0].kind, "aws-health");
        assert_eq!(forms[0].interval_secs, Some(3600));
    }

    #[test]
    fn register_installs_both_domains_in_the_dispatcher() {
        register();
        let source = tatara_lisp::domain::lookup("defizumisource").expect("registered");
        assert!(tatara_lisp::domain::lookup("defizumiboard").is_some());

        // Dispatch a compile through the erased handler — the registry
        // path a polymorphic consumer (checks.lisp) takes.
        let forms = tatara_lisp::read(r#"(defizumisource :kind "tend-repos")"#).unwrap();
        let list = forms[0].as_list().unwrap();
        let json = (source.compile)(&list[1..]).unwrap();
        assert_eq!(json["kind"], "tend-repos");
    }
}
