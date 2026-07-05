# board-options.nix — the ONE fleet-shared izumi board options schema.
#
# PRIME DIRECTIVE macro layer: this schema was born in mado's flake.nix
# (the M2 typed surface for the Ctrl-S suggestion stream, flake.nix
# `suggestions` submodule) and is lifted here so EVERY consumer module
# declares the identical option surface — mado's extraHmOptions swap,
# izumi-board's own module trio (this repo's flake.nix), and any future
# frost/praca board module. One schema, N consumers; drift is a bug.
#
# izumi adds exactly ONE field beyond mado's proven schema:
# `per_source_cap` (board-level row-diversity cap). Everything else is
# field-for-field the mado shape — same names, same nullability, same
# "null = omit, the Rust prescribed default wins" contract.
#
# Pure: every function takes `lib` as a parameter — this file never
# imports nixpkgs. Two granularities are exported:
#
#   mkBoardSubmodule = lib: <types.submodule>
#     Just the option TYPE — for consumers that author their own
#     mkOption around it (custom description, custom default, nesting
#     the body under another YAML key the way mado nests `suggestions:`).
#
#   mkBoardOption = lib: description: <mkOption>
#     A complete option (default = { }) — for consumers that want the
#     whole surface in one line: `board = izumi.lib.mkBoardOption lib "…";`.
#
# Render the resulting cfg with ./render-board.nix — the matching
# omission-rule renderer. Schema + renderer move together.
let
  mkBoardSubmodule = lib: lib.types.submodule {
    options = {
      enabled = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Master switch. null = the consumer's prescribed default (on). With engine hot-reload, flipping this at runtime parks/revives the engine without a restart.";
      };
      persist = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Persist the board snapshot across restarts. null = the consumer's prescribed default.";
      };
      ttl_secs = lib.mkOption {
        type = lib.types.nullOr lib.types.int;
        default = null;
        description = "Global row TTL floor in seconds. null = the consumer's prescribed default.";
      };
      max_entries = lib.mkOption {
        type = lib.types.nullOr lib.types.int;
        default = null;
        description = "Hard cap on stored rows (rank-ordered GC). null = the consumer's prescribed default.";
      };
      persist_debounce_secs = lib.mkOption {
        type = lib.types.nullOr lib.types.int;
        default = null;
        description = "Snapshot write coalescing cadence. null = the consumer's prescribed default.";
      };
      default_enabled = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Whether kinds absent from `sources` are armed. null = the consumer's prescribed default.";
      };
      sources_replace = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Escape hatch: true = `sources` REPLACES the prescribed arm-list instead of merging over it.";
      };
      # The one izumi extension over mado's proven schema.
      per_source_cap = lib.mkOption {
        type = lib.types.nullOr lib.types.int;
        default = null;
        description = "Board-level row-diversity cap: at most N ranked rows per source kind, so one noisy source never monopolizes the board. null = the consumer's prescribed default (uncapped).";
      };
      sources = lib.mkOption {
        default = [ ];
        description = "Per-kind overrides (credentials, cadence, params). Merged over the prescribed arm-list by kind (BoardConfig::effective_sources) — a params-only entry never disarms the rest.";
        type = lib.types.listOf (lib.types.submodule {
          options = {
            kind = lib.mkOption {
              type = lib.types.str;
              description = "Source kind slug (e.g. jira-assigned, flux-failing, github-actions-failing).";
            };
            enabled = lib.mkOption {
              type = lib.types.nullOr lib.types.bool;
              default = null;
              description = "Arm/disarm this kind. null = omit (the per-kind prescribed default).";
            };
            interval_secs = lib.mkOption {
              type = lib.types.nullOr lib.types.int;
              default = null;
              description = "Poll cadence override in seconds. null = omit.";
            };
            max_items = lib.mkOption {
              type = lib.types.nullOr lib.types.int;
              default = null;
              description = "Row budget override for this kind. null = omit.";
            };
            params = lib.mkOption {
              type = lib.types.attrs;
              default = { };
              description = "Kind-specific params (site, base_url, secret paths, context, repos, ...). Rendered only when non-empty.";
            };
          };
        });
      };
    };
  };
in
{
  inherit mkBoardSubmodule;

  mkBoardOption = lib: description: lib.mkOption {
    default = { };
    inherit description;
    type = mkBoardSubmodule lib;
  };
}
