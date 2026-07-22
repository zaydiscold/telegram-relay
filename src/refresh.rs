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
    /// Discussion/comment count as reported by this fetch. `None` = "unknown":
    /// the refresh fetch (`get_messages_by_id`) does not populate `replies`, so
    /// `reply_count()` returns `None` and the stored value must be preserved
    /// rather than downgraded to 0 (queued-polish §10b).
    pub comment_count: Option<i32>,
    /// ORIGINAL Telegram publish time, RFC3339. Re-read on every fetch so a
    /// PATCHed embed keeps the source timestamp instead of losing it.
    pub date: Option<String>,
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
///
/// `pub` (not private): main.rs's `backfill` command lives in the separate
/// binary crate, so it needs this exported to reuse the exact same
/// title/body/deep-link/reactions/comment-count extraction the refresh worker
/// uses, rather than duplicating it.
pub fn to_fetched(msg: &grammers_client::message::Message) -> FetchedPost {
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
        // `None` (reply_count not populated) is preserved as "unknown" — the
        // diff/apply path keeps the stored count instead of writing 0 (§10b).
        comment_count: msg.reply_count(),
        date: Some(render::embed_timestamp(msg.date())),
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

/// Current Unix time in seconds (relay clock — same basis as the store's
/// `posted_at`/`last_checked`).
fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Per-row refresh decision, derived purely from timing + config.
///
/// Splits the two cadences (queued-polish §1): edits/deletes are re-checked on
/// the `interval_mins` cadence for the whole horizon, while reactions/comments
/// are checked only at a few early checkpoints and then frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshDecision {
    /// Re-fetch this row from Telegram this tick.
    pub fetch: bool,
    /// If fetched, a reaction/comment change may be applied (the post is still
    /// within the reaction horizon). Edits/deletes are always applied when
    /// fetched (the caller only fetches rows still inside the edit/delete
    /// horizon, which `Store::due` already enforces).
    pub allow_stats: bool,
}

/// Decide whether (and how) to refresh one row this tick, from its age, the time
/// since its last check, and its age at that last check.
///
/// A **reaction check** is due when the post is still inside the reaction horizon
/// AND age has just crossed one of the reaction checkpoints
/// (`reaction_early_check_secs`, `interval_mins`, `reaction_horizon_mins`) that
/// the last check had not yet passed — so each checkpoint fires exactly once as
/// age grows. An **edit/delete check** is due every `interval_mins` (measured
/// from the last check). Either makes the row worth fetching; a single fetch
/// answers both. Reaction/comment changes are only *applied* while inside the
/// reaction horizon (`allow_stats`).
pub fn decide_refresh(
    age_secs: i64,
    since_checked_secs: i64,
    last_checked_age_secs: i64,
    cfg: &RefreshCfg,
) -> RefreshDecision {
    let interval = (cfg.interval_mins * 60) as i64;
    let reaction_horizon = (cfg.reaction_horizon_mins * 60) as i64;
    let early = cfg.reaction_early_check_secs as i64;

    // Reaction checkpoints, relative to the post's age. A checkpoint C fires when
    // last_checked_age < C <= age (age just crossed it since the last check).
    let checkpoints = [early, interval, reaction_horizon];
    let reaction_due = age_secs <= reaction_horizon
        && checkpoints
            .iter()
            .any(|&c| last_checked_age_secs < c && c <= age_secs);

    // Edit/delete cadence: every `interval` since the last check.
    let edit_due = since_checked_secs >= interval;

    RefreshDecision {
        fetch: reaction_due || edit_due,
        allow_stats: age_secs <= reaction_horizon,
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

/// The comment count to display/store for a fetched post, given the value we
/// already have stored.
///
/// The refresh fetch (`get_messages_by_id`) does not populate `replies`, so
/// `reply_count()` returns `None`. Treat that as "unknown" and keep the stored
/// count rather than downgrading a real value to 0 (queued-polish §10b). Only a
/// concrete `Some(n)` updates the count.
pub fn effective_comment_count(fetched: Option<i32>, stored: i32) -> i32 {
    fetched.unwrap_or(stored)
}

/// Pure diff: what (if anything) needs PATCHing for one tracked message.
pub fn diff(tracked: &TrackedMsg, fetched: Option<&FetchedPost>) -> RefreshAction {
    match fetched {
        None => RefreshAction::Deleted,
        Some(f) => {
            if content_hash(&f.body) != tracked.content_hash {
                RefreshAction::Edited
            } else {
                // An unknown (`None`) comment count is never a change — preserve
                // the stored value instead of reporting a spurious StatsChanged.
                let comments_changed = match f.comment_count {
                    Some(n) => n != tracked.comment_count,
                    None => false,
                };
                if f.reactions != tracked.reactions || comments_changed {
                    RefreshAction::StatsChanged
                } else {
                    RefreshAction::None
                }
            }
        }
    }
}

/// Per-route embed stripe colors, snapshotted at startup.
///
/// A PATCHed embed must re-declare `color` — Discord replaces the whole embed
/// object, so omitting it would drop the stripe to gray on the very first
/// reaction update. Routes missing from the map (added after startup, or rows
/// left by an older config) fall back to [`render::DEFAULT_EMBED_COLOR`].
pub type RouteColors = HashMap<String, u32>;

/// Look up a route's stripe color, falling back to the default.
pub fn color_for(colors: &RouteColors, route: &str) -> u32 {
    colors
        .get(route)
        .copied()
        .unwrap_or(render::DEFAULT_EMBED_COLOR)
}

/// The base worker tick.
///
/// It runs at the reaction early-check granularity (default 60s, capped) rather
/// than the 30-min edit interval, so the "+1 min" reaction checkpoint can be
/// honored. Each tick is cheap: per-row gating in [`tick_once`] fetches only the
/// rows whose cadence is actually due, so a fast tick does NOT mean fast
/// Telegram polling (queued-polish §1).
fn base_tick_secs(cfg: &RefreshCfg) -> u64 {
    cfg.reaction_early_check_secs.clamp(1, 60)
}

/// The interval worker. Runs until the process ends.
pub async fn run<F: PostFetcher>(
    fetcher: F,
    store: Arc<Store>,
    deliverer: Arc<Deliverer>,
    webhooks: HashMap<WebhookName, WebhookUrl>,
    colors: RouteColors,
    cfg: RefreshCfg,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(base_tick_secs(&cfg)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; skip it so we don't refresh before anything
    // has been posted.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if let Err(e) = tick_once(&fetcher, &store, &deliverer, &webhooks, &colors, &cfg).await {
            warn!(error = %e, "refresh tick failed");
        }
    }
}

/// A single refresh pass: fetch due posts per chat, apply diffs, then prune.
#[allow(clippy::too_many_arguments)]
pub async fn tick_once<F: PostFetcher>(
    fetcher: &F,
    store: &Arc<Store>,
    deliverer: &Arc<Deliverer>,
    webhooks: &HashMap<WebhookName, WebhookUrl>,
    colors: &RouteColors,
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

    let now = now_secs();
    for (chat_id, rows) in by_chat {
        // Per-row cadence gate: decide which rows are actually due this tick, so
        // a fast base tick does not translate into fast Telegram polling.
        let decisions: Vec<RefreshDecision> = rows
            .iter()
            .map(|r| {
                let age = now - r.posted_at;
                let since_checked = now - r.last_checked;
                let last_checked_age = r.last_checked - r.posted_at;
                decide_refresh(age, since_checked, last_checked_age, cfg)
            })
            .collect();

        // Unique tg ids that want a fetch this tick, batched at BATCH_MAX.
        let mut ids: Vec<i32> = rows
            .iter()
            .zip(&decisions)
            .filter(|(_, d)| d.fetch)
            .map(|(r, _)| r.tg_msg_id)
            .collect();
        if ids.is_empty() {
            continue; // nothing due for this chat this tick
        }
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

        for (row, decision) in rows.iter().zip(&decisions) {
            if !decision.fetch {
                continue; // not due this tick
            }
            // If the id wasn't in any successful batch, don't guess it's deleted.
            let Some(slot) = fetched.get(&row.tg_msg_id) else {
                continue;
            };
            let mut action = diff(row, slot.as_ref());
            // Freeze reaction/comment updates past the reaction horizon; edits
            // and deletes still apply for the full edit/delete horizon.
            if action == RefreshAction::StatsChanged && !decision.allow_stats {
                action = RefreshAction::None;
            }
            if action == RefreshAction::None {
                // Fetched but nothing to PATCH: still advance last_checked so the
                // cadence gate progresses (otherwise a checkpoint re-fires every
                // base tick).
                if let Err(e) = store.touch_checked(row.chat_id, row.tg_msg_id, &row.discord_msg_id)
                {
                    warn!(chat_id, tg = row.tg_msg_id, error = %e, "touch_checked failed");
                }
                continue;
            }
            if let Err(e) = apply(
                &action,
                row,
                slot.as_ref(),
                store,
                deliverer,
                webhooks,
                colors,
            )
            .await
            {
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
///
/// A PATCH replaces the embed wholesale, so `color` (and, where recoverable,
/// `timestamp`) must be re-declared on every rebuild here — otherwise the first
/// reaction update would strip the stripe and the source time off the post.
#[allow(clippy::too_many_arguments)]
async fn apply(
    action: &RefreshAction,
    row: &TrackedMsg,
    fetched: Option<&FetchedPost>,
    store: &Arc<Store>,
    deliverer: &Arc<Deliverer>,
    webhooks: &HashMap<WebhookName, WebhookUrl>,
    colors: &RouteColors,
) -> Result<()> {
    if *action == RefreshAction::None {
        return Ok(());
    }
    let Some(url) = webhooks.get(&WebhookName(row.webhook_name.clone())) else {
        warn!(hook = %row.webhook_name, "refresh: unknown webhook; skipping patch");
        return Ok(());
    };
    let color = color_for(colors, &row.route);

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
                color,
                // The source is gone, so its publish time is no longer
                // fetchable and the store does not keep it — the tombstone
                // drops the timestamp the same way it drops title and body.
                timestamp: None,
            };
            let mut embed = render::embed(&text, &meta);
            reattach_stored_images(&mut embed, row, None);
            patch(deliverer, url, &row.discord_msg_id, embed).await;
            store.mark_deleted(row.chat_id, row.tg_msg_id)?;
        }
        RefreshAction::Edited | RefreshAction::StatsChanged => {
            let f = fetched.expect("edited/stats implies a fetched post");
            // Preserve the stored comment count when the fetch reports it as
            // unknown (`None`), so a reaction-only refresh never zeroes a real
            // comment count (§10b).
            let comment_count = effective_comment_count(f.comment_count, row.comment_count);
            // Once a post has been edited it STAYS edited (orange): a later
            // stats-only refresh must not revert the stripe. So OR the freshly
            // detected edit with the persisted flag.
            let is_edited = row.edited || *action == RefreshAction::Edited;
            if *action == RefreshAction::Edited && !row.edited {
                store.mark_edited(row.chat_id, row.tg_msg_id)?;
            }
            let text = RelayText {
                sender: None,
                body: f.body.clone(),
                reply_quote: None,
                edited: is_edited,
            };
            let meta = EmbedMeta {
                title: f.title.clone(),
                avatar_url: None,
                deep_link: f.deep_link.clone(),
                reactions: f.reactions.clone(),
                comment_count,
                deleted: false,
                color,
                timestamp: f.date.clone(),
            };
            let mut embed = render::embed(&text, &meta);
            reattach_stored_images(&mut embed, row, f.deep_link.as_deref());
            patch(deliverer, url, &row.discord_msg_id, embed).await;
            store.update_stats(
                row.chat_id,
                row.tg_msg_id,
                &row.discord_msg_id,
                &content_hash(&f.body),
                &f.reactions,
                comment_count,
            )?;
        }
    }
    Ok(())
}

/// Re-reference a tracked post's images (by their stored Discord CDN url) inside
/// the embed being PATCHed. A PATCH sends no attachments, so without this the
/// `attachment://` image from the original post would be stripped on every
/// reaction/edit/delete update.
fn reattach_stored_images(embed: &mut serde_json::Value, row: &TrackedMsg, gallery: Option<&str>) {
    if row.image_urls.is_empty() {
        return;
    }
    let urls: Vec<&str> = row.image_urls.iter().map(String::as_str).collect();
    render::attach_image_urls(embed, &urls, gallery);
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
            route: "r1".into(),
            webhook_name: "hook".into(),
            discord_msg_id: "d1".into(),
            content_hash: content_hash(body),
            reactions: map(reactions),
            comment_count: comments,
            posted_at: 0,
            last_checked: 0,
            edited: false,
            image_urls: Vec::new(),
        }
    }

    fn fetched(body: &str, reactions: &[(&str, i32)], comments: i32) -> FetchedPost {
        FetchedPost {
            title: "Chan".into(),
            body: body.into(),
            deep_link: Some("https://t.me/c/5".into()),
            reactions: map(reactions),
            comment_count: Some(comments),
            date: Some("2026-07-20T12:00:00Z".into()),
        }
    }

    /// Like [`fetched`] but with an unknown (`None`) comment count — mirrors the
    /// real refresh fetch, where `reply_count()` is not populated.
    fn fetched_unknown_comments(body: &str, reactions: &[(&str, i32)]) -> FetchedPost {
        FetchedPost {
            comment_count: None,
            ..fetched(body, reactions, 0)
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

    // ---- decide_refresh cadence gate (queued-polish §1) ----
    // Defaults: interval 30m=1800s, reaction_horizon 60m=3600s, early 60s.

    fn cadence_cfg() -> RefreshCfg {
        RefreshCfg::default()
    }

    #[test]
    fn early_reaction_checkpoint_fires_at_one_minute() {
        // Fresh post, first evaluated at +60s: the early checkpoint fires.
        let cfg = cadence_cfg();
        let d = decide_refresh(60, 60, 0, &cfg);
        assert!(d.fetch, "the +1min reaction check must fire");
        assert!(d.allow_stats);
    }

    #[test]
    fn no_fetch_between_checkpoints() {
        // Checked at +60s (last_checked_age = 60); now +120s. No checkpoint has
        // been crossed and the 30-min edit cadence hasn't elapsed -> no fetch.
        let cfg = cadence_cfg();
        let d = decide_refresh(120, 60, 60, &cfg);
        assert!(!d.fetch, "should not poll Telegram between checkpoints");
    }

    #[test]
    fn interval_checkpoint_fires_at_thirty_minutes() {
        // Last checked at +60s; now +30min. The interval checkpoint (1800s) has
        // just been crossed -> fetch, and reactions still apply (<= horizon).
        let cfg = cadence_cfg();
        let d = decide_refresh(1800, 1800 - 60, 60, &cfg);
        assert!(d.fetch);
        assert!(d.allow_stats);
    }

    #[test]
    fn reaction_horizon_checkpoint_fires_at_sixty_minutes() {
        // Last checked at +30min; now +60min: the reaction-horizon checkpoint
        // fires and the 30-min edit cadence is also due. Still within horizon.
        let cfg = cadence_cfg();
        let d = decide_refresh(3600, 1800, 1800, &cfg);
        assert!(d.fetch);
        assert!(d.allow_stats, "age == horizon is still inside the horizon");
    }

    #[test]
    fn past_reaction_horizon_still_checks_edits_but_freezes_stats() {
        // +90min, last checked +60min: the edit cadence fires (fetch), but the
        // post is past the reaction horizon so reaction/comment stats are frozen.
        let cfg = cadence_cfg();
        let d = decide_refresh(5400, 1800, 3600, &cfg);
        assert!(d.fetch, "edits/deletes still checked every 30 min");
        assert!(!d.allow_stats, "reactions frozen past the 1h horizon");
    }

    #[test]
    fn edit_cadence_recurs_every_interval_far_past_reaction_horizon() {
        // Deep into the 48h horizon: no reaction checkpoint remains, but the edit
        // cadence keeps firing every 30 min and stats stay frozen.
        let cfg = cadence_cfg();
        // 10h old, last checked 30min ago.
        let age = 10 * 3600;
        let d = decide_refresh(age, 1800, age - 1800, &cfg);
        assert!(d.fetch);
        assert!(!d.allow_stats);
        // ...but only just short of the interval -> no fetch yet.
        let d2 = decide_refresh(age, 1799, age - 1799, &cfg);
        assert!(!d2.fetch);
    }

    #[test]
    fn allow_stats_boundary_is_inclusive() {
        let cfg = cadence_cfg();
        assert!(decide_refresh(3600, 3600, 0, &cfg).allow_stats);
        assert!(!decide_refresh(3601, 3601, 0, &cfg).allow_stats);
    }

    #[test]
    fn each_reaction_checkpoint_fires_at_most_once() {
        // Once the early checkpoint has been consumed (last_checked_age just past
        // it), re-evaluating at a slightly larger age does NOT re-fire it.
        let cfg = cadence_cfg();
        // age 65s, last checked at 61s (already past the 60s early checkpoint),
        // 30-min cadence not elapsed -> no fetch.
        let d = decide_refresh(65, 4, 61, &cfg);
        assert!(!d.fetch, "a consumed checkpoint must not re-fire");
    }

    #[test]
    fn unknown_comment_count_is_not_a_stats_change() {
        // The live bug (§10b): a refresh fetch returns reply_count() == None, so
        // comment_count is unknown. With reactions unchanged, this must NOT read
        // as StatsChanged — otherwise the embed would be PATCHed to 0 comments.
        let t = tracked("hello", &[("❤️", 3)], 47);
        let f = fetched_unknown_comments("hello", &[("❤️", 3)]);
        assert_eq!(diff(&t, Some(&f)), RefreshAction::None);
    }

    #[test]
    fn unknown_comment_count_still_lets_reactions_update() {
        // Comments unknown but reactions grew -> StatsChanged (reactions only).
        let t = tracked("hello", &[("❤️", 3)], 47);
        let f = fetched_unknown_comments("hello", &[("❤️", 9)]);
        assert_eq!(diff(&t, Some(&f)), RefreshAction::StatsChanged);
    }

    #[test]
    fn known_comment_count_updates() {
        // A concrete Some(n) that differs is still a StatsChanged.
        let t = tracked("hello", &[("❤️", 3)], 47);
        let f = fetched("hello", &[("❤️", 3)], 50);
        assert_eq!(diff(&t, Some(&f)), RefreshAction::StatsChanged);
    }

    #[test]
    fn effective_comment_count_prefers_known_keeps_stored_on_unknown() {
        // Some(n) wins; None keeps the stored value (never downgrades to 0).
        assert_eq!(effective_comment_count(Some(50), 47), 50);
        assert_eq!(effective_comment_count(None, 47), 47);
        assert_eq!(effective_comment_count(Some(0), 47), 0);
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
