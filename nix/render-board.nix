# render-board.nix — the ONE fleet-shared YAML-body renderer for the
# izumi board config: the omission-rule half of ./board-options.nix.
#
# BYTE-PARITY PROMISE: for equal inputs this produces EXACTLY what
# mado's inline `suggestionsBody` renderer produces today — same key
# names, same set-only omission rules — so mado can swap its inline
# render for `izumi.lib.renderBoardBody` with a byte-identical YAML
# output. The rules:
#
#   * nullable scalars render ONLY when set (null = omit — the Rust
#     prescribed default wins);
#   * a `sources` entry is { kind } plus set-only optionals; `params`
#     only when non-empty;
#   * the `sources` list renders only when non-empty;
#   * an untouched cfg renders the EMPTY attrset — the CONSUMER decides
#     whether/where the body lands (mado wraps it under a `suggestions:`
#     key only when non-empty; izumi-board renders it at the TOP LEVEL
#     of ~/.config/izumi/izumi.yaml).
#
# Pure: `lib: cfg: <attrset>` — never imports nixpkgs. The attrset feeds
# pkgs.formats.yaml (which sorts keys), so merge order here carries no
# byte-level meaning — only presence/absence does.
lib: cfg:
let
  scalarOpt = name: v: lib.optionalAttrs (v != null) { ${name} = v; };
  renderSource = src:
    { kind = src.kind; }
    // scalarOpt "enabled" src.enabled
    // scalarOpt "interval_secs" src.interval_secs
    // scalarOpt "max_items" src.max_items
    // lib.optionalAttrs (src.params != { }) { params = src.params; };
in
  scalarOpt "enabled" cfg.enabled
  // scalarOpt "persist" cfg.persist
  // scalarOpt "ttl_secs" cfg.ttl_secs
  // scalarOpt "max_entries" cfg.max_entries
  // scalarOpt "persist_debounce_secs" cfg.persist_debounce_secs
  // scalarOpt "default_enabled" cfg.default_enabled
  // scalarOpt "sources_replace" cfg.sources_replace
  # `or null` tolerates a cfg evaluated against mado's pre-izumi
  # submodule (which lacks per_source_cap), so mado can adopt the
  # renderer before the schema (or vice versa) — no lockstep commit.
  # Absent and null both omit, preserving byte parity either way.
  // scalarOpt "per_source_cap" (cfg.per_source_cap or null)
  // lib.optionalAttrs (cfg.sources != [ ]) {
    sources = map renderSource cfg.sources;
  }
