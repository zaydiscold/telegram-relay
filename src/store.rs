//! SQLite-backed store of relayed messages, enabling live-updating embeds.
//!
//! Every Discord message we post from a Telegram post is recorded here so the
//! refresh worker (see [`crate::refresh`]) can later re-fetch the source, notice
//! reaction/comment/edit/delete changes, and PATCH the Discord embed in place.
//!
//! One Telegram post can fan out to several Discord messages (multiple routes /
//! webhooks), so the primary key is `(chat_id, tg_msg_id, discord_msg_id)`.
//!
//! `reactions` is stored as a JSON object mapping emoji -> count. The connection
//! runs in WAL mode and is guarded by a `Mutex` so the store is `Send + Sync`
//! and can be shared (via `Arc`) between the hot path and the refresh worker.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// A row to insert after a successful Discord post.
#[derive(Debug, Clone)]
pub struct NewRecord {
    pub chat_id: i64,
    pub tg_msg_id: i32,
    pub route: String,
    pub webhook_name: String,
    pub discord_msg_id: String,
    pub content_hash: String,
    pub reactions: BTreeMap<String, i32>,
    pub comment_count: i32,
}

/// A tracked Discord message that the refresh worker may need to update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedMsg {
    pub chat_id: i64,
    pub tg_msg_id: i32,
    /// Config route name this was relayed by — carried so the refresh worker can
    /// look the route's embed color back up when it PATCHes.
    pub route: String,
    pub webhook_name: String,
    pub discord_msg_id: String,
    pub content_hash: String,
    pub reactions: BTreeMap<String, i32>,
    pub comment_count: i32,
}

/// Serialize a reactions map to the JSON stored in the `reactions` column.
pub fn reactions_to_json(reactions: &BTreeMap<String, i32>) -> String {
    serde_json::to_string(reactions).unwrap_or_else(|_| "{}".to_string())
}

/// Parse the `reactions` column back into a map (empty on any error).
pub fn reactions_from_json(s: &str) -> BTreeMap<String, i32> {
    serde_json::from_str(s).unwrap_or_default()
}

/// Restrict the store file (and its `-wal`/`-shm` siblings, if present) to
/// owner-only permissions (0600). The relay's own message metadata should not
/// be readable by other local users. Missing sidecar files are not an error
/// (WAL/SHM are created on first write, and may be checkpointed away).
#[cfg(unix)]
fn restrict_db_permissions(path: &Path) {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    for suffix in ["", "-wal", "-shm"] {
        let sidecar = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let mut s = path.as_os_str().to_owned();
            s.push(suffix);
            std::path::PathBuf::from(s)
        };
        match fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    "failed to restrict permissions on {}: {e}",
                    sidecar.display()
                );
            }
        }
    }
}

#[cfg(not(unix))]
fn restrict_db_permissions(_path: &Path) {}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A handle to the relayed-message store.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (or create) the store at `path`, enabling WAL mode and creating the
    /// schema if needed.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite store at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous=NORMAL")?;
        restrict_db_permissions(path);
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS relayed (
              chat_id INTEGER NOT NULL, tg_msg_id INTEGER NOT NULL,
              route TEXT NOT NULL, webhook_name TEXT NOT NULL,
              discord_msg_id TEXT NOT NULL,
              posted_at INTEGER NOT NULL, last_checked INTEGER NOT NULL,
              content_hash TEXT NOT NULL,
              reactions TEXT NOT NULL DEFAULT '{}',
              comment_count INTEGER NOT NULL DEFAULT 0,
              deleted INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (chat_id, tg_msg_id, discord_msg_id));

            -- Tracks the last identity (title + photo) pushed to each Discord
            -- webhook, so the avatar/name PATCH only fires when the source
            -- channel's photo or title actually changed (rate-limit friendly).
            CREATE TABLE IF NOT EXISTS webhook_identity (
              webhook_name TEXT PRIMARY KEY,
              identity_hash TEXT NOT NULL,
              updated_at INTEGER NOT NULL);
            "#,
        )
        .context("creating relayed table")?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Record a freshly-posted Discord message. Idempotent on the primary key.
    pub fn record(&self, rec: NewRecord) -> Result<()> {
        let now = now_secs();
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO relayed
              (chat_id, tg_msg_id, route, webhook_name, discord_msg_id,
               posted_at, last_checked, content_hash, reactions, comment_count, deleted)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, 0)
            ON CONFLICT(chat_id, tg_msg_id, discord_msg_id) DO UPDATE SET
              content_hash = excluded.content_hash,
              reactions = excluded.reactions,
              comment_count = excluded.comment_count,
              last_checked = excluded.last_checked
            "#,
            rusqlite::params![
                rec.chat_id,
                rec.tg_msg_id,
                rec.route,
                rec.webhook_name,
                rec.discord_msg_id,
                now,
                rec.content_hash,
                reactions_to_json(&rec.reactions),
                rec.comment_count,
            ],
        )
        .context("inserting relayed row")?;
        Ok(())
    }

    /// Whether a Discord post already exists for this exact (chat, message,
    /// webhook) triple. Used by the `backfill` CLI verb so re-running it (or
    /// overlapping with live relay) skips rather than double-posting.
    pub fn already_relayed(
        &self,
        chat_id: i64,
        tg_msg_id: i32,
        webhook_name: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let exists: bool = conn
            .query_row(
                r#"
                SELECT EXISTS(
                  SELECT 1 FROM relayed
                  WHERE chat_id = ?1 AND tg_msg_id = ?2 AND webhook_name = ?3
                )
                "#,
                rusqlite::params![chat_id, tg_msg_id, webhook_name],
                |r| r.get(0),
            )
            .context("checking already_relayed")?;
        Ok(exists)
    }

    /// All non-deleted tracked messages posted within the last `horizon_hours`.
    ///
    /// Ordered by `chat_id, tg_msg_id` so the refresh worker can batch by chat.
    pub fn due(&self, horizon_hours: u64) -> Result<Vec<TrackedMsg>> {
        let cutoff = now_secs() - (horizon_hours as i64) * 3600;
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT chat_id, tg_msg_id, route, webhook_name, discord_msg_id,
                   content_hash, reactions, comment_count
            FROM relayed
            WHERE deleted = 0 AND posted_at >= ?1
            ORDER BY chat_id, tg_msg_id
            "#,
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            Ok(TrackedMsg {
                chat_id: r.get(0)?,
                tg_msg_id: r.get(1)?,
                route: r.get(2)?,
                webhook_name: r.get(3)?,
                discord_msg_id: r.get(4)?,
                content_hash: r.get(5)?,
                reactions: reactions_from_json(&r.get::<_, String>(6)?),
                comment_count: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Update stats + content hash after re-checking a specific Discord message.
    pub fn update_stats(
        &self,
        chat_id: i64,
        tg_msg_id: i32,
        discord_msg_id: &str,
        content_hash: &str,
        reactions: &BTreeMap<String, i32>,
        comment_count: i32,
    ) -> Result<()> {
        let now = now_secs();
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            r#"
            UPDATE relayed
            SET content_hash = ?4, reactions = ?5, comment_count = ?6, last_checked = ?7
            WHERE chat_id = ?1 AND tg_msg_id = ?2 AND discord_msg_id = ?3
            "#,
            rusqlite::params![
                chat_id,
                tg_msg_id,
                discord_msg_id,
                content_hash,
                reactions_to_json(reactions),
                comment_count,
                now,
            ],
        )
        .context("updating stats")?;
        Ok(())
    }

    /// Mark every Discord message for a Telegram post as deleted (all routes).
    pub fn mark_deleted(&self, chat_id: i64, tg_msg_id: i32) -> Result<()> {
        let now = now_secs();
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "UPDATE relayed SET deleted = 1, last_checked = ?3 WHERE chat_id = ?1 AND tg_msg_id = ?2",
            rusqlite::params![chat_id, tg_msg_id, now],
        )
        .context("marking deleted")?;
        Ok(())
    }

    /// Drop rows older than `horizon_hours` (they are no longer refreshed).
    pub fn prune(&self, horizon_hours: u64) -> Result<usize> {
        let cutoff = now_secs() - (horizon_hours as i64) * 3600;
        let conn = self.conn.lock().expect("store mutex poisoned");
        let n = conn
            .execute("DELETE FROM relayed WHERE posted_at < ?1", [cutoff])
            .context("pruning old rows")?;
        Ok(n)
    }

    /// The identity hash last pushed to `webhook_name`, if any.
    ///
    /// Used to skip the webhook avatar/name PATCH when the source channel's
    /// title + photo are unchanged since the last sync.
    pub fn webhook_identity_hash(&self, webhook_name: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let hash = conn
            .query_row(
                "SELECT identity_hash FROM webhook_identity WHERE webhook_name = ?1",
                rusqlite::params![webhook_name],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(hash)
    }

    /// Record the identity hash just pushed to `webhook_name` (upsert).
    pub fn set_webhook_identity_hash(&self, webhook_name: &str, identity_hash: &str) -> Result<()> {
        let now = now_secs();
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO webhook_identity (webhook_name, identity_hash, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(webhook_name) DO UPDATE SET
              identity_hash = excluded.identity_hash,
              updated_at = excluded.updated_at
            "#,
            rusqlite::params![webhook_name, identity_hash, now],
        )
        .context("upserting webhook identity hash")?;
        Ok(())
    }

    /// Test/diagnostic helper: total row count.
    pub fn count(&self) -> Result<i64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let n = conn.query_row("SELECT COUNT(*) FROM relayed", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Test helper: backdate every row's `posted_at` by `secs` seconds so
    /// prune/due horizon behaviour can be exercised without real time passing.
    #[cfg(test)]
    fn backdate_all(&self, secs: i64) {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute("UPDATE relayed SET posted_at = posted_at - ?1", [secs])
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tr-store-{}-{}.db", std::process::id(), tag))
    }

    fn map(pairs: &[(&str, i32)]) -> BTreeMap<String, i32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn rec(chat: i64, tg: i32, discord: &str) -> NewRecord {
        NewRecord {
            chat_id: chat,
            tg_msg_id: tg,
            route: "r1".into(),
            webhook_name: "hook".into(),
            discord_msg_id: discord.into(),
            content_hash: "h0".into(),
            reactions: map(&[("❤️", 1)]),
            comment_count: 0,
        }
    }

    #[test]
    fn record_due_update_prune_roundtrip() {
        let path = tmp_db("roundtrip");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();

        // record two Discord messages for one Telegram post + one for another
        store.record(rec(100, 5, "d-a")).unwrap();
        store.record(rec(100, 5, "d-b")).unwrap();
        store.record(rec(200, 9, "d-c")).unwrap();
        assert_eq!(store.count().unwrap(), 3);

        // due returns all three, ordered by chat then tg id
        let due = store.due(48).unwrap();
        assert_eq!(due.len(), 3);
        assert_eq!(due[0].chat_id, 100);
        assert_eq!(due[2].chat_id, 200);
        assert_eq!(due[0].reactions.get("❤️"), Some(&1));

        // update stats on one row
        store
            .update_stats(100, 5, "d-a", "h1", &map(&[("🔥", 7)]), 3)
            .unwrap();
        let due = store.due(48).unwrap();
        let updated = due
            .iter()
            .find(|t| t.discord_msg_id == "d-a")
            .expect("row present");
        assert_eq!(updated.content_hash, "h1");
        assert_eq!(updated.comment_count, 3);
        assert_eq!(updated.reactions.get("🔥"), Some(&7));

        // recent rows survive a 48h prune...
        assert_eq!(store.prune(48).unwrap(), 0);
        assert_eq!(store.count().unwrap(), 3);

        // ...but once backdated past the horizon, prune drops them all
        store.backdate_all(49 * 3600);
        let pruned = store.prune(48).unwrap();
        assert_eq!(pruned, 3);
        assert_eq!(store.count().unwrap(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mark_deleted_excludes_from_due() {
        let path = tmp_db("deleted");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();

        store.record(rec(1, 1, "d-a")).unwrap();
        store.record(rec(1, 1, "d-b")).unwrap();
        assert_eq!(store.due(48).unwrap().len(), 2);

        store.mark_deleted(1, 1).unwrap();
        // still present in the table, but excluded from due()
        assert_eq!(store.count().unwrap(), 2);
        assert_eq!(store.due(48).unwrap().len(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_is_idempotent_on_primary_key() {
        let path = tmp_db("idempotent");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();

        store.record(rec(1, 1, "d-a")).unwrap();
        let mut again = rec(1, 1, "d-a");
        again.content_hash = "h9".into();
        again.comment_count = 42;
        store.record(again).unwrap();

        assert_eq!(store.count().unwrap(), 1);
        let due = store.due(48).unwrap();
        assert_eq!(due[0].content_hash, "h9");
        assert_eq!(due[0].comment_count, 42);

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn db_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = tmp_db("perms");
        let _ = std::fs::remove_file(&path);
        let _store = Store::open(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn already_relayed_true_only_for_exact_triple() {
        let path = tmp_db("already-relayed");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();

        store.record(rec(1, 5, "d-a")).unwrap(); // webhook_name "hook" per rec()
        assert!(store.already_relayed(1, 5, "hook").unwrap());
        assert!(!store.already_relayed(1, 5, "other-hook").unwrap());
        assert!(!store.already_relayed(1, 6, "hook").unwrap());
        assert!(!store.already_relayed(2, 5, "hook").unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn webhook_identity_hash_roundtrip_and_upsert() {
        let path = tmp_db("wh-identity");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();

        // Absent until set.
        assert_eq!(store.webhook_identity_hash("hook").unwrap(), None);

        store.set_webhook_identity_hash("hook", "h1").unwrap();
        assert_eq!(
            store.webhook_identity_hash("hook").unwrap(),
            Some("h1".to_string())
        );

        // Upsert replaces the hash for the same webhook.
        store.set_webhook_identity_hash("hook", "h2").unwrap();
        assert_eq!(
            store.webhook_identity_hash("hook").unwrap(),
            Some("h2".to_string())
        );

        // Distinct webhooks are independent.
        assert_eq!(store.webhook_identity_hash("other").unwrap(), None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn due_carries_route_name() {
        let path = tmp_db("due-route");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        store.record(rec(1, 1, "d-a")).unwrap(); // route "r1" per rec()
        let due = store.due(48).unwrap();
        assert_eq!(due[0].route, "r1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reactions_json_roundtrip() {
        let m = map(&[("❤️", 47), ("🔥", 12)]);
        let s = reactions_to_json(&m);
        assert_eq!(reactions_from_json(&s), m);
        assert_eq!(reactions_from_json("garbage"), BTreeMap::new());
    }
}
