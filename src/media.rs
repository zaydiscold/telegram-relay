//! Media lane: album coalescing.
//!
//! Telegram delivers each photo/video of an "album" (media group) as a
//! *separate* `NewMessage` update, all sharing the same `grouped_id`, arriving
//! back-to-back. Posting each as its own Discord message would fragment the
//! album, so [`AlbumBuffer`] groups siblings by `grouped_id` and flushes them
//! together once a quiet window (default 1s) has elapsed with no new sibling.
//!
//! Ungrouped media (`grouped_id == None`) flushes immediately.
//!
//! [`MediaItem`] carries *pre-downloaded* bytes plus all routing info, so the
//! buffer is fully testable without any grammers types — the download happens in
//! `main` before `push`.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::config::{ChatId, WebhookName, WebhookUrl};

/// A resolved fan-out target for a media post: which route selected it and the
/// named webhook to deliver to. The webhook *name* is required so the post can
/// be recorded in the store (keyed on webhook name) for dedup + refresh.
#[derive(Clone)]
pub struct MediaTarget {
    /// Route name that selected this webhook (store record + display username).
    pub route: String,
    /// Webhook name — the store dedup/refresh key.
    pub webhook: WebhookName,
    /// Resolved webhook URL.
    pub url: WebhookUrl,
}

/// One downloaded media file ready to post, with its routing/context attached.
///
/// An album (media group) arrives as several of these sharing `grouped_id`;
/// [`AlbumBuffer`] coalesces them so a single Discord message carries every
/// file. Each item carries the *full* fan-out target list (deduped by webhook
/// at flush time) plus the embed metadata (`title`, `caption`, `deep_link`), so
/// the media post renders as a rich embed identical to the text path.
#[derive(Clone)]
pub struct MediaItem {
    /// Album id, if this file is part of a media group.
    pub grouped_id: Option<i64>,
    /// Telegram message id (ordering / dedup / per-file de-duplication).
    pub msg_id: i32,
    /// Source chat.
    pub chat: ChatId,
    /// Filename to present on the uploaded attachment.
    pub filename: String,
    /// The already-downloaded file bytes.
    pub bytes: Vec<u8>,
    /// Caption text (only the caption-bearing message of an album has one).
    pub caption: String,
    /// t.me deep link to this message, if the chat has a public username.
    pub deep_link: Option<String>,
    /// Channel/chat title for the embed author.
    pub title: String,
    /// Message sender display name, if any (for the webhook username).
    pub sender: Option<String>,
    /// Distinct routes×webhooks this album should fan out to.
    pub targets: Vec<MediaTarget>,
}

/// Coalesces album (media-group) siblings into a single batch.
///
/// Keyed by `grouped_id`; each group records the batch so far and the
/// [`Instant`] of its most recent sibling. [`tick`](Self::tick) — polled from
/// the main loop every ~250ms — flushes any group whose quiet window has
/// elapsed. Uses [`tokio::time::Instant`] so it honors `tokio::time::advance`
/// in `start_paused` tests.
pub struct AlbumBuffer {
    window: Duration,
    groups: HashMap<i64, (Vec<MediaItem>, Instant)>,
}

impl AlbumBuffer {
    /// Create a buffer that flushes a group after `window` of no new siblings.
    pub fn new(window: Duration) -> Self {
        AlbumBuffer {
            window,
            groups: HashMap::new(),
        }
    }

    /// Add an item.
    ///
    /// Ungrouped items (`grouped_id == None`) are returned immediately as a
    /// one-element batch. Grouped items are buffered (returns `None`); they are
    /// released later by [`tick`](Self::tick) once the quiet window passes.
    pub async fn push(&mut self, item: MediaItem) -> Option<Vec<MediaItem>> {
        match item.grouped_id {
            None => Some(vec![item]),
            Some(gid) => {
                let now = Instant::now();
                let entry = self.groups.entry(gid).or_insert_with(|| (Vec::new(), now));
                entry.0.push(item);
                entry.1 = now;
                None
            }
        }
    }

    /// Flush one group whose quiet window has elapsed, if any.
    ///
    /// Returns at most one batch per call; the caller should loop until this
    /// returns `None` to drain every expired group in a tick.
    pub async fn tick(&mut self) -> Option<Vec<MediaItem>> {
        let now = Instant::now();
        let expired = self
            .groups
            .iter()
            .find(|(_, (_, last))| now.duration_since(*last) >= self.window)
            .map(|(gid, _)| *gid);
        expired.and_then(|gid| self.groups.remove(&gid).map(|(items, _)| items))
    }

    /// Number of groups currently buffered (for diagnostics/heartbeat).
    pub fn pending_groups(&self) -> usize {
        self.groups.len()
    }

    /// Drain every buffered group, returning each as its own batch.
    ///
    /// Used on shutdown to force-flush albums still inside their quiet window so
    /// they are delivered rather than dropped. Leaves the buffer empty.
    pub fn flush_all(&mut self) -> Vec<Vec<MediaItem>> {
        self.groups.drain().map(|(_, (items, _))| items).collect()
    }
}

/// Sort an album batch by msg_id in ascending order.
///
/// Concurrent downloads may complete out of order; sorting ensures album items
/// are posted in canonical order and the caption (which rides on the first item)
/// is paired with the correct msg_id.
pub fn sort_album_batch(batch: &mut [MediaItem]) {
    batch.sort_by_key(|item| item.msg_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_item(grouped_id: Option<i64>, msg_id: i32) -> MediaItem {
        MediaItem {
            grouped_id,
            msg_id,
            chat: ChatId(1),
            filename: format!("f{msg_id}.bin"),
            bytes: vec![0u8, 1, 2, 3],
            caption: String::new(),
            deep_link: None,
            title: "tester".into(),
            sender: None,
            targets: vec![MediaTarget {
                route: "r1".into(),
                webhook: WebhookName("hook".into()),
                url: WebhookUrl("https://example.invalid/webhook".into()),
            }],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ungrouped_flushes_immediately() {
        let mut b = AlbumBuffer::new(Duration::from_secs(1));
        let out = b.push(fake_item(None, 1)).await;
        assert_eq!(out.map(|v| v.len()), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn grouped_coalesces_within_window() {
        let mut b = AlbumBuffer::new(Duration::from_secs(1));
        assert!(b.push(fake_item(Some(7), 1)).await.is_none());
        assert!(b.push(fake_item(Some(7), 2)).await.is_none());
        tokio::time::advance(Duration::from_millis(1100)).await;
        let out = b.tick().await; // timer-driven flush
        assert_eq!(out.map(|v| v.len()), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn grouped_not_flushed_before_window() {
        let mut b = AlbumBuffer::new(Duration::from_secs(1));
        assert!(b.push(fake_item(Some(9), 1)).await.is_none());
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(b.tick().await.is_none(), "should still be buffering");
        assert_eq!(b.pending_groups(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn late_sibling_resets_window() {
        let mut b = AlbumBuffer::new(Duration::from_secs(1));
        assert!(b.push(fake_item(Some(3), 1)).await.is_none());
        tokio::time::advance(Duration::from_millis(800)).await;
        // sibling arrives, resetting the quiet window
        assert!(b.push(fake_item(Some(3), 2)).await.is_none());
        tokio::time::advance(Duration::from_millis(800)).await;
        assert!(
            b.tick().await.is_none(),
            "window should have reset on the late sibling"
        );
        tokio::time::advance(Duration::from_millis(400)).await;
        let out = b.tick().await;
        assert_eq!(out.map(|v| v.len()), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn flush_all_returns_all_buffered_groups() {
        let mut b = AlbumBuffer::new(Duration::from_secs(1));
        // Two grouped items in one album, plus a second album — none past window.
        assert!(b.push(fake_item(Some(1), 1)).await.is_none());
        assert!(b.push(fake_item(Some(1), 2)).await.is_none());
        assert!(b.push(fake_item(Some(2), 3)).await.is_none());
        assert_eq!(b.pending_groups(), 2);

        let mut batches = b.flush_all();
        batches.sort_by_key(|batch| batch.len());
        assert_eq!(batches.len(), 2);
        // One group has 2 items, the other has 1.
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 2);
        // Buffer is now empty.
        assert_eq!(b.pending_groups(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_groups_each_flush() {
        let mut b = AlbumBuffer::new(Duration::from_secs(1));
        b.push(fake_item(Some(1), 1)).await;
        b.push(fake_item(Some(2), 2)).await;
        tokio::time::advance(Duration::from_millis(1100)).await;
        let mut batches = Vec::new();
        while let Some(batch) = b.tick().await {
            batches.push(batch);
        }
        assert_eq!(batches.len(), 2);
        assert_eq!(b.pending_groups(), 0);
    }

    #[test]
    fn album_batch_sorts_by_msg_id() {
        // Create items with msg_ids out of order: [3, 1, 2]
        let mut batch = vec![
            fake_item(Some(100), 3),
            fake_item(Some(100), 1),
            fake_item(Some(100), 2),
        ];

        // Apply the sort helper
        sort_album_batch(&mut batch);

        // Verify order is now [1, 2, 3]
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].msg_id, 1);
        assert_eq!(batch[1].msg_id, 2);
        assert_eq!(batch[2].msg_id, 3);
    }
}
