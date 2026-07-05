//! izumi-board — the headless fresh-source board daemon/CLI (consumer #2 of
//! the izumi substrate; mado's Ctrl-S picker is consumer #1).
//!
//! One binary, two faces over one typed board:
//!
//! * **`izumi-board serve`** — the daemon. The [`izumi::Engine`] runs one
//!   paced watcher per enabled source in the 25-provider
//!   [`registry`] (the same generic providers mado's
//!   suggestion plane polls), feeding the living-board [`izumi::Store`]
//!   (recurrence tombstones, aging escalation, lifecycle soft-ack, source
//!   health). A maintenance loop owns decay + gc + the debounced,
//!   generation-gated, writer-election-gated snapshot persist (frame magic
//!   `izumi-board v1`), and a unix control socket serves the typed
//!   newline-delimited-JSON protocol.
//! * **CLI verbs** — `list` / `json` ask the live daemon first and degrade
//!   to a READ-ONLY parse of the persisted snapshot (the catalog-erased
//!   [`izumi::raw`] reader) when no daemon answers, with the degradation
//!   named on stderr; `dismiss` / `accept` / `nudge` are lifecycle
//!   mutations and are socket-only — no daemon, no mutation, typed refusal.
//!
//! Configuration is shikumi-tiered ([`config`]): `~/.config/izumi/izumi.yaml`
//! overlaid on the prescribed default (`IZUMI_TIER` selects the baseline;
//! a missing file IS the prescribed default). State (snapshot + socket +
//! writer lock) lives under `$IZUMI_STATE_DIR`/the OS state dir ([`state`]).
//!
//! # ⚠ Double-polling hazard
//!
//! izumi's `HostPacer` is **per-process**: arming izumi-board beside mado on
//! the same workstation doubles upstream QPS against github / atlassian /
//! grafana / … — the two processes' pacing buckets cannot see each other.
//! Do NOT arm this daemon while mado's suggestion engine runs with
//! overlapping sources (see the repo `CLAUDE.md`'s double-polling note);
//! deployment surfaces default it OFF for exactly this reason. Disarm one
//! side (`enabled: false`, or disjoint `sources:` lists) before running both.

mod catalog;
mod client;
mod config;
mod protocol;
mod registry;
mod serve;
mod state;

use clap::{Parser, Subcommand};

/// The headless izumi fresh-source board — daemon + CLI.
#[derive(Parser)]
#[command(name = "izumi-board", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the board daemon: watchers + maintenance + the control socket.
    Serve,
    /// Print the ranked board, one row per line (live daemon first; degrades
    /// to the persisted snapshot read-only).
    List {
        /// Row cap (clamped 1..=200).
        #[arg(long, default_value_t = 20)]
        max: usize,
    },
    /// Print the board as JSON — rows + per-source health (live daemon
    /// first; degrades to the persisted snapshot read-only, empty health).
    Json {
        /// Row cap (clamped 1..=200).
        #[arg(long, default_value_t = 50)]
        max: usize,
    },
    /// Dismiss a row by id — never offered again (or snooze it instead).
    Dismiss {
        /// The row id (the decimal string `list`/`json` print).
        id: String,
        /// Snooze for this many seconds instead of dismissing forever.
        #[arg(long)]
        snooze: Option<u64>,
    },
    /// Mark a row in-progress under a session name (soft-ack: demoted,
    /// badged, never removed).
    Accept {
        /// The row id (the decimal string `list`/`json` print).
        id: String,
        /// The session name the acceptance is working under.
        session: String,
    },
    /// Ask every watcher to re-poll right now (paced per-watcher — a nudge
    /// storm cannot hammer an API).
    Nudge,
}

/// Route tracing to stderr (stdout stays machine-parseable for `list`/`json`)
/// with the standard `RUST_LOG` env-filter, defaulting to `info`.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> std::process::ExitCode {
    init_tracing();
    let cli = Cli::parse();
    let outcome: Result<(), Box<dyn std::error::Error>> = match cli.cmd {
        Command::Serve => serve::run().map_err(Into::into),
        Command::List { max } => client::list(max).map_err(Into::into),
        Command::Json { max } => client::json(max).map_err(Into::into),
        Command::Dismiss { id, snooze } => client::dismiss(&id, snooze).map_err(Into::into),
        Command::Accept { id, session } => client::accept(&id, &session).map_err(Into::into),
        Command::Nudge => client::nudge().map_err(Into::into),
    };
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(err = %err, "izumi-board failed");
            std::process::ExitCode::FAILURE
        }
    }
}
