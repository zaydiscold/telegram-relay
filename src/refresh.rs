//! Refresh worker: keep already-posted Discord embeds in sync with Telegram.
//!
//! On a fixed interval it re-fetches every tracked post within the horizon,
//! diffs it against the stored snapshot, and PATCHes the Discord embed when the
//! source changed:
//!
//! * **deleted** on Telegram  → mark deleted + PATCH a `🗑 deleted on Telegram`
//!   notice (the original body/title are not stored, so the notice carries only
//!   the last-known stats — see the store schema).
//! * **edited** (body hash changed) → PATCH the new body with an `(edited)` mark.
//! * **reactions / comment count changed** → PATCH the refreshed stats line.
//!
//! The diff logic is a pure function ([`diff`]) and fetching is behind the
//! [`PostFetcher`] trait, so both are unit-testable with fakes and no network.
//!
//! ## What grammers 0.10.0 actually exposes (verified against crate source)
//! * per-emoji reactions: `Message::raw` → `MessageReactions::Reactions.results`
//!   (`ReactionCount { reaction, count }`); `Reaction::Emoji(e).emoticon` is the
//!   unicode key. `Paid` → `⭐`, `CustomEmoji` → `🎨` (no unicode available),
//!   `Empty` skipped.
//! * comment/discussion count: `Message::reply_count() -> Option<i32>`.
//! * edit marker: body-hash comparison (also `Message::edit_date()` is available).
//!
//! No reaction/comment data had to be stubbed — it is all reachable.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

use crate::config::{ChatId, RefreshCfg, WebhookName, WebhookUrl};
use crate::deliver::{Deliverer, Outcome};
use crate::render::{self, EmbedMeta, RelayText};
use crate::store::{Store, TrackedMsg};

/// Max message ids per `get_messages_by_id` call (Telegram limit).
const BATCH_MAX: usize = 100;

/// A re-fetched Telegram post, distilled to what the embed + diff need.
#[derive(Debug, Clone, Default)]
pub struct FetchedPost {
    pub title: String,
    pub body: String,
    pub deep_link: Option<String>,
    pub reactions: BTreeMap<String, i32>,
    pub comment_count: i32,
}

/// Fetch tracked messages by id for a chat. Behind a trait so the diff/apply
/// loop can be tested with a fake that returns canned posts.
#[allow(async_fn_in_trait)]
pub trait PostFetcher {
    /// Return one slot per input id, aligned by index; `None` = not found
    /// (deleted or inaccessible).
    async fn fetch(&self, chat: ChatId, ids: &[i32]) -> Result<Vec<Option<FetchedPost>>>;
}

/// The real [`PostFetcher`], backed by a grammers [`Client`] and a map of
/// bot-API chat id -> [`PeerRef`] (built at route resolution, since fetching by
/// id requires the peer's access hash which only `PeerRef` carries).
///
/// Chats without a cached `PeerRef` (e.g. numeric-id routes we never resolved a
/// full peer for) cannot be fetched and are reported as an error for that chat.
pub struct GrammersFetcher {
    client: grammers_client::Client,
    peers: HashMap<i64, grammers_client::session::types::PeerRef>,
}

impl GrammersFetcher {
    pub fn new(
        client: grammers_client::Client,
        peers: HashMap<i64, grammers_client::session::types::PeerRef>,
    ) -> Self {
        GrammersFetcher { client, peers }
    }
}

/// Distil a fetched grammers message into a [`FetchedPost`].
fn to_fetched(msg: &grammers_client::message::Message) -> FetchedPost {
    let title = msg
        .peer()
        .and_then(|p| p.name())
        .unwrap_or_default()
        .to_string();
    let deep_link = msg
        .peer()
        .and_then(|p| p.username())
        .map(|u| format!("https://t.me/{u}/{}", msg.id()));
    FetchedPost {
        title,
        body: msg.text().to_string(),
        deep_link,
        reactions: extract_reactions(msg),
        comment_count: msg.reply_count().unwrap_or(0),
    }
}

/// Extract per-emoji reaction counts from a message's raw TL payload.
///
/// grammers 0.10.0 exposes `Message::raw`; reactions live in
/// `MessageReactions::Reactions.results`. Custom emoji and paid ("star")
/// reactions have no unicode emoticon, so they get generic markers.
fn extract_reactions(msg: &grammers_client::message::Message) -> BTreeMap<String, i32> {
    use grammers_client::tl::enums::{
        Message as TlMessage, MessageReactions, Reaction, ReactionCount,
    };
    let mut out: BTreeMap<String, i32> = BTreeMap::new();
    if let TlMessage::Message(m) = &msg.raw {
        if let Some(MessageReactions::Reactions(r)) = &m.reactions {
            for rc in &r.results {
                let ReactionCount::Count(c) = rc;
                let key = match &c.reaction {
                    Reaction::Emoji(e) => e.emoticon.clone(),
                    Reaction::Paid => "⭐".to_string(),
                    Reaction::CustomEmoji(_) => "🎨".to_string(),
                    Reaction::Empty => continue,
                };
                *out.entry(key).or_insert(0) += c.count;
            }
        }
    }
    out
}

impl PostFetcher for GrammersFetcher {
    async fn fetch(&self, chat: ChatId, ids: &[i32]) -> Result<Vec<Option<FetchedPost>>> {
        let Some(peer) = self.peers.get(&chat.0).copied() else {
            anyhow::bail!("no cached peer ref for chat {}", chat.0);
        };
        let msgs = self
            .client
            .get_messages_by_id(peer, ids)
            .await
            .map_err(|e| anyhow::anyhow!("get_messages_by_id({}): {e}", chat.0))?;
        Ok(msgs.iter().map(|m| m.as_ref().map(to_fetched)).collect())
    }
}

/// The change detected between a stored row and its re-fetched source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshAction {
    None,
    Deleted,
    Edited,
    StatsChanged,
}

/// Stable content hash of a message body (FNV-1a, 64-bit, hex).
///
/// Deterministic across runs (unlike `DefaultHasher`), so it can be persisted
/// and compared across restarts.
pub fn content_hash(body: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in body.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Pure diff: what (if anything) needs PATCHing for one tracked message.
pub fn diff(tracked: &TrackedMsg, fetched: Option<&FetchedPost>) -> RefreshAction {
    match fetched {
        None => RefreshAction::Deleted,
        Some(f) => {
            if content_hash(&f.body) != tracked.content_hash {
                RefreshAction::Edited
            } else if f.reactions != tracked.reactions || f.comment_count != tracked.comment_count {
                RefreshAction::StatsChanged
            } else {
                RefreshAction::None
            }
        }
    }
}

/// The interval worker. Runs until the process ends.
pub async fn run<F: PostFetcher>(
    fetcher: F,
    store: Arc<Store>,
    deliverer: Arc<Deliverer>,
    webhooks: HashMap<WebhookName, WebhookUrl>,
    cfg: RefreshCfg,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.interval_mins * 60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; skip it so we don't refresh before anything
    // has been posted.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if let Err(e) = tick_once(&fetcher, &store, &deliverer, &webhooks, &cfg).await {
            warn!(error = %e, "refresh tick failed");
        }
    }
}

/// A single refresh pass: fetch due posts per chat, apply diffs, then prune.
pub async fn tick_once<F: PostFetcher>(
    fetcher: &F,
    store: &Arc<Store>,
    deliverer: &Arc<Deliverer>,
    webhooks: &HashMap<WebhookName, WebhookUrl>,
    cfg: &RefreshCfg,
) -> Result<()> {
    let due = store.due(cfg.horizon_hours)?;
    if due.is_empty() {
        return Ok(());
    }

    // Group tracked rows by chat.
    let mut by_chat: HashMap<i64, Vec<TrackedMsg>> = HashMap::new();
    for t in due {
        by_chat.entry(t.chat_id).or_default().push(t);
    }

    for (chat_id, rows) in by_chat {
        // Unique tg ids for this chat, batched at BATCH_MAX.
        let mut ids: Vec<i32> = rows.iter().map(|r| r.tg_msg_id).collect();
        ids.sort_unstable();
        ids.dedup();

        let mut fetched: HashMap<i32, Option<FetchedPost>> = HashMap::new();
        for batch in ids.chunks(BATCH_MAX) {
            match fetcher.fetch(ChatId(chat_id), batch).await {
                Ok(posts) => {
                    for (id, post) in batch.iter().zip(posts) {
                        fetched.insert(*id, post);
                    }
                }
                Err(e) => {
                    warn!(chat_id, error = %e, "refresh fetch failed; skipping chat batch");
                }
            }
        }
        if fetched.is_empty() {
            continue; // fetch failed entirely for this chat
        }

        for row in rows {
            // If the id wasn't in any successful batch, don't guess it's deleted.
            let Some(slot) = fetched.get(&row.tg_msg_id) else {
                continue;
            };
            let action = diff(&row, slot.as_ref());
            if let Err(e) = apply(&action, &row, slot.as_ref(), store, deliverer, webhooks).await {
                warn!(chat_id, tg = row.tg_msg_id, error = %e, "applying refresh failed");
            }
        }
    }

    let pruned = store.prune(cfg.horizon_hours)?;
    if pruned > 0 {
        info!(pruned, "refresh pruned expired rows");
    }
    Ok(())
}

/// Apply a single diff result: PATCH Discord + update the store.
async fn apply(
    action: &RefreshAction,
    row: &TrackedMsg,
    fetched: Option<&FetchedPost>,
    store: &Arc<Store>,
    deliverer: &Arc<Deliverer>,
    webhooks: &HashMap<WebhookName, WebhookUrl>,
) -> Result<()> {
    if *action == RefreshAction::None {
        return Ok(());
    }
    let Some(url) = webhooks.get(&WebhookName(row.webhook_name.clone())) else {
        warn!(hook = %row.webhook_name, "refresh: unknown webhook; skipping patch");
        return Ok(());
    };

    match action {
        RefreshAction::None => {}
        RefreshAction::Deleted => {
            // The original body/title aren't stored, so the notice carries only
            // the last-known stats.
            let text = RelayText {
                sender: None,
                body: String::new(),
                reply_quote: None,
                edited: false,
            };
            let meta = EmbedMeta {
                title: "deleted post".to_string(),
                avatar_url: None,
                deep_link: None,
                reactions: row.reactions.clone(),
                comment_count: row.comment_count,
                deleted: true,
            };
            let embed = render::embed(&text, &meta);
            patch(deliverer, url, &row.discord_msg_id, embed).await;
            store.mark_deleted(row.chat_id, row.tg_msg_id)?;
        }
        RefreshAction::Edited | RefreshAction::StatsChanged => {
            let f = fetched.expect("edited/stats implies a fetched post");
            let text = RelayText {
                sender: None,
                body: f.body.clone(),
                reply_quote: None,
                edited: *action == RefreshAction::Edited,
            };
            let meta = EmbedMeta {
                title: f.title.clone(),
                avatar_url: None,
                deep_link: f.deep_link.clone(),
                reactions: f.reactions.clone(),
                comment_count: f.comment_count,
                deleted: false,
            };
            let embed = render::embed(&text, &meta);
            patch(deliverer, url, &row.discord_msg_id, embed).await;
            store.update_stats(
                row.chat_id,
                row.tg_msg_id,
                &row.discord_msg_id,
                &content_hash(&f.body),
                &f.reactions,
                f.comment_count,
            )?;
        }
    }
    Ok(())
}

async fn patch(
    deliverer: &Arc<Deliverer>,
    url: &WebhookUrl,
    discord_msg_id: &str,
    embed: serde_json::Value,
) {
    if let Outcome::Dropped { reason } = deliverer.patch_embed(url, discord_msg_id, embed).await {
        warn!(%reason, discord_msg_id, "refresh PATCH dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, i32)]) -> BTreeMap<String, i32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn tracked(body: &str, reactions: &[(&str, i32)], comments: i32) -> TrackedMsg {
        TrackedMsg {
            chat_id: 1,
            tg_msg_id: 5,
            webhook_name: "hook".into(),
            discord_msg_id: "d1".into(),
            content_hash: content_hash(body),
            reactions: map(reactions),
            comment_count: comments,
        }
    }

    fn fetched(body: &str, reactions: &[(&str, i32)], comments: i32) -> FetchedPost {
        FetchedPost {
            title: "Chan".into(),
            body: body.into(),
            deep_link: Some("https://t.me/c/5".into()),
            reactions: map(reactions),
            comment_count: comments,
        }
    }

    #[test]
    fn diff_none_when_unchanged() {
        let t = tracked("hello", &[("❤️", 3)], 2);
        let f = fetched("hello", &[("❤️", 3)], 2);
        assert_eq!(diff(&t, Some(&f)), RefreshAction::None);
    }

    #[test]
    fn diff_deleted_when_missing() {
        let t = tracked("hello", &[], 0);
        assert_eq!(diff(&t, None), RefreshAction::Deleted);
    }

    #[test]
    fn diff_edited_when_body_changes() {
        let t = tracked("hello", &[("❤️", 3)], 2);
        let f = fetched("hello world", &[("❤️", 3)], 2);
        assert_eq!(diff(&t, Some(&f)), RefreshAction::Edited);
    }

    #[test]
    fn diff_stats_when_reactions_or_comments_change() {
        let t = tracked("hello", &[("❤️", 3)], 2);
        assert_eq!(
            diff(&t, Some(&fetched("hello", &[("❤️", 9)], 2))),
            RefreshAction::StatsChanged
        );
        assert_eq!(
            diff(&t, Some(&fetched("hello", &[("❤️", 3)], 7))),
            RefreshAction::StatsChanged
        );
    }

    #[test]
    fn edit_takes_precedence_over_stats() {
        let t = tracked("hello", &[("❤️", 3)], 2);
        // both body and reactions changed -> Edited wins
        let f = fetched("changed", &[("🔥", 1)], 9);
        assert_eq!(diff(&t, Some(&f)), RefreshAction::Edited);
    }

    #[test]
    fn content_hash_is_stable_and_distinct() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    // A fake fetcher proving the trait is injectable for the fetch/diff loop.
    struct FakeFetcher {
        posts: HashMap<i32, Option<FetchedPost>>,
    }

    impl PostFetcher for FakeFetcher {
        async fn fetch(&self, _chat: ChatId, ids: &[i32]) -> Result<Vec<Option<FetchedPost>>> {
            Ok(ids
                .iter()
                .map(|id| self.posts.get(id).cloned().unwrap_or(None))
                .collect())
        }
    }

    #[tokio::test]
    async fn fake_fetcher_aligns_results_by_index() {
        let mut posts = HashMap::new();
        posts.insert(5, Some(fetched("hi", &[("❤️", 1)], 0)));
        posts.insert(7, None); // deleted
        let f = FakeFetcher { posts };
        let out = f.fetch(ChatId(1), &[5, 6, 7]).await.unwrap();
        assert_eq!(out.len(), 3);
        assert!(out[0].is_some());
        assert!(out[1].is_none()); // unknown id -> None
        assert!(out[2].is_none()); // explicitly deleted
    }
}
