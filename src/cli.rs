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
}
