//! Command-line interface (clap derive).
//!
//! Verbs:
//! - `run`   — start the relay daemon (default config `config.yaml`).
//! - `login` — one-time interactive sign-in; persists the session file.
//! - `chats` — list all dialogs (id / type / title) to find chat ids.
//! - `check` — validate config + webhooks (and routes if a session exists),
//!   print a table, exit 0 on success / 1 on any failure. Sends nothing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Default config path when `--config` is omitted.
pub const DEFAULT_CONFIG: &str = "config.yaml";
/// Session file path (SQLite-backed grammers session).
pub const SESSION_FILE: &str = "relay.session";

/// Default `backfill --count` when omitted.
pub const BACKFILL_DEFAULT_COUNT: usize = 3;
/// Hard cap on `backfill --count` — backfill is an on-demand nudge, not a
/// history export; keep the burst small so it can't accidentally fan out an
/// unbounded pile of old posts to every webhook on the route.
pub const BACKFILL_MAX_COUNT: usize = 25;

/// Clamp a requested `backfill --count` into the allowed `1..=BACKFILL_MAX_COUNT`
/// range.
pub fn clamp_backfill_count(n: usize) -> usize {
    n.clamp(1, BACKFILL_MAX_COUNT)
}

#[derive(Parser, Debug)]
#[command(
    name = "telegram-relay",
    about = "Fast Telegram → Discord webhook relay (MTProto userbot)",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the relay daemon.
    Run {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Interactive first-time login (phone → code → optional 2FA).
    Login {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// List all dialogs (chats) to discover chat ids.
    Chats {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Validate config + webhooks (+ routes if a session exists); exit 0/1.
    Check {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Print store counters + relay latency percentiles (no network). Exit 0.
    Stats {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Print an ASCII diagram of the routing wiring (source → webhooks) from
    /// config alone — no Telegram session needed. Shows fan-in and fan-out.
    Routes {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Relay the most recent N messages from one route's chat, on demand.
    ///
    /// Fetches via the same live pipeline (filter, embed, post, record) so the
    /// refresh worker picks the posts up on its next tick. Media messages relay
    /// caption/text + deep link only (no download/re-upload).
    Backfill {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Name of the route to backfill (must exist in config.yaml).
        route: String,
        /// Number of most-recent messages to relay (default 3, max 25).
        #[arg(long, default_value_t = BACKFILL_DEFAULT_COUNT)]
        count: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_backfill_count_passes_through_in_range() {
        assert_eq!(clamp_backfill_count(1), 1);
        assert_eq!(clamp_backfill_count(3), 3);
        assert_eq!(clamp_backfill_count(BACKFILL_MAX_COUNT), BACKFILL_MAX_COUNT);
    }

    #[test]
    fn clamp_backfill_count_caps_at_max() {
        assert_eq!(clamp_backfill_count(26), BACKFILL_MAX_COUNT);
        assert_eq!(clamp_backfill_count(1_000_000), BACKFILL_MAX_COUNT);
    }

    #[test]
    fn clamp_backfill_count_floors_at_one() {
        assert_eq!(clamp_backfill_count(0), 1);
    }
}
