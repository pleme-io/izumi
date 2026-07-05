{
  description = "izumi — the fresh-source board substrate (continuously-refreshing, ranked, actionable data sources)";

  # Canonical pleme-io Rust-tool consumer flake. substrate.rust.tool pre-binds
  # nixpkgs / crate2nix / flake-utils / fenix / devenv / gen — every dependency
  # the build kit needs — so a substrate bump propagates fleet-wide without
  # touching this file. toolName (izumi-board) + repo are read from the typed
  # `flake_metadata.izumi-board` in Cargo.build-spec.json.
  inputs.substrate.url = "github:pleme-io/substrate";

  outputs = { substrate, ... }:
  let
    # The fleet-shared board schema + renderer (the Nix macro layer — the
    # PRIME DIRECTIVE applied to board config). Pure lib-parameterized
    # functions under nix/ — each eval-testable standalone, no nixpkgs
    # import — consumed here for izumi-board's own trio and re-exported
    # on the `lib` flake output below so every other consumer (mado's
    # extraHmOptions swap, future frost/praca board modules) declares
    # the IDENTICAL schema: `inputs.izumi.lib.mkBoardOption`.
    boardOptions    = import ./nix/board-options.nix;
    renderBoardBody = import ./nix/render-board.nix;

    # The canonical substrate Rust-tool surface (binary + HM/NixOS/Darwin
    # module trio + the six operator verbs).
    base = substrate.rust.tool {
      src = ./.;
      member = "izumi-board"; # the deployable bin; the other 4 members are libraries

      module = {
        # Pin the trio name explicitly — extraHmConfigFn below writes
        # `services.izumi-board.settings` literally, and the two must
        # never drift apart.
        name = "izumi-board";
        description = "izumi-board — headless fresh-source board daemon";

        # shikumi YAML at the izumi config home (~/.config/izumi/ — the
        # workspace-wide config dir, not a per-binary one). Gated on
        # `enable` so a disabled module leaves no config file behind.
        withShikumiConfig   = true;
        shikumiConfigPath   = ".config/izumi/izumi.yaml";
        shikumiGateOnEnable = true;

        # Native user-daemon wiring (substrate mkModuleTrio): a launchd
        # agent (Darwin) / systemd user unit (Linux) running
        # `izumi-board serve`. DISABLED BY DEFAULT at TWO gates —
        # programs.izumi-board.enable (mkEnableOption, false) AND
        # programs.izumi-board.daemon.enable (false) — because of the
        # DOUBLE-POLLING HAZARD (CLAUDE.md): HostPacer is per-process,
        # so arming izumi-board beside mado on a workstation doubles
        # upstream QPS against github/atlassian/grafana. Workstation
        # profiles must not arm this while mado's suggestion engine
        # runs with overlapping sources.
        withUserDaemon       = true;
        userDaemonSubcommand = "serve";

        # ── The typed board surface (fleet-shared schema) ────────────
        # The same rich list-of-submodule schema as mado's `suggestions`
        # option — declared ONCE in nix/board-options.nix, consumed
        # identically here and (post-swap) in mado. Function-form
        # receives `lib` from substrate so this flake never declares
        # nixpkgs as an input.
        extraHmOptions = lib: {
          board = boardOptions.mkBoardOption lib ''
            Typed config for the izumi board (the living board).
            Rendered at the TOP LEVEL of ~/.config/izumi/izumi.yaml —
            izumi-board's whole config IS the board (unlike mado, where
            the same body nests under `suggestions:`). Per-kind
            `sources` entries MERGE over the prescribed arm-list
            (BoardConfig::effective_sources) — a params-only entry
            never disarms the rest.
          '';
          extraSettings = lib.mkOption {
            type = lib.types.attrs;
            default = { };
            description = "Additional raw settings merged on top of the typed YAML (LAST — raw wins). Do NOT set keys the typed `board` option owns: extraSettings shallow-merges last and would clobber the typed render wholesale (the cross-module tear.* clobber class, 2026-07-02).";
          };
        };

        # Typed board render: nullable scalars appear only when set; a
        # source entry is {kind} plus set-only optionals; an untouched
        # `board` renders {} — a consumer that never touches the option
        # gets a byte-identical (empty) YAML. The body lands at the TOP
        # LEVEL of the settings tree, and extraSettings merges LAST
        # (raw wins) — the mado precedent.
        extraHmConfigFn = { cfg, lib, ... }: {
          services.izumi-board.settings =
            (renderBoardBody lib cfg.board) // cfg.extraSettings;
        };
      };
    };
  in
    # `base` carries per-system packages/devShells/apps/checks plus
    # overlays.default + the three module outputs — and NO `lib` today
    # (verified against substrate's flake-wrapper.nix). recursiveUpdate
    # (not //) keeps this future-proof: if substrate ever grows a lib
    # output, the two merge instead of clobbering.
    substrate.inputs.nixpkgs.lib.recursiveUpdate base {
      # The fleet-shared board vocabulary. Consumers:
      #   inputs.izumi.lib.mkBoardOption lib "<description>"  → complete option
      #   inputs.izumi.lib.mkBoardSubmodule lib               → just the type
      #   inputs.izumi.lib.renderBoardBody lib cfg            → YAML body attrset
      lib = {
        inherit (boardOptions) mkBoardSubmodule mkBoardOption;
        inherit renderBoardBody;
      };
    };
}
