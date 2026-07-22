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
    /// End-to-end relay latency in milliseconds (Discord snowflake time minus
    /// Telegram publish time). `Some` only for LIVE relays; backfilled rows carry
    /// `None` because their latency is meaningless. (queued-polish §3)
    pub latency_ms: Option<i64>,
}

/// Aggregate latency figures over the recorded LIVE relays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyStats {
    pub count: usize,
    pub p50: i64,
    pub p95: i64,
    pub min: i64,
    pub max: i64,
}

/// A snapshot of store-wide counters for the `stats` CLI verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStats {
    /// Distinct Telegram posts currently tracked (across all fan-out webhooks).
    pub tracked_posts: i64,
    /// Rows marked deleted on Telegram.
    pub deleted: i64,
    /// Latency percentiles over rows with a recorded latency, or `None` when no
    /// live relay has been recorded yet.
    pub latency: Option<LatencyStats>,
}

/// Nearest-rank percentile of a pre-sorted ascending slice (`p` in `0..=100`).
fn percentile(sorted: &[i64], p: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p as f64 / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Compute latency percentiles from raw millisecond samples (pure; testable).
///
/// Returns `None` for an empty sample set. Uses the nearest-rank method for
/// p50/p95, so results are always actual observed values.
pub fn latency_stats(values: &[i64]) -> Option<LatencyStats> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    Some(LatencyStats {
        count: v.len(),
        p50: percentile(&v, 50),
        p95: percentile(&v, 95),
        min: v[0],
        max: v[v.len() - 1],
    })
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
    /// Unix seconds when this row was first recorded (relay time). Drives the
    /// per-data-type refresh cadence (queued-polish §1) — post age = now -
    /// posted_at.
    pub posted_at: i64,
    /// Unix seconds of the last refresh check on this row. Advances on every
    /// evaluated tick (even a no-change one) so the cadence gate progresses.
    pub last_checked: i64,
    /// Whether the source message has ever been edited on Telegram. Persisted so
    /// a later stats-only PATCH keeps the edited (orange) stripe instead of
    /// reverting it to the regular color.
    pub edited: bool,
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
              edited INTEGER NOT NULL DEFAULT 0,
              latency_ms INTEGER,
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
        // Defensive migration for the already-deployed live db, which was created
        // before the latency_ms column existed. A no-op (ignored duplicate-column
        // error) once the column is present — including on brand-new dbs where the
        // CREATE above already added it. (queued-polish §3)
        Self::add_column_if_missing(&conn, "ALTER TABLE relayed ADD COLUMN latency_ms INTEGER")?;
        // `edited` persists the edited state so a later stats-only refresh PATCH
        // doesn't revert an edited post's stripe from orange back to purple.
        Self::add_column_if_missing(
            &conn,
            "ALTER TABLE relayed ADD COLUMN edited INTEGER NOT NULL DEFAULT 0",
        )?;
        Self::enable_incremental_auto_vacuum(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Run an `ALTER TABLE ... ADD COLUMN`, treating an already-present column as
    /// success (SQLite reports `duplicate column name`). Any other failure is an
    /// error.
    fn add_column_if_missing(conn: &Connection, alter_sql: &str) -> Result<()> {
        match conn.execute_batch(alter_sql) {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
            Err(e) => Err(e).with_context(|| format!("running migration: {alter_sql}")),
        }
    }

    /// Ensure the database uses INCREMENTAL auto-vacuum so pruned rows release
    /// file space (via [`prune`](Self::prune)'s `PRAGMA incremental_vacuum`).
    ///
    /// The live db already exists in the default `none` (0) mode, and setting
    /// `PRAGMA auto_vacuum=INCREMENTAL` does NOT convert an existing db on its
    /// own — a one-time `VACUUM` is required. So: read the current mode, and only
    /// when it is not already INCREMENTAL (2) set the pragma and VACUUM once to
    /// rewrite the file with auto-vacuum enabled. On a brand-new db the VACUUM is
    /// trivial. WAL is unaffected. (queued-polish §11a)
    fn enable_incremental_auto_vacuum(conn: &Connection) -> Result<()> {
        // 0 = none, 1 = full, 2 = incremental.
        let mode: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .context("reading auto_vacuum mode")?;
        if mode != 2 {
            conn.pragma_update(None, "auto_vacuum", 2)
                .context("setting auto_vacuum=INCREMENTAL")?;
            conn.execute_batch("VACUUM")
                .context("VACUUM to convert to incremental auto_vacuum")?;
        }
        Ok(())
    }

    /// Record a freshly-posted Discord message. Idempotent on the primary key.
    pub fn record(&self, rec: NewRecord) -> Result<()> {
        let now = now_secs();
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO relayed
              (chat_id, tg_msg_id, route, webhook_name, discord_msg_id,
               posted_at, last_checked, content_hash, reactions, comment_count,
               deleted, latency_ms)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, 0, ?10)
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
                rec.latency_ms,
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
                   content_hash, reactions, comment_count, posted_at, last_checked,
                   edited
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
                posted_at: r.get(8)?,
                last_checked: r.get(9)?,
                edited: r.get::<_, i64>(10)? != 0,
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

    /// Bump `last_checked` on one row without changing its stats.
    ///
    /// Called when the refresh worker fetched a row but found no change: the
    /// cadence gate (queued-polish §1) keys off `last_checked`, so it must
    /// advance on every evaluated tick, not only on ticks that PATCH something —
    /// otherwise a checkpoint would re-fire every base tick.
    pub fn touch_checked(&self, chat_id: i64, tg_msg_id: i32, discord_msg_id: &str) -> Result<()> {
        let now = now_secs();
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "UPDATE relayed SET last_checked = ?4 WHERE chat_id = ?1 AND tg_msg_id = ?2 AND discord_msg_id = ?3",
            rusqlite::params![chat_id, tg_msg_id, discord_msg_id, now],
        )
        .context("touching last_checked")?;
        Ok(())
    }

    /// Mark a Telegram post as edited (all its Discord messages), so later
    /// stats-only refreshes keep the edited stripe instead of reverting it.
    pub fn mark_edited(&self, chat_id: i64, tg_msg_id: i32) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "UPDATE relayed SET edited = 1 WHERE chat_id = ?1 AND tg_msg_id = ?2",
            rusqlite::params![chat_id, tg_msg_id],
        )
        .context("marking edited")?;
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
        // Release the pages the delete just freed back to the OS. A no-op when
        // nothing is reclaimable; requires INCREMENTAL auto_vacuum (set on open).
        conn.execute_batch("PRAGMA incremental_vacuum")
            .context("incremental_vacuum after prune")?;
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

    /// Aggregate counters + latency percentiles for the `stats` CLI verb.
    ///
    /// `tracked_posts` counts distinct Telegram posts (a post fanned out to N
    /// webhooks counts once); `deleted` counts rows tombstoned on Telegram.
    /// Latency percentiles cover only rows with a recorded `latency_ms` (live
    /// relays); backfilled rows have `NULL` and are excluded.
    pub fn stats(&self) -> Result<StoreStats> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let tracked_posts: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT chat_id || ':' || tg_msg_id) FROM relayed",
                [],
                |r| r.get(0),
            )
            .context("counting tracked posts")?;
        let deleted: i64 = conn
            .query_row("SELECT COUNT(*) FROM relayed WHERE deleted = 1", [], |r| {
                r.get(0)
            })
            .context("counting deleted rows")?;
        let mut stmt =
            conn.prepare("SELECT latency_ms FROM relayed WHERE latency_ms IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut values = Vec::new();
        for row in rows {
            values.push(row?);
        }
        Ok(StoreStats {
            tracked_posts,
            deleted,
            latency: latency_stats(&values),
        })
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
            latency_ms: None,
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
    fn mark_edited_persists_and_survives_stats_update() {
        let path = tmp_db("edited");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();

        store.record(rec(1, 1, "d-a")).unwrap();
        assert!(!store.due(48).unwrap()[0].edited, "starts un-edited");

        store.mark_edited(1, 1).unwrap();
        assert!(store.due(48).unwrap()[0].edited, "edited flag persisted");

        // A later stats-only update must NOT clear the edited flag (the bug:
        // reaction refresh reverting an edited post's stripe to purple).
        store
            .update_stats(1, 1, "d-a", "newhash", &BTreeMap::new(), 0)
            .unwrap();
        assert!(
            store.due(48).unwrap()[0].edited,
            "edited must survive a stats-only refresh"
        );

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
    fn due_carries_posted_at_and_last_checked() {
        let path = tmp_db("due-timing");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        store.record(rec(1, 1, "d-a")).unwrap();
        let due = store.due(48).unwrap();
        // Freshly recorded: posted_at == last_checked and both are set.
        assert!(due[0].posted_at > 0);
        assert_eq!(due[0].posted_at, due[0].last_checked);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn touch_checked_advances_last_checked_only() {
        let path = tmp_db("touch");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        store.record(rec(1, 1, "d-a")).unwrap();
        let before = store.due(48).unwrap().into_iter().next().unwrap();
        // Backdate posted_at + last_checked so the touch's `now` is strictly later.
        store.backdate_all(120);
        store.touch_checked(1, 1, "d-a").unwrap();
        let after = store.due(48).unwrap().into_iter().next().unwrap();
        assert!(
            after.last_checked > before.last_checked - 120,
            "last_checked should advance to ~now"
        );
        // Stats untouched.
        assert_eq!(after.content_hash, before.content_hash);
        assert_eq!(after.reactions, before.reactions);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_enables_incremental_auto_vacuum() {
        let path = tmp_db("autovac-mode");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let mode: i64 = {
            let conn = store.conn.lock().unwrap();
            conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(mode, 2, "auto_vacuum should be INCREMENTAL (2)");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_is_idempotent_on_already_incremental_db() {
        // Re-opening a db that is already INCREMENTAL must not error (the mode
        // check skips the one-time VACUUM the second time).
        let path = tmp_db("autovac-idem");
        let _ = std::fs::remove_file(&path);
        {
            let s = Store::open(&path).unwrap();
            s.record(rec(1, 1, "d-a")).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.count().unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_runs_incremental_vacuum_without_error() {
        let path = tmp_db("autovac-prune");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        for i in 0..20 {
            store.record(rec(1, i, &format!("d-{i}"))).unwrap();
        }
        // Age everything past the horizon, then prune (which incremental_vacuums).
        store.backdate_all(49 * 3600);
        let pruned = store.prune(48).unwrap();
        assert_eq!(pruned, 20);
        assert_eq!(store.count().unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reactions_json_roundtrip() {
        let m = map(&[("❤️", 47), ("🔥", 12)]);
        let s = reactions_to_json(&m);
        assert_eq!(reactions_from_json(&s), m);
        assert_eq!(reactions_from_json("garbage"), BTreeMap::new());
    }

    #[test]
    fn latency_stats_empty_is_none() {
        assert_eq!(latency_stats(&[]), None);
    }

    #[test]
    fn latency_stats_single_sample() {
        let s = latency_stats(&[240]).unwrap();
        assert_eq!(s.count, 1);
        assert_eq!((s.p50, s.p95, s.min, s.max), (240, 240, 240, 240));
    }

    #[test]
    fn latency_stats_percentiles_nearest_rank() {
        // 1..=100 ms: nearest-rank p50 = 50, p95 = 95, min 1, max 100.
        let vals: Vec<i64> = (1..=100).collect();
        let s = latency_stats(&vals).unwrap();
        assert_eq!(s.count, 100);
        assert_eq!(s.p50, 50);
        assert_eq!(s.p95, 95);
        assert_eq!(s.min, 1);
        assert_eq!(s.max, 100);
    }

    #[test]
    fn latency_stats_is_order_independent() {
        let a = latency_stats(&[300, 100, 200, 500, 400]).unwrap();
        let b = latency_stats(&[500, 400, 300, 200, 100]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.min, 100);
        assert_eq!(a.max, 500);
    }

    #[test]
    fn stats_counts_posts_deleted_and_latency() {
        let path = tmp_db("stats");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();

        // Two Discord rows for one post (100,5), one for (200,9): 2 distinct posts.
        let mut a = rec(100, 5, "d-a");
        a.latency_ms = Some(150);
        store.record(a).unwrap();
        let mut b = rec(100, 5, "d-b");
        b.latency_ms = Some(450);
        store.record(b).unwrap();
        store.record(rec(200, 9, "d-c")).unwrap(); // backfill-style: NULL latency

        store.mark_deleted(200, 9).unwrap();

        let s = store.stats().unwrap();
        assert_eq!(s.tracked_posts, 2, "distinct (chat,tg) posts");
        assert_eq!(s.deleted, 1);
        let lat = s.latency.expect("two live-relay latencies recorded");
        assert_eq!(lat.count, 2);
        assert_eq!(lat.min, 150);
        assert_eq!(lat.max, 450);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_latency_none_when_no_live_relays() {
        let path = tmp_db("stats-nolat");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        store.record(rec(1, 1, "d-a")).unwrap(); // NULL latency
        let s = store.stats().unwrap();
        assert_eq!(s.tracked_posts, 1);
        assert!(s.latency.is_none());
        let _ = std::fs::remove_file(&path);
    }
}
