# nix/ — the fleet-shared board config vocabulary

Pure `lib`-parameterized functions (no nixpkgs import), exported on the
flake's `lib` output:

| Export | Shape | Purpose |
|---|---|---|
| `mkBoardSubmodule` | `lib → types.submodule` | just the option TYPE (nest it anywhere) |
| `mkBoardOption` | `lib → description → mkOption` | complete option, `default = { }` |
| `renderBoardBody` | `lib → cfg → attrset` | the set-only YAML-body renderer |

**Consumer pattern.** mado swaps its inline `suggestions` schema+render for
`inputs.izumi.lib.mkBoardOption lib "…"` in `extraHmOptions` and wraps
`renderBoardBody lib cfg.suggestions` under its `suggestions:` key;
izumi-board's own module trio (this repo's flake.nix) renders the same body
at the TOP LEVEL of `~/.config/izumi/izumi.yaml`.

**Byte-parity promise.** For equal inputs `renderBoardBody` produces exactly
what mado's inline renderer produces today (same keys, same omission rules;
`per_source_cap` is the one izumi addition and omits when null/absent) —
proven by the parity eval-check in the extraction session.

**Double-polling hazard.** `HostPacer` is per-process: arming the izumi-board
daemon beside mado with overlapping sources doubles upstream QPS. The HM
module is therefore OFF by default at two gates (`enable` + `daemon.enable`).
