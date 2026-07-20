//! Telegram (grammers-client 0.10.0) integration: connect, interactive login,
//! route resolution, dialog listing, update classification, and catch-up.
//!
//! grammers 0.10.0 has a materially different shape from the older API the
//! plan pseudocode assumed (no `Client::connect`, no `Session::load_file_or_create`,
//! no bare `next_update()` loop). This module is written against the real 0.10.0
//! API verified against the crate source; see
//! `docs/superpowers/plans/api-notes.md` (Task 6 corrections) for the deltas.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use grammers_client::client::{ClientConfiguration, UpdatesConfiguration};
use grammers_client::media::Media;
use grammers_client::sender::SenderPoolFatHandle;
use grammers_client::session::updates::UpdatesLike;
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool, SignInError};
use grammers_session::storages::SqliteSession;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tracing::info;

use crate::config::{ChatId, ChatRef, Config};
use crate::router::ResolvedRoute;

/// A live connection to Telegram.
///
/// In grammers 0.10.0 the `Client` and the stream of incoming updates are
/// separate objects driven by a background `SenderPool` runner, so `connect`
/// hands back all three (the plan's `connect() -> Client` was written against
/// the old API where the client owned everything).
pub struct Connection {
    /// The high-level client used for all API calls.
    pub client: Client,
    /// Receiver of raw pushed updates; feed into [`stream_updates`].
    pub updates: UnboundedReceiver<UpdatesLike>,
    /// Handle to the sender pool; call `handle.quit()` for graceful shutdown.
    pub handle: SenderPoolFatHandle,
    /// Background IO task; `await` it after `quit()` to flush cleanly.
    pub pool_task: JoinHandle<()>,
}

/// Open (or create) the session and bring up the connection.
///
/// The session is a SQLite file (`session_path`) that persists the auth key,
/// cached peers and update state automatically — so catch-up after downtime is
/// handled by the library ([`stream_updates`] with `catch_up: true`); no
/// separate `state.json` is needed. `api_hash` is not required here; it is only
/// used during [`interactive_login`].
pub async fn connect(api_id: i32, session_path: &Path) -> anyhow::Result<Connection> {
    let session = SqliteSession::open(session_path)
        .await
        .map_err(|e| anyhow!("open session {}: {e}", session_path.display()))?;
    restrict_session_permissions(session_path);
    let session = Arc::new(session);

    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::new(Arc::clone(&session), api_id);

    let client = Client::with_configuration(handle.clone(), ClientConfiguration::default());
    let pool_task = tokio::spawn(runner.run());

    Ok(Connection {
        client,
        updates,
        handle,
        pool_task,
    })
}

/// Build the update stream with catch-up enabled.
///
/// Consumes the `updates` receiver produced by [`connect`]. `catch_up: true`
/// asks Telegram to replay updates missed while the client was offline, which
/// satisfies the plan's "catch up on missed messages after downtime" requirement.
pub async fn stream_updates(
    client: &Client,
    updates: UnboundedReceiver<UpdatesLike>,
) -> anyhow::Result<grammers_client::client::UpdateStream> {
    client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow!("failed to start update stream: {e}"))
}

/// Interactive first-time login: phone -> code -> (optional) 2FA password.
///
/// No-op if the session is already authorized. Session state persists to the
/// SQLite file automatically once sign-in completes.
pub async fn interactive_login(client: &Client, api_hash: &str) -> anyhow::Result<()> {
    if client.is_authorized().await? {
        info!("already authorized; skipping login");
        return Ok(());
    }

    let phone = prompt("Enter phone number (international format, e.g. +14155551234): ")?;
    let token = client
        .request_login_code(phone.trim(), api_hash)
        .await
        .context("requesting login code")?;

    let code = prompt("Enter the login code you received: ")?;
    match client.sign_in(&token, code.trim()).await {
        Ok(_user) => {}
        Err(SignInError::PasswordRequired(password_token)) => {
            if let Some(hint) = password_token.hint() {
                println!("Two-factor authentication enabled (hint: {hint}).");
            }
            let password =
                rpassword::prompt_password("2FA password: ").context("reading 2FA password")?;
            client
                .check_password(password_token, password.trim())
                .await
                .context("checking 2FA password")?;
        }
        Err(e) => return Err(anyhow!("sign-in failed: {e}")),
    }

    info!("login successful; session persisted");
    Ok(())
}

/// Route resolution result: the resolved routes plus a map of chat id ->
/// [`PeerRef`] for chats we resolved a full peer for (used by the refresh worker
/// to re-fetch messages by id, which needs the peer's access hash).
pub struct Resolution {
    pub routes: Vec<ResolvedRoute>,
    pub peers: HashMap<ChatId, grammers_client::session::types::PeerRef>,
}

/// Resolve each configured route's `from` into a concrete [`ChatId`].
///
/// `ChatRef::Username` is resolved via `resolve_username` (and its `PeerRef` is
/// captured for refresh); `ChatRef::Id` is used as-is (no `PeerRef`, so numeric
/// routes are not refreshable — they still relay live). Logs
/// `route '{name}' watching '{title}' ({id})` per route.
pub async fn resolve_routes(client: &Client, cfg: &Config) -> anyhow::Result<Resolution> {
    let mut resolved = Vec::with_capacity(cfg.routes.len());
    let mut peers = HashMap::new();
    for route in &cfg.routes {
        let (chat, title) = match &route.from {
            ChatRef::Username(username) => {
                let peer = client
                    .resolve_username(username)
                    .await
                    .with_context(|| format!("resolving @{username}"))?
                    .ok_or_else(|| {
                        anyhow!("route '{}': username '@{username}' not found", route.name)
                    })?;
                let id = peer.id().bot_api_dialog_id_unchecked();
                let title = peer.name().unwrap_or(username).to_string();
                if let Ok(Some(pref)) = peer.to_ref().await {
                    peers.insert(ChatId(id), pref);
                }
                (ChatId(id), title)
            }
            ChatRef::Id(id) => (*id, format!("id:{}", id.0)),
        };
        info!("route '{}' watching '{}' ({})", route.name, title, chat.0);
        resolved.push(ResolvedRoute {
            name: route.name.clone(),
            chat,
            to: route.to.clone(),
            filter: route.filter.clone(),
        });
    }
    Ok(Resolution {
        routes: resolved,
        peers,
    })
}

/// Print an `id  type  title` table of all dialogs (the `chats` CLI verb).
pub async fn list_chats(client: &Client) -> anyhow::Result<()> {
    let mut dialogs = client.iter_dialogs();
    println!("{:>14}  {:8}  title", "id", "type");
    while let Some(dialog) = dialogs.next().await? {
        let peer = dialog.peer();
        let id = peer.id().bot_api_dialog_id_unchecked();
        let kind = peer_kind(peer);
        let title = peer.name().unwrap_or("(no title)");
        println!("{id:>14}  {kind:8}  {title}");
    }
    Ok(())
}

/// A relay-relevant incoming event, distilled from a raw [`Update`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Incoming {
    Text {
        chat: ChatId,
        msg_id: i32,
        sender: Option<String>,
        body: String,
        reply_quote: Option<String>,
        edited: bool,
        /// Channel/chat title, for the embed author.
        title: Option<String>,
        /// `t.me` deep link to this message, if the chat has a public username.
        deep_link: Option<String>,
    },
    Media {
        chat: ChatId,
        msg_id: i32,
        grouped_id: Option<i64>,
        media: Media,
        caption: String,
        sender: Option<String>,
        approx_size: u64,
        deep_link: Option<String>,
    },
}

/// Classify a raw update into an [`Incoming`], or `None` for events we ignore.
///
/// `NewMessage`/`MessageEdited` produce an `Incoming`; everything else is `None`.
/// A message carrying media becomes `Incoming::Media`, otherwise `Incoming::Text`.
pub fn classify(update: Update) -> Option<Incoming> {
    let (message, edited) = match update {
        Update::NewMessage(m) => (m, false),
        Update::MessageEdited(m) => (m, true),
        _ => return None,
    };

    let chat = ChatId(message.peer_id().bot_api_dialog_id_unchecked());
    let msg_id = message.id();
    let sender = message
        .sender()
        .and_then(|p| p.name())
        .map(|s| s.to_string());

    if let Some(media) = message.media() {
        // Public username -> t.me deep link.
        let deep_link = message
            .peer()
            .and_then(|p| p.username())
            .map(|u| format!("https://t.me/{u}/{msg_id}"));
        let approx_size = media.size().unwrap_or(0) as u64;
        Some(Incoming::Media {
            chat,
            msg_id,
            grouped_id: message.grouped_id(),
            media,
            caption: message.text().to_string(),
            sender,
            approx_size,
            deep_link,
        })
    } else {
        let title = message.peer().and_then(|p| p.name()).map(|s| s.to_string());
        let deep_link = message
            .peer()
            .and_then(|p| p.username())
            .map(|u| format!("https://t.me/{u}/{msg_id}"));
        Some(Incoming::Text {
            chat,
            msg_id,
            sender,
            body: message.text().to_string(),
            // The quoted body of a reply is not carried inline by Telegram; it
            // requires a follow-up fetch (handled in the enrichment lane).
            reply_quote: None,
            edited,
            title,
            deep_link,
        })
    }
}

fn peer_kind(peer: &grammers_client::peer::Peer) -> &'static str {
    use grammers_client::peer::Peer;
    match peer {
        Peer::User(_) => "user",
        Peer::Group(_) => "group",
        Peer::Channel(_) => "channel",
    }
}

/// Restrict the session file (and its `-wal`/`-shm` siblings, if present) to
/// owner-only permissions (0600). The session carries an auth key, so it must
/// never be group/world readable. Missing sidecar files (WAL not yet created,
/// or already checkpointed away) are not an error.
#[cfg(unix)]
fn restrict_session_permissions(session_path: &Path) {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    for suffix in ["", "-wal", "-shm"] {
        let path = if suffix.is_empty() {
            session_path.to_path_buf()
        } else {
            let mut s = session_path.as_os_str().to_owned();
            s.push(suffix);
            std::path::PathBuf::from(s)
        };
        match fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("failed to restrict permissions on {}: {e}", path.display());
            }
        }
    }
}

#[cfg(not(unix))]
fn restrict_session_permissions(_session_path: &Path) {}

fn prompt(msg: &str) -> anyhow::Result<String> {
    print!("{msg}");
    io::stdout().flush().context("flushing prompt")?;
    let mut line = String::new();
    io::stdin().read_line(&mut line).context("reading stdin")?;
    Ok(line)
}
