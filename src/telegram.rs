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

/// A single resolved chat plus the [`PeerRef`](grammers_client::session::types::PeerRef)
/// needed to fetch its message history (`iter_messages`).
pub struct ResolvedChat {
    pub chat: ChatId,
    pub title: String,
    pub peer: grammers_client::session::types::PeerRef,
}

/// Resolve one route's `ChatRef` to a concrete chat + `PeerRef`, for the
/// `backfill` CLI verb.
///
/// Unlike [`resolve_routes`] (which only captures a `PeerRef` for
/// `ChatRef::Username` routes — numeric-id routes still relay live updates
/// fine without one), `backfill` needs a `PeerRef` unconditionally because
/// `Client::iter_messages` requires one. For `ChatRef::Id` routes that means
/// falling back to a dialog scan (mirroring [`list_chats`]) to find the
/// matching peer and derive its ref from there.
pub async fn resolve_chat_peer(
    client: &Client,
    chat_ref: &ChatRef,
) -> anyhow::Result<ResolvedChat> {
    match chat_ref {
        ChatRef::Username(username) => {
            let peer = client
                .resolve_username(username)
                .await
                .with_context(|| format!("resolving @{username}"))?
                .ok_or_else(|| anyhow!("username '@{username}' not found"))?;
            let id = peer.id().bot_api_dialog_id_unchecked();
            let title = peer.name().unwrap_or(username).to_string();
            let pref = peer
                .to_ref()
                .await
                .map_err(|e| anyhow!("resolving peer ref for @{username}: {e}"))?
                .ok_or_else(|| anyhow!("no peer ref available for @{username} (never seen it?)"))?;
            Ok(ResolvedChat {
                chat: ChatId(id),
                title,
                peer: pref,
            })
        }
        ChatRef::Id(id) => {
            let mut dialogs = client.iter_dialogs();
            while let Some(dialog) = dialogs.next().await? {
                let peer = dialog.peer();
                if peer.id().bot_api_dialog_id_unchecked() == id.0 {
                    let title = peer.name().unwrap_or("(no title)").to_string();
                    let pref = peer
                        .to_ref()
                        .await
                        .map_err(|e| anyhow!("resolving peer ref for chat {}: {e}", id.0))?
                        .ok_or_else(|| anyhow!("no peer ref available for chat {}", id.0))?;
                    return Ok(ResolvedChat {
                        chat: *id,
                        title,
                        peer: pref,
                    });
                }
            }
            Err(anyhow!(
                "chat id {} not found among dialogs (never seen it, or account lost access?)",
                id.0
            ))
        }
    }
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
        /// Channel/chat title, for the embed author (same as the text path).
        title: Option<String>,
    },
}

/// Whether a message carries anything worth relaying.
///
/// Telegram service messages (a "pinned a message" / "joined" action) and empty
/// messages report an empty `text()` and no `media()`. Relaying them produces a
/// blank Discord embed, so both the live path and backfill skip anything with
/// neither text nor media. A caption-less video still has media, so it relays.
pub fn has_relayable_content(text: &str, has_media: bool) -> bool {
    has_media || !text.trim().is_empty()
}

/// A human-readable notice describing non-file media (a poll, a location, a
/// contact, …) that has no downloadable file. Relaying the notice as text keeps
/// the post instead of dropping it. The rendering is pure (no grammers types),
/// so it is unit-tested directly; the [`Media`] → notice mapping is
/// [`classify_media`].
#[derive(Debug, Clone, PartialEq)]
pub enum MediaNotice {
    Poll {
        question: String,
        options: Vec<String>,
    },
    Location {
        lat: f64,
        lng: f64,
    },
    LiveLocation {
        coords: Option<(f64, f64)>,
    },
    Venue {
        title: String,
        address: String,
    },
    Contact {
        name: String,
        phone: String,
    },
    Dice {
        emoji: String,
        value: i32,
    },
    /// Media Telegram sent that grammers could not parse into a known [`Media`]
    /// variant (paid media, games, giveaways, stories, or any future type).
    Unsupported,
}

impl MediaNotice {
    /// Render the notice into the body used for the relayed embed.
    pub fn render(&self) -> String {
        match self {
            MediaNotice::Poll { question, options } => {
                let mut s = format!("[poll] {question}");
                for opt in options {
                    s.push_str(&format!("\n• {opt}"));
                }
                s
            }
            MediaNotice::Location { lat, lng } => format!("[location] {lat}, {lng}"),
            MediaNotice::LiveLocation { coords } => match coords {
                Some((lat, lng)) => format!("[live location] {lat}, {lng}"),
                None => "[live location]".to_string(),
            },
            MediaNotice::Venue { title, address } => {
                if address.trim().is_empty() {
                    format!("[venue] {title}")
                } else {
                    format!("[venue] {title} — {address}")
                }
            }
            MediaNotice::Contact { name, phone } => {
                let name = name.trim();
                match (name.is_empty(), phone.trim().is_empty()) {
                    (false, false) => format!("[contact] {name} — {phone}"),
                    (false, true) => format!("[contact] {name}"),
                    (true, false) => format!("[contact] {phone}"),
                    (true, true) => "[contact]".to_string(),
                }
            }
            MediaNotice::Dice { emoji, value } => format!("[dice] {emoji} {value}"),
            MediaNotice::Unsupported => "[unsupported media]".to_string(),
        }
    }
}

/// How a message's *parsed* media should be relayed.
#[derive(Debug)]
enum MediaClass {
    /// Downloadable file (photo, document, sticker): download + re-upload.
    File,
    /// Relay as text. `None` = a link preview (`WebPage`): the URL already lives
    /// in the message text and Discord renders its own preview. `Some(notice)` =
    /// non-file media described by the notice.
    AsText(Option<MediaNotice>),
}

/// Classify a parsed [`Media`] value into how it should be relayed.
///
/// Photo/Document/Sticker are downloadable files. `WebPage` is a link preview
/// (relayed as its own text). Everything else (poll, geo, venue, contact, dice,
/// live location, and any future `#[non_exhaustive]` variant) becomes a text
/// notice rather than being pushed through a download that is guaranteed to fail.
fn classify_media(media: &Media) -> MediaClass {
    match media {
        Media::Photo(_) | Media::Document(_) | Media::Sticker(_) => MediaClass::File,
        Media::WebPage(_) => MediaClass::AsText(None),
        Media::Poll(p) => MediaClass::AsText(Some(poll_notice(p))),
        Media::Geo(g) => MediaClass::AsText(Some(MediaNotice::Location {
            lat: g.latitue(),
            lng: g.longitude(),
        })),
        Media::GeoLive(gl) => MediaClass::AsText(Some(MediaNotice::LiveLocation {
            coords: gl.geo.as_ref().map(|g| (g.latitue(), g.longitude())),
        })),
        Media::Venue(v) => MediaClass::AsText(Some(MediaNotice::Venue {
            title: v.title().to_string(),
            address: v.address().to_string(),
        })),
        Media::Contact(c) => MediaClass::AsText(Some(MediaNotice::Contact {
            name: format!("{} {}", c.first_name(), c.last_name())
                .trim()
                .to_string(),
            phone: c.phone_number().to_string(),
        })),
        Media::Dice(d) => MediaClass::AsText(Some(MediaNotice::Dice {
            emoji: d.emoji().to_string(),
            value: d.value(),
        })),
        // `Media` is #[non_exhaustive]; a future variant relays as a notice
        // rather than being silently dropped.
        _ => MediaClass::AsText(Some(MediaNotice::Unsupported)),
    }
}

/// Extract a poll's question + answer options into a [`MediaNotice::Poll`].
fn poll_notice(poll: &grammers_client::media::Poll) -> MediaNotice {
    use grammers_client::tl::enums::TextWithEntities;
    let question = match poll.question() {
        TextWithEntities::Entities(t) => t.text.clone(),
    };
    let options = poll
        .iter_answers()
        .map(|a| match a.text() {
            TextWithEntities::Entities(t) => t.text,
        })
        .collect();
    MediaNotice::Poll { question, options }
}

/// Whether the message carries media in its raw TL payload — including media
/// grammers could not parse into a [`Media`] variant (paid media, giveaways,
/// stories, …), where `message.media()` is `None` yet content still exists.
///
/// Note: this reads the *inner message's* raw (`message::Message::raw`, a
/// `tl::enums::Message`), reached by Deref coercion from `update::Message` whose
/// own `raw` field is a `tl::enums::Update` — a different type.
fn raw_has_media(msg: &grammers_client::message::Message) -> bool {
    use grammers_client::tl::enums::Message as TlMessage;
    matches!(&msg.raw, TlMessage::Message(m) if m.media.is_some())
}

/// How a whole message should be relayed. Shared by the live [`classify`] path
/// and the `backfill` CLI so both make the same skip/text/file decision.
#[derive(Debug, PartialEq)]
pub enum Routing {
    /// Nothing worth relaying (service/empty message).
    Skip,
    /// Relay as a text embed with this ready-to-render body.
    Text(String),
    /// Relay as downloadable media; the caller downloads `message.media()`.
    File,
}

/// Decide how to relay a message: skip, text (with a ready body), or file.
///
/// This is the single source of truth for the classification: a link preview
/// relays as its text (Discord renders the preview), non-file media relays as a
/// notice, unparsed raw media relays as an `[unsupported media]` notice, and
/// service/empty messages are skipped — none are dropped with an empty body.
pub fn route_message(msg: &grammers_client::message::Message) -> Routing {
    let caption = msg.text();
    let parsed = msg.media();
    if !has_relayable_content(caption, parsed.is_some() || raw_has_media(msg)) {
        return Routing::Skip;
    }
    match parsed {
        Some(media) => match classify_media(&media) {
            MediaClass::File => Routing::File,
            MediaClass::AsText(None) => Routing::Text(caption.to_string()),
            MediaClass::AsText(Some(notice)) => {
                Routing::Text(combine_body(caption, &notice.render()))
            }
        },
        None => {
            if raw_has_media(msg) {
                Routing::Text(combine_body(caption, &MediaNotice::Unsupported.render()))
            } else {
                Routing::Text(caption.to_string())
            }
        }
    }
}

/// Join a caption with a media notice, dropping the separator when the caption
/// is empty so a notice never renders with a leading blank line.
fn combine_body(caption: &str, notice: &str) -> String {
    if caption.trim().is_empty() {
        notice.to_string()
    } else {
        format!("{caption}\n{notice}")
    }
}

/// Classify a raw update into an [`Incoming`], or `None` for events we ignore.
///
/// `NewMessage`/`MessageEdited` produce an `Incoming`; everything else is `None`.
/// A message carrying media becomes `Incoming::Media`, otherwise `Incoming::Text`.
/// Service/empty messages (e.g. a pin action) carry no relayable content and are
/// dropped here so they never reach Discord as blank embeds.
pub fn classify(update: Update) -> Option<Incoming> {
    let (message, edited) = match update {
        Update::NewMessage(m) => (m, false),
        Update::MessageEdited(m) => (m, true),
        _ => return None,
    };

    let routing = route_message(&message);
    if matches!(routing, Routing::Skip) {
        return None;
    }

    let chat = ChatId(message.peer_id().bot_api_dialog_id_unchecked());
    let msg_id = message.id();
    let sender = message
        .sender()
        .and_then(|p| p.name())
        .map(|s| s.to_string());
    let title = message.peer().and_then(|p| p.name()).map(|s| s.to_string());
    // Public username -> t.me deep link.
    let deep_link = message
        .peer()
        .and_then(|p| p.username())
        .map(|u| format!("https://t.me/{u}/{msg_id}"));

    match routing {
        // Already handled above; kept exhaustive for the compiler.
        Routing::Skip => None,
        Routing::Text(body) => Some(Incoming::Text {
            chat,
            msg_id,
            sender,
            body,
            // Not yet populated: resolving the quoted body requires a follow-up
            // fetch. Field kept for forward compatibility.
            reply_quote: None,
            edited,
            title,
            deep_link,
        }),
        Routing::File => {
            let media = message
                .media()
                .expect("File routing implies message.media() is Some");
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
                title,
            })
        }
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

#[cfg(test)]
mod tests {
    use super::has_relayable_content;

    #[test]
    fn text_only_message_relays() {
        assert!(has_relayable_content("gm", false));
    }

    #[test]
    fn media_with_caption_relays() {
        assert!(has_relayable_content("nice video", true));
    }

    #[test]
    fn captionless_media_still_relays() {
        assert!(has_relayable_content("", true));
    }

    #[test]
    fn service_or_empty_message_is_skipped() {
        // A pin/join service message: empty text, no media.
        assert!(!has_relayable_content("", false));
    }

    #[test]
    fn whitespace_only_no_media_is_skipped() {
        assert!(!has_relayable_content("   \n  ", false));
    }
}

#[cfg(test)]
mod classify_tests {
    use super::{classify_media, combine_body, MediaClass, MediaNotice};
    use grammers_client::media::{Contact, Dice, Geo, GeoLive, Media, Venue, WebPage};
    use grammers_client::tl::{enums, types};

    fn notice_of(m: &Media) -> MediaNotice {
        match classify_media(m) {
            MediaClass::AsText(Some(n)) => n,
            other => panic!("expected a non-file text notice, got {other:?}"),
        }
    }

    #[test]
    fn media_notice_render_covers_each_variant() {
        assert_eq!(
            MediaNotice::Poll {
                question: "Best chain?".into(),
                options: vec!["ETH".into(), "SOL".into()],
            }
            .render(),
            "[poll] Best chain?\n• ETH\n• SOL"
        );
        assert_eq!(
            MediaNotice::Location {
                lat: 48.8,
                lng: 2.3
            }
            .render(),
            "[location] 48.8, 2.3"
        );
        assert_eq!(
            MediaNotice::LiveLocation {
                coords: Some((1.0, 2.0))
            }
            .render(),
            "[live location] 1, 2"
        );
        assert_eq!(
            MediaNotice::LiveLocation { coords: None }.render(),
            "[live location]"
        );
        assert_eq!(
            MediaNotice::Venue {
                title: "Blue Bottle".into(),
                address: "1 Main St".into(),
            }
            .render(),
            "[venue] Blue Bottle — 1 Main St"
        );
        assert_eq!(
            MediaNotice::Contact {
                name: "Rob".into(),
                phone: "+1555".into(),
            }
            .render(),
            "[contact] Rob — +1555"
        );
        assert_eq!(
            MediaNotice::Dice {
                emoji: "🎯".into(),
                value: 5,
            }
            .render(),
            "[dice] 🎯 5"
        );
        assert_eq!(MediaNotice::Unsupported.render(), "[unsupported media]");
    }

    #[test]
    fn combine_body_joins_and_handles_empty_caption() {
        assert_eq!(combine_body("", "[poll] Q"), "[poll] Q");
        assert_eq!(combine_body("   \n ", "[poll] Q"), "[poll] Q");
        assert_eq!(
            combine_body("look at this", "[poll] Q"),
            "look at this\n[poll] Q"
        );
    }

    #[test]
    fn webpage_routes_to_text_not_file() {
        // The CRITICAL bug: a link post is Media::WebPage. It must relay as text
        // (Discord renders its own preview), never as a download that fails and
        // drops the post.
        let wp = Media::WebPage(WebPage::from_raw_media(types::MessageMediaWebPage {
            force_large_media: false,
            force_small_media: false,
            manual: false,
            safe: false,
            webpage: enums::WebPage::Empty(types::WebPageEmpty { id: 0, url: None }),
        }));
        assert!(matches!(classify_media(&wp), MediaClass::AsText(None)));
    }

    #[test]
    fn photo_and_document_are_files() {
        // Sanity: downloadable media still classifies as File (unchanged path).
        // (Photo/Document need heavy raw construction; the WebPage + notice tests
        // above lock the *new* branches, and the enum's File arm is exercised by
        // route_message + the live/backfill media paths.)
        let dice = Media::Dice(Dice::from_raw_media(types::MessageMediaDice {
            value: 1,
            emoticon: "🎲".into(),
            game_outcome: None,
        }));
        assert!(!matches!(classify_media(&dice), MediaClass::File));
    }

    #[test]
    fn dice_maps_to_dice_notice() {
        let d = Media::Dice(Dice::from_raw_media(types::MessageMediaDice {
            value: 6,
            emoticon: "🎲".into(),
            game_outcome: None,
        }));
        assert_eq!(
            notice_of(&d),
            MediaNotice::Dice {
                emoji: "🎲".into(),
                value: 6
            }
        );
    }

    #[test]
    fn contact_maps_to_contact_notice() {
        let c = Media::Contact(Contact::from_raw_media(types::MessageMediaContact {
            phone_number: "+15551234".into(),
            first_name: "Rob".into(),
            last_name: "T".into(),
            vcard: String::new(),
            user_id: 0,
        }));
        assert_eq!(
            notice_of(&c),
            MediaNotice::Contact {
                name: "Rob T".into(),
                phone: "+15551234".into()
            }
        );
    }

    #[test]
    fn venue_maps_to_venue_notice() {
        let v = Media::Venue(Venue::from_raw_media(types::MessageMediaVenue {
            geo: enums::GeoPoint::Empty,
            title: "Blue Bottle".into(),
            address: "1 Main St".into(),
            provider: String::new(),
            venue_id: String::new(),
            venue_type: String::new(),
        }));
        assert_eq!(
            notice_of(&v),
            MediaNotice::Venue {
                title: "Blue Bottle".into(),
                address: "1 Main St".into()
            }
        );
    }

    #[test]
    fn geo_maps_to_location() {
        let geo = Geo::from_raw_media(types::MessageMediaGeo {
            geo: enums::GeoPoint::Point(types::GeoPoint {
                long: 2.3,
                lat: 48.8,
                access_hash: 0,
                accuracy_radius: None,
            }),
        })
        .expect("point geo");
        assert_eq!(
            notice_of(&Media::Geo(geo)),
            MediaNotice::Location {
                lat: 48.8,
                lng: 2.3
            }
        );
    }

    #[test]
    fn geolive_maps_to_live_location() {
        let gl = GeoLive::from_raw_media(types::MessageMediaGeoLive {
            geo: enums::GeoPoint::Point(types::GeoPoint {
                long: 5.0,
                lat: 10.0,
                access_hash: 0,
                accuracy_radius: None,
            }),
            heading: None,
            period: 60,
            proximity_notification_radius: None,
        });
        assert_eq!(
            notice_of(&Media::GeoLive(gl)),
            MediaNotice::LiveLocation {
                coords: Some((10.0, 5.0))
            }
        );
    }
}
