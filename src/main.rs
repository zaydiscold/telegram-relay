//! telegram-relay daemon entrypoint + CLI dispatch.
//!
//! Wires everything built so far into a running relay:
//! `run` connects to Telegram, resolves routes, and runs a `tokio::select!` loop
//! over the update stream + SIGTERM/SIGINT + a heartbeat + a media-flush tick +
//! a config hot-reload tick. Each update is classified → deduped → routed →
//! filtered → dispatched (text rendered+posted, media downloaded+coalesced).
//!
//! See `docs/superpowers/plans/api-notes.md` for the grammers 0.10.0 API shape.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context};
use arc_swap::ArcSwap;
use clap::Parser;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use telegram_relay::cli::{clamp_backfill_count, Cli, Command, SESSION_FILE};
use telegram_relay::config::{
    effective_media_mode, Config, MediaCfg, MediaMode, WebhookName, WebhookUrl,
};
use telegram_relay::dedup::Dedup;
use telegram_relay::deliver::{Deliverer, PostResult};
use telegram_relay::media::{sort_album_batch, AlbumBuffer, MediaItem, MediaTarget};
use telegram_relay::refresh::{self, content_hash, GrammersFetcher};
use telegram_relay::render::{self, passes_filter, EmbedMeta, RelayText};
use telegram_relay::router::{ResolvedRoute, Router};
use telegram_relay::store::{NewRecord, Store};
use telegram_relay::telegram::{self, Incoming};

use base64::Engine;
use grammers_client::media::Media;
use grammers_client::Client;

use telegram_relay::deliver::Outcome;
use telegram_relay::telegram::ChannelIdentity;

/// Heartbeat interval — an `info!` line proving the loop is alive.
const HEARTBEAT: Duration = Duration::from_secs(300);
/// How often the media buffer is polled for expired albums.
const MEDIA_TICK: Duration = Duration::from_millis(250);
/// How often config.yaml's mtime is checked for hot-reload.
const RELOAD_TICK: Duration = Duration::from_secs(5);
/// Album coalescing quiet window.
const ALBUM_WINDOW: Duration = Duration::from_secs(1);
/// Dedup LRU capacity.
const DEDUP_CAP: usize = 8192;
/// Capacity of the channel carrying downloaded media items back to the loop.
const MEDIA_CHANNEL_CAP: usize = 64;
/// After this much silence with no incoming update, actively probe Telegram to
/// prove the connection is alive (grammers' own idle timeout is ~15 min, far too
/// long for a relay). The probe also detects a revoked session and forces a
/// reconnect if the socket dropped.
const LIVENESS_PROBE: Duration = Duration::from_secs(120);
/// Max media downloads in flight at once. Each buffers a whole file in memory, so
/// this caps peak media memory at ~= permits * media.max_bytes and stops a burst
/// of posts (or a catch-up backlog) from OOM-ing the host.
const MAX_CONCURRENT_DOWNLOADS: usize = 6;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run { config } => run(&config).await,
        Command::Login { config } => login(&config).await,
        Command::Chats { config } => chats(&config).await,
        Command::Check { config } => check(&config).await,
        Command::Stats { config } => stats_cmd(&config),
        Command::Routes { config } => routes_cmd(&config),
        Command::Backfill {
            config,
            route,
            count,
        } => backfill(&config, &route, count).await,
    }
}

/// Read `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` from the environment.
fn api_credentials() -> anyhow::Result<(i32, String)> {
    let api_id = std::env::var("TELEGRAM_API_ID")
        .context("TELEGRAM_API_ID not set")?
        .parse::<i32>()
        .context("TELEGRAM_API_ID must be an integer")?;
    let api_hash = std::env::var("TELEGRAM_API_HASH").context("TELEGRAM_API_HASH not set")?;
    Ok((api_id, api_hash))
}

// ---------------------------------------------------------------------------
// login / chats
// ---------------------------------------------------------------------------

async fn login(_config: &Path) -> anyhow::Result<()> {
    let (api_id, api_hash) = api_credentials()?;
    let conn = telegram::connect(api_id, Path::new(SESSION_FILE)).await?;
    let res = telegram::interactive_login(&conn.client, &api_hash).await;
    conn.handle.quit();
    let _ = conn.pool_task.await;
    res
}

async fn chats(_config: &Path) -> anyhow::Result<()> {
    let (api_id, _) = api_credentials()?;
    let conn = telegram::connect(api_id, Path::new(SESSION_FILE)).await?;
    if !conn.client.is_authorized().await? {
        conn.handle.quit();
        let _ = conn.pool_task.await;
        return Err(anyhow!("not authorized; run `telegram-relay login` first"));
    }
    let res = telegram::list_chats(&conn.client).await;
    conn.handle.quit();
    let _ = conn.pool_task.await;
    res
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// Validate config + webhook reachability (+ route resolution if a session
/// exists). Prints a table and exits with code 0 (all good) or 1 (any failure).
/// Never sends a message — GET on a Discord webhook returns its JSON metadata.
async fn check(config_path: &Path) -> anyhow::Result<()> {
    let cfg = Config::load(config_path).context("loading config")?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let mut all_ok = true;
    println!("{:<24}  {:<8}  detail", "webhook", "status");
    println!("{}", "-".repeat(60));

    // Named webhooks + optional ops webhook.
    let mut targets: Vec<(String, WebhookUrl)> = cfg
        .webhooks
        .iter()
        .map(|(n, u)| (n.0.clone(), u.clone()))
        .collect();
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(ops) = &cfg.ops_webhook {
        targets.push(("ops_webhook".to_string(), ops.clone()));
    }

    for (name, url) in &targets {
        match http.get(&url.0).send().await {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.unwrap_or_default();
                let detail = webhook_label(&body);
                println!("{name:<24}  {:<8}  {detail}", "ok");
            }
            Ok(r) => {
                all_ok = false;
                println!("{name:<24}  {:<8}  HTTP {}", "FAIL", r.status().as_u16());
            }
            Err(e) => {
                all_ok = false;
                // Print the webhook NAME (already the first column) and a
                // URL-stripped error — never the URL, which carries the token.
                println!("{name:<24}  {:<8}  {}", "FAIL", e.without_url());
            }
        }
    }

    // Route resolution — only if a session file already exists.
    if Path::new(SESSION_FILE).exists() {
        match api_credentials() {
            Ok((api_id, _)) => match telegram::connect(api_id, Path::new(SESSION_FILE)).await {
                Ok(conn) => {
                    if conn.client.is_authorized().await.unwrap_or(false) {
                        println!("\n{:<24}  {:<8}  detail", "route", "status");
                        println!("{}", "-".repeat(60));
                        match telegram::resolve_routes(&conn.client, &cfg).await {
                            Ok(res) => {
                                for r in &res.routes {
                                    println!("{:<24}  {:<8}  chat {}", r.name, "ok", r.chat.0);
                                }
                                for f in &res.failures {
                                    all_ok = false;
                                    println!("{:<24}  {:<8}  {f}", "(unresolved)", "FAIL");
                                }
                            }
                            Err(e) => {
                                all_ok = false;
                                println!("{:<24}  {:<8}  {e}", "(resolution)", "FAIL");
                            }
                        }
                    } else {
                        println!("\nsession present but not authorized; skipping route check");
                    }
                    conn.handle.quit();
                    let _ = conn.pool_task.await;
                }
                Err(e) => {
                    all_ok = false;
                    println!("\nconnect failed: {e}");
                }
            },
            Err(e) => {
                println!("\nsession present but credentials missing; skipping route check: {e}");
            }
        }
    } else {
        println!("\nno session file ({SESSION_FILE}); skipping route resolution");
    }

    if all_ok {
        println!("\nOK");
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// Extract a friendly label from a Discord webhook GET response body.
fn webhook_label(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let channel = v.get("channel_id").and_then(|c| c.as_str()).unwrap_or("?");
            format!("\"{name}\" (channel {channel})")
        }
        Err(_) => "reachable".to_string(),
    }
}

// ---------------------------------------------------------------------------
// stats — store counters + relay latency percentiles
// ---------------------------------------------------------------------------

/// `telegram-relay stats`: open the store and print tracked-post / deleted
/// counts plus latency p50/p95/min/max over recorded LIVE relays. No network;
/// always exits 0. Latency is `NULL` for backfilled rows, so those are excluded.
fn stats_cmd(config_path: &Path) -> anyhow::Result<()> {
    let cfg = Config::load(config_path).context("loading config")?;
    let store = Store::open(&cfg.store.path).context("opening message store")?;
    let s = store.stats().context("reading store stats")?;

    const W: usize = 18;
    println!("{:<W$}{}", "tracked posts", s.tracked_posts);
    println!("{:<W$}{}", "deleted", s.deleted);
    match s.latency {
        Some(l) => {
            println!("{:<W$}{}", "latency samples", l.count);
            println!("{:<W$}{} ms", "latency p50", l.p50);
            println!("{:<W$}{} ms", "latency p95", l.p95);
            println!("{:<W$}{} ms", "latency min", l.min);
            println!("{:<W$}{} ms", "latency max", l.max);
        }
        None => println!("{:<W$}(no live relays recorded yet)", "latency"),
    }
    Ok(())
}

#[cfg(test)]
mod routes_diagram_tests {
    use super::render_routes_ascii;
    use std::collections::HashMap;
    use telegram_relay::config::{ChatId, ChatRef, RouteCfg, WebhookName};
    use telegram_relay::render::DEFAULT_EMBED_COLOR;

    fn route(label: Option<&str>, from: ChatRef, to: &[&str]) -> RouteCfg {
        RouteCfg {
            name: "r".into(),
            label: label.map(str::to_string),
            from,
            to: to.iter().map(|w| WebhookName(w.to_string())).collect(),
            filter: None,
            color: DEFAULT_EMBED_COLOR,
            media_mode: None,
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> HashMap<WebhookName, String> {
        pairs
            .iter()
            .map(|(k, v)| (WebhookName(k.to_string()), v.to_string()))
            .collect()
    }

    #[test]
    fn diagram_shows_both_directions_with_friendly_labels() {
        let routes = vec![
            route(
                Some("Alpha"),
                ChatRef::Username("alpha_news".into()),
                &["crypto", "hub"],
            ),
            route(
                Some("Beta"),
                ChatRef::Username("beta_wire".into()),
                &["news", "hub"],
            ),
        ];
        let lbls = labels(&[
            ("crypto", "server · #crypto"),
            ("hub", "HUB"),
            ("news", "server · #news"),
        ]);
        let out = render_routes_ascii(&routes, &lbls);
        assert!(out.contains("2 source(s) → 3 destination(s)"), "{out}");
        // both directional views present
        assert!(out.contains("by source"));
        assert!(out.contains("by destination"));
        // friendly source ("Alpha (@alpha_news)") and destination labels render
        assert!(out.contains("Alpha (@alpha_news)"), "{out}");
        assert!(out.contains("server · #crypto"));
        assert!(out.contains("HUB"));
        // both arrows
        assert!(out.contains("──▶") && out.contains("◀──"));
        // the shared hub takes both sources -> flagged as the firehose
        assert!(out.contains("(all sources)"), "{out}");
    }

    #[test]
    fn diagram_falls_back_to_handle_when_unlabeled() {
        let routes = vec![
            route(None, ChatRef::Username("chan_a".into()), &["main"]),
            route(None, ChatRef::Id(ChatId(-100)), &["main"]),
        ];
        // no webhook label -> the raw key "main" is used as the destination name
        let out = render_routes_ascii(&routes, &labels(&[("main", "main")]));
        assert!(out.contains("@chan_a")); // username rendered with @
        assert!(out.contains("-100")); // numeric id as-is
        assert!(out.contains("(all sources)")); // both feed the one destination
    }
}

// ---------------------------------------------------------------------------
// routes — ASCII wiring diagram (no network)
// ---------------------------------------------------------------------------

/// Render a `ChatRef` the way a user wrote it in config: `@name` or a raw id.
fn source_label(from: &telegram_relay::config::ChatRef) -> String {
    use telegram_relay::config::ChatRef;
    match from {
        ChatRef::Username(u) => format!("@{u}"),
        ChatRef::Id(id) => id.0.to_string(),
    }
}

/// Draw the routing table two ways so the wiring is unambiguous:
///   * **by source** — every telegram channel and where it fans out to
///   * **by destination** — every discord channel and which sources fan into it
///
/// Both use friendly names when the config sets them (a route `label` like
/// "Rob", a webhook `label` like "lock-in · #crypto"), falling back to the raw
/// `@handle` / webhook key. Pure so it can be unit-tested; `routes_cmd` prints
/// it. Never receives a URL — only labels — so it is safe to log.
fn render_routes_ascii(
    routes: &[telegram_relay::config::RouteCfg],
    webhook_labels: &std::collections::HashMap<telegram_relay::config::WebhookName, String>,
) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    use telegram_relay::config::WebhookName;

    // A webhook's friendly destination name: its `label`, else the config key.
    let hook_label = |w: &WebhookName| -> String {
        webhook_labels
            .get(w)
            .cloned()
            .unwrap_or_else(|| w.0.clone())
    };
    // Short source name for the inverse view: route `label` ("Rob"), else @handle.
    let src_short = |r: &telegram_relay::config::RouteCfg| {
        r.label.clone().unwrap_or_else(|| source_label(&r.from))
    };
    // Long source name for the forward view: "News (@some_channel)" when labeled.
    let src_long = |r: &telegram_relay::config::RouteCfg| match &r.label {
        Some(l) => format!("{l} ({})", source_label(&r.from)),
        None => source_label(&r.from),
    };

    // Distinct source channels — two routes on one @handle count once.
    let distinct_sources: BTreeSet<String> = routes.iter().map(|r| source_label(&r.from)).collect();

    // by destination is keyed by the UNIQUE webhook key (w.0), never the display
    // label — two webhooks that happen to share a label must not collapse into
    // one row. Value = (display label, the distinct sources feeding it).
    let mut by_hook: BTreeMap<&str, (String, BTreeSet<String>)> = BTreeMap::new();
    for r in routes {
        for w in &r.to {
            let entry = by_hook
                .entry(w.0.as_str())
                .or_insert_with(|| (hook_label(w), BTreeSet::new()));
            entry.1.insert(src_short(r));
        }
    }

    let mut out = String::new();
    out.push_str(&format!(
        "telegram-relay — routing ({} source(s) → {} destination(s))\n\n",
        distinct_sources.len(),
        by_hook.len(),
    ));

    // ---- by source: each telegram channel and where it goes (fan-out) ----
    out.push_str("by source — where each telegram channel goes:\n");
    let swidth = routes
        .iter()
        .map(|r| src_long(r).chars().count())
        .max()
        .unwrap_or(0);
    for r in routes {
        let dests: Vec<String> = r.to.iter().map(&hook_label).collect();
        out.push_str(&format!(
            "  {:<swidth$}  ──▶  {}\n",
            src_long(r),
            dests.join(" · ")
        ));
    }

    // ---- by destination: each discord channel and who feeds it (fan-in) ----
    out.push_str("\nby destination — what each discord channel receives:\n");
    let dwidth = by_hook
        .values()
        .map(|(lbl, _)| lbl.chars().count())
        .max()
        .unwrap_or(0);
    let total_src = distinct_sources.len();
    for (lbl, srcs) in by_hook.values() {
        // Flag a channel that takes the whole firehose — every distinct source.
        let note = if srcs.len() > 1 && srcs.len() == total_src {
            "   (all sources)"
        } else {
            ""
        };
        let joined = srcs.iter().cloned().collect::<Vec<_>>().join(" · ");
        out.push_str(&format!("  {lbl:<dwidth$}  ◀──  {joined}{note}\n"));
    }
    out
}

/// `routes` verb: print the wiring diagram from config alone (no session).
fn routes_cmd(config_path: &Path) -> anyhow::Result<()> {
    let cfg = Config::load(config_path).context("loading config")?;
    print!("{}", render_routes_ascii(&cfg.routes, &cfg.webhook_labels));
    Ok(())
}

// ---------------------------------------------------------------------------
// backfill — relay recent channel history on demand
// ---------------------------------------------------------------------------

/// Reverse grammers' newest-to-oldest `iter_messages` order into
/// oldest-to-newest, so backfilled posts land in the same chronological order
/// live traffic would have produced them.
fn oldest_first<T>(mut v: Vec<T>) -> Vec<T> {
    v.reverse();
    v
}

/// `telegram-relay backfill <route> [--count N]`.
///
/// Loads config, connects, resolves ONLY the named route (erroring loudly if
/// it doesn't exist), fetches its `count` most recent messages, and relays
/// them oldest -> newest through the same filter -> embed -> post -> record
/// pipeline `run` uses for live updates — so the refresh worker picks the
/// resulting posts up on its next tick same as anything relayed live.
///
/// Differences from the live path, by design (see module docs / spec):
/// * Sequential, not fire-and-forget: this is a one-shot CLI call, so posts
///   are awaited in order rather than spawned, which is what keeps them
///   oldest -> newest on the Discord side.
/// * Media messages relay caption/text + deep link only; the download lane is
///   skipped entirely and a "media omitted (backfill)" line is appended.
/// * Dedup against the store: a (chat, msg, webhook) triple already recorded
///   (e.g. relayed live, or by an earlier backfill run) is skipped, not
///   double-posted.
async fn backfill(
    config_path: &Path,
    route_name: &str,
    requested_count: usize,
) -> anyhow::Result<()> {
    let count = clamp_backfill_count(requested_count);
    let (api_id, _api_hash) = api_credentials()?;
    let cfg = Config::load(config_path).context("loading config")?;

    let route = cfg
        .routes
        .iter()
        .find(|r| r.name == route_name)
        .ok_or_else(|| {
            let available: Vec<&str> = cfg.routes.iter().map(|r| r.name.as_str()).collect();
            anyhow!(
                "route '{route_name}' not found in {}; available route(s): {}",
                config_path.display(),
                if available.is_empty() {
                    "(none configured)".to_string()
                } else {
                    available.join(", ")
                }
            )
        })?;

    let mut targets: Vec<(WebhookName, WebhookUrl)> = Vec::new();
    for name in &route.to {
        match cfg.webhooks.get(name) {
            Some(url) => targets.push((name.clone(), url.clone())),
            None => {
                warn!(webhook = %name.0, route = %route_name, "route references unknown webhook")
            }
        }
    }
    if targets.is_empty() {
        return Err(anyhow!(
            "route '{route_name}' has no resolvable webhook targets; check config.yaml"
        ));
    }

    let deliverer = Deliverer::new();

    // Connect BEFORE opening the store: grammers-session (libsql) must call
    // sqlite3_config() before rusqlite initializes the linked-in SQLite, or it
    // panics with SQLITE_MISUSE. Same load-bearing ordering as `run`.
    let conn = telegram::connect(api_id, Path::new(SESSION_FILE)).await?;
    if !conn.client.is_authorized().await? {
        conn.handle.quit();
        let _ = conn.pool_task.await;
        return Err(anyhow!("not authorized; run `telegram-relay login` first"));
    }

    let store = Store::open(&cfg.store.path).context("opening message store")?;

    let resolved = match telegram::resolve_chat_peer(&conn.client, &route.from).await {
        Ok(r) => r,
        Err(e) => {
            conn.handle.quit();
            let _ = conn.pool_task.await;
            return Err(e.context(format!("resolving route '{route_name}' for backfill")));
        }
    };
    info!(
        route = route_name,
        chat = resolved.chat.0,
        title = %resolved.title,
        count,
        "backfill: fetching recent messages"
    );

    let mut messages = Vec::with_capacity(count);
    {
        let mut iter = conn.client.iter_messages(resolved.peer).limit(count);
        loop {
            match iter.next().await {
                Ok(Some(msg)) => messages.push(msg),
                Ok(None) => break,
                Err(e) => {
                    conn.handle.quit();
                    let _ = conn.pool_task.await;
                    return Err(anyhow!("fetching message history for '{route_name}': {e}"));
                }
            }
        }
    }
    let fetched_total = messages.len();
    let messages = oldest_first(messages);

    let mut posted = 0usize;
    let mut skipped_filter = 0usize;
    let mut skipped_dupe = 0usize;
    let mut dropped = 0usize;

    let mut skipped_empty = 0usize;

    // Effective media mode for this one route (route override or global).
    let route_mode = effective_media_mode(route.media_mode, cfg.media.mode);
    // Fan-out targets for the coalesced-media path (backfill has one route).
    let media_targets: Vec<MediaTarget> = targets
        .iter()
        .map(|(name, url)| MediaTarget {
            route: route.name.clone(),
            webhook: name.clone(),
            url: url.clone(),
            color: route.color,
            mode: route_mode,
        })
        .collect();
    let placeholder = route_mode == MediaMode::Placeholder;

    // Coalesce consecutive album siblings (same grouped_id) into one group; any
    // other message is its own group. `messages` is already oldest -> newest, so
    // an album's siblings sit next to each other.
    let mut groups: Vec<Vec<&grammers_client::message::Message>> = Vec::new();
    for msg in &messages {
        match msg.grouped_id() {
            Some(gid) => match groups.last_mut() {
                Some(last) if last[0].grouped_id() == Some(gid) => last.push(msg),
                _ => groups.push(vec![msg]),
            },
            None => groups.push(vec![msg]),
        }
    }

    for group in &groups {
        let anchor = group[0];

        // Skip groups with nothing relayable (service/empty messages).
        if group
            .iter()
            .all(|m| matches!(telegram::route_message(m), telegram::Routing::Skip))
        {
            info!(
                msg_id = anchor.id(),
                "backfill: skipping group with no relayable content"
            );
            skipped_empty += group.len();
            continue;
        }

        // Caption-bearing member (first with non-empty text), else the anchor.
        // Its msg_id keys the store row + content hash, matching the refresh
        // worker which re-fetches that id.
        let cap = group
            .iter()
            .copied()
            .find(|m| !m.text().trim().is_empty())
            .unwrap_or(anchor);
        let anchor_msg_id = cap.id();
        let fetched = refresh::to_fetched(cap);
        let raw_caption = fetched.body.clone();
        let title = if fetched.title.is_empty() {
            route.name.clone()
        } else {
            fetched.title.clone()
        };
        let sender = cap.sender().and_then(|p| p.name()).map(|s| s.to_string());

        // Filter on the caption/body (same text for a media caption or a text
        // body, so one check covers both).
        if let Some(f) = &route.filter {
            if !passes_filter(&raw_caption, f) {
                skipped_filter += group.len();
                continue;
            }
        }

        // Downloadable file members of the group (photo/document/sticker).
        let file_members: Vec<&grammers_client::message::Message> = group
            .iter()
            .copied()
            .filter(|m| matches!(telegram::route_message(m), telegram::Routing::File))
            .collect();

        // Real media (reupload mode): download each non-oversized file and post
        // ONE coalesced rich embed — identical rendering + store tracking to the
        // live media path. Stats captured now (backfilled posts already have
        // reactions); refresh keeps them updated.
        if !file_members.is_empty() && !placeholder {
            let mut files: Vec<(String, Vec<u8>)> = Vec::new();
            for m in &file_members {
                let media = m
                    .media()
                    .expect("File routing implies message.media() is Some");
                if media.size().unwrap_or(0) as u64 > cfg.media.max_bytes {
                    continue; // oversized: falls back to a deep-link notice below
                }
                match download_media(&conn.client, &media, cfg.media.max_bytes).await {
                    Ok(bytes) => files.push((media_filename(&media, m.id()), bytes)),
                    Err(e) => {
                        warn!(error = %e, msg_id = m.id(), "backfill: media download failed")
                    }
                }
            }
            if !files.is_empty() {
                let (p, d, s) = deliver_coalesced_media(
                    &deliverer,
                    &store,
                    None,
                    resolved.chat.0,
                    anchor_msg_id,
                    &title,
                    &raw_caption,
                    fetched.deep_link.as_deref(),
                    sender.as_deref(),
                    &fetched.reactions,
                    fetched.comment_count.unwrap_or(0),
                    fetched.date.as_deref(),
                    &files,
                    false, // backfill downloads only non-oversized files
                    &media_targets,
                    None, // no contract passthrough on catch-up (would re-trigger bots)
                )
                .await;
                posted += p;
                dropped += d;
                skipped_dupe += s;
                continue;
            }
            // else: every file oversized/failed -> deep-link notice fallback.
        }

        // Text / notice path: a plain text or link message, non-file media
        // (poll/geo/…), unparsed media, placeholder-mode media, or the
        // oversized-media fallback. Build the body accordingly.
        let body = if !file_members.is_empty() {
            let notice = if placeholder {
                "[media]"
            } else {
                "[media too large to relay]"
            };
            if raw_caption.trim().is_empty() {
                notice.to_string()
            } else {
                format!("{raw_caption}\n{notice}")
            }
        } else {
            match telegram::route_message(cap) {
                telegram::Routing::Text(b) => b,
                // File is handled above; Skip cannot reach here (checked above).
                _ => raw_caption.clone(),
            }
        };

        let username = display_name(sender.as_deref(), &route.name);
        let text = RelayText {
            sender: sender.clone(),
            body,
            reply_quote: None,
            edited: false,
        };
        let meta = EmbedMeta {
            title,
            avatar_url: None,
            deep_link: fetched.deep_link.clone(),
            reactions: fetched.reactions.clone(),
            comment_count: fetched.comment_count.unwrap_or(0),
            deleted: false,
            color: route.color,
            timestamp: fetched.date.clone(),
        };
        let embeds = render::embed(&text, &meta);
        // Hash the RAW caption so the refresh worker (which hashes msg.text())
        // does not see a spurious edit on its next tick.
        let hash = content_hash(&raw_caption);

        for (name, url) in &targets {
            match store.already_relayed(resolved.chat.0, anchor_msg_id, &name.0) {
                Ok(true) => {
                    info!(
                        chat = resolved.chat.0,
                        msg_id = anchor_msg_id,
                        webhook = %name.0,
                        "backfill: already relayed; skipping"
                    );
                    skipped_dupe += 1;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(error = %e, chat = resolved.chat.0, msg_id = anchor_msg_id, "backfill: dedup check failed; posting anyway");
                }
            }

            match deliverer.post_embed(url, &username, &embeds, None).await {
                PostResult::Delivered { discord_msg_id, .. } => {
                    let record = NewRecord {
                        chat_id: resolved.chat.0,
                        tg_msg_id: anchor_msg_id,
                        route: route.name.clone(),
                        webhook_name: name.0.clone(),
                        discord_msg_id,
                        content_hash: hash.clone(),
                        reactions: fetched.reactions.clone(),
                        comment_count: fetched.comment_count.unwrap_or(0),
                        latency_ms: None,       // backfill latency is meaningless
                        image_urls: Vec::new(), // text post: no attachments
                    };
                    if let Err(e) = store.record(record) {
                        warn!(error = %e, "backfill: failed to record relayed message");
                    }
                    posted += 1;
                }
                PostResult::Dropped { reason } => {
                    warn!(%reason, chat = resolved.chat.0, msg_id = anchor_msg_id, webhook = %name.0, "backfill: delivery dropped");
                    dropped += 1;
                }
            }
        }
    }

    conn.handle.quit();
    let _ = conn.pool_task.await;

    info!(
        fetched = fetched_total,
        posted, skipped_empty, skipped_filter, skipped_dupe, dropped, "backfill complete"
    );
    println!(
        "backfill '{route_name}': {fetched_total} fetched, {posted} posted, \
         {skipped_empty} skipped (no content), {skipped_filter} skipped (filter), \
         {skipped_dupe} skipped (already relayed), {dropped} dropped"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// run — the daemon
// ---------------------------------------------------------------------------

/// Hot-swappable routing snapshot.
struct Live {
    router: Router,
    webhooks: HashMap<WebhookName, WebhookUrl>,
    media: MediaCfg,
}

impl Live {
    fn build(routes: Vec<ResolvedRoute>, cfg: &Config) -> Self {
        Live {
            router: Router::new(routes),
            webhooks: cfg.webhooks.clone(),
            media: cfg.media.clone(),
        }
    }
}

/// Redact any Discord webhook token from a string before it is printed or
/// posted to the ops channel: `.../webhooks/{id}/{token}` becomes
/// `.../webhooks/{id}/«redacted»`. Defense-in-depth alongside deliver.rs's
/// URL-stripping so no drop reason can ever egress a token, even if a future
/// error string reintroduces the URL.
fn scrub_webhook_tokens(s: &str) -> String {
    const MARK: &str = "/webhooks/";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(MARK) {
        out.push_str(&rest[..pos + MARK.len()]);
        rest = &rest[pos + MARK.len()..];
        // Webhook id = the leading digit run.
        let id_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        out.push_str(&rest[..id_end]);
        rest = &rest[id_end..];
        // A '/' then the token: redact the (non-whitespace) token run.
        if let Some(after) = rest.strip_prefix('/') {
            out.push_str("/«redacted»");
            let tok_end = after.find(char::is_whitespace).unwrap_or(after.len());
            rest = &after[tok_end..];
        }
    }
    out.push_str(rest);
    out
}

/// Ops-webhook notifier. Lifecycle/error notices go out unconditionally; drop
/// notices are rate-limited to at most one per minute to avoid flooding.
#[derive(Clone)]
struct Ops {
    deliverer: Arc<Deliverer>,
    url: Option<WebhookUrl>,
    last_drop: Arc<Mutex<Option<Instant>>>,
}

impl Ops {
    fn new(deliverer: Arc<Deliverer>, url: Option<WebhookUrl>) -> Self {
        Ops {
            deliverer,
            url,
            last_drop: Arc::new(Mutex::new(None)),
        }
    }

    /// Unconditional notice (startup, shutdown, resolution failures).
    ///
    /// The message is scrubbed of any webhook token before egress: even though
    /// deliver.rs already strips URLs from reqwest errors, this guarantees no
    /// drop reason can ever leak a token to the ops Discord channel.
    async fn notice(&self, msg: &str) {
        if let Some(url) = &self.url {
            let _ = self
                .deliverer
                .post_text(url, "relay-ops", &[scrub_webhook_tokens(msg)])
                .await;
        }
    }

    /// Drop notice, rate-limited to 1/min.
    async fn drop_notice(&self, msg: &str) {
        if self.url.is_none() {
            return;
        }
        {
            let mut guard = self.last_drop.lock().await;
            let now = Instant::now();
            let allow = guard.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(60));
            if !allow {
                return;
            }
            *guard = Some(now);
        }
        self.notice(msg).await;
    }
}

/// True when a Telegram RPC error means our session is no longer valid — a
/// remote logout, ban, or deactivation. Unlike a transient network error,
/// restarting won't fix it (re-login required), so the relay must alert and stop
/// rather than loop uselessly.
fn is_auth_error(e: &grammers_client::InvocationError) -> bool {
    matches!(
        e,
        grammers_client::InvocationError::Rpc(rpc)
            if rpc.code == 401
                || rpc.name.contains("AUTH_KEY_UNREGISTERED")
                || rpc.name.contains("SESSION_REVOKED")
                || rpc.name.contains("USER_DEACTIVATED")
                || rpc.name.contains("AUTH_KEY_DUPLICATED")
    )
}

async fn run(config_path: &Path) -> anyhow::Result<()> {
    let (api_id, _api_hash) = api_credentials()?;
    let cfg = Config::load(config_path).context("loading config")?;

    let deliverer = Arc::new(Deliverer::new());
    let ops = Ops::new(deliverer.clone(), cfg.ops_webhook.clone());

    // Telegram must connect BEFORE the store opens: grammers-session (libsql)
    // calls sqlite3_config(), which fails with SQLITE_MISUSE if rusqlite has
    // already initialized the linked-in SQLite. Order is load-bearing.
    let mut conn = telegram::connect(api_id, Path::new(SESSION_FILE)).await?;

    let store = Arc::new(Store::open(&cfg.store.path).context("opening message store")?);
    info!("message store at {}", cfg.store.path.display());

    if !conn.client.is_authorized().await? {
        conn.handle.quit();
        let _ = conn.pool_task.await;
        return Err(anyhow!("not authorized; run `telegram-relay login` first"));
    }

    // Initial route resolution — fatal only if EVERY route fails (nothing to
    // route). A partial failure skips the bad routes and relays the rest.
    let resolved = match telegram::resolve_routes(&conn.client, &cfg).await {
        Ok(r) => {
            for f in &r.failures {
                warn!("skipping unresolved {f}");
                ops.notice(&format!("route skipped (unresolved): {f}"))
                    .await;
            }
            r
        }
        Err(e) => {
            ops.notice(&format!("startup route resolution failed: {e}"))
                .await;
            conn.handle.quit();
            let _ = conn.pool_task.await;
            return Err(e);
        }
    };
    // Best-effort: mirror each source channel's title + photo onto its Discord
    // webhook's persistent name + avatar. Runs once, after resolution, before
    // the loop starts. Never fatal — a failure here must not stop relaying.
    sync_webhook_identities(
        &deliverer,
        &store,
        &cfg.webhooks,
        &resolved.routes,
        &resolved.identities,
    )
    .await;

    // Peer refs for the refresh worker (keyed by bot-API chat id).
    let peer_refs = resolved
        .peers
        .iter()
        .map(|(chat, pref)| (chat.0, *pref))
        .collect::<HashMap<_, _>>();
    let live = Arc::new(ArcSwap::from_pointee(Live::build(resolved.routes, &cfg)));

    // Spawn the refresh worker: re-fetch tracked posts on an interval, PATCH
    // embeds whose reactions/comments/body changed, mark deletes, then prune.
    // It holds a fixed peer map + webhook snapshot; routes added at runtime are
    // refreshed only after a restart.
    {
        let fetcher = GrammersFetcher::new(conn.client.clone(), peer_refs);
        let store = store.clone();
        let deliverer = deliverer.clone();
        let webhooks = cfg.webhooks.clone();
        // Route -> embed color, so a PATCH re-declares the stripe instead of
        // dropping it to Discord's gray. Snapshotted with the webhook map, so a
        // color changed at runtime applies to refreshes only after a restart.
        let colors: refresh::RouteColors = cfg
            .routes
            .iter()
            .map(|r| (r.name.clone(), r.color))
            .collect();
        let refresh_cfg = cfg.refresh;
        tokio::spawn(async move {
            refresh::run(fetcher, store, deliverer, webhooks, colors, refresh_cfg).await;
        });
        info!(
            interval_mins = cfg.refresh.interval_mins,
            horizon_hours = cfg.refresh.horizon_hours,
            reaction_horizon_mins = cfg.refresh.reaction_horizon_mins,
            reaction_early_check_secs = cfg.refresh.reaction_early_check_secs,
            "refresh worker started"
        );
    }

    let mut updates = telegram::stream_updates(&conn.client, conn.updates).await?;
    let mut dedup = Dedup::new(DEDUP_CAP);
    let mut album = AlbumBuffer::new(ALBUM_WINDOW);

    // Downloads run on spawned tasks and hand finished items back to the loop
    // over this channel; `album` stays single-owned by the loop.
    let (media_tx, mut media_rx) = mpsc::channel::<MediaItem>(MEDIA_CHANNEL_CAP);
    // Bounds concurrent media downloads so a burst can't OOM the host.
    let download_sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
    // Captured once: toggling contract_passthrough takes effect on restart.
    let contract_passthrough = cfg.contract_passthrough;

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut media_tick = tokio::time::interval(MEDIA_TICK);
    let mut reload_tick = tokio::time::interval(RELOAD_TICK);
    let mut last_mtime = file_mtime(config_path);
    let mut liveness_tick = tokio::time::interval(LIVENESS_PROBE);
    liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // When an update last arrived, so the liveness probe fires only during
    // silence; and consecutive probe failures, so a transient blip is tolerated
    // but a sustained outage escalates to a restart.
    let mut last_update = Instant::now();
    let mut probe_failures: u32 = 0;

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;

    // Quiet ops (queued-polish §13): NO routine lifecycle post on start —
    // those fired on every deploy/restart and were pure noise. Silence is
    // healthy; the systemd OnFailure watchdog covers "down", and the feed
    // flowing (plus `stats`) answers "is it up". Only error/drop notices and
    // route-resolution failures still post. Startup is logged locally only.
    info!("relay started; watching {} route(s)", cfg.routes.len());

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { info!("SIGINT received"); break; }
            _ = sigterm.recv() => { info!("SIGTERM received"); break; }

            // The sender pool's runner exited (e.g. a panic in a per-connection
            // sender). The update stream is now dead and would hot-spin returning
            // Err forever with the process still "up" — so systemd's restart would
            // never fire. Alert and exit non-zero instead: systemd restarts us and
            // catch_up recovers the gap. This is the "never silently dead" guard.
            res = &mut conn.pool_task => {
                error!(result = ?res, "telegram sender pool exited");
                ops.notice("telegram sender pool exited — restarting").await;
                return Err(anyhow!("telegram sender pool exited"));
            }

            update = updates.next() => {
                match update {
                    Ok(u) => {
                        last_update = Instant::now();
                        probe_failures = 0;
                        handle_update(
                            u, &conn.client, &live, &deliverer, &ops, &store,
                            &mut dedup, &media_tx, &download_sem, contract_passthrough,
                        ).await;
                    }
                    // Back off briefly so a repeated stream error can't hot-spin
                    // the loop (the pool-exit arm above handles a truly dead pool).
                    Err(e) => {
                        warn!(error = %e, "update stream error");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }

            // Active liveness + deauth probe, only after LIVENESS_PROBE of silence
            // (a busy relay is self-evidently alive). A revoked session is fatal —
            // re-login required — so alert and exit. A transient failure is
            // tolerated (the invoke also forces a reconnect); only sustained
            // failure escalates to a restart.
            _ = liveness_tick.tick() => {
                if last_update.elapsed() >= LIVENESS_PROBE {
                    match conn.client.get_me().await {
                        Ok(_) => { probe_failures = 0; }
                        Err(e) if is_auth_error(&e) => {
                            error!(error = %e, "telegram session deauthorized");
                            ops.notice(
                                "telegram session deauthorized — relay stopped; re-login required",
                            ).await;
                            return Err(anyhow!("session deauthorized: {e}"));
                        }
                        Err(e) => {
                            probe_failures += 1;
                            warn!(error = %e, probe_failures, "telegram liveness probe failed");
                            if probe_failures >= 3 {
                                ops.notice("telegram connection unrecoverable — restarting").await;
                                return Err(anyhow!(
                                    "telegram liveness lost after {probe_failures} probes"
                                ));
                            }
                        }
                    }
                }
            }

            // A download task finished: push the ready item into the album
            // buffer. The buffer stays single-owned by this loop.
            Some(item) = media_rx.recv() => {
                if let Some(batch) = album.push(item).await {
                    let d = deliverer.clone();
                    let ops = ops.clone();
                    let store = store.clone();
                    tokio::spawn(async move {
                        flush_album(&d, &ops, &store, batch, contract_passthrough).await;
                    });
                }
            }

            _ = media_tick.tick() => {
                while let Some(batch) = album.tick().await {
                    let d = deliverer.clone();
                    let ops = ops.clone();
                    let store = store.clone();
                    tokio::spawn(async move {
                        flush_album(&d, &ops, &store, batch, contract_passthrough).await;
                    });
                }
            }

            _ = heartbeat.tick() => {
                info!(pending_albums = album.pending_groups(), "heartbeat");
            }

            _ = reload_tick.tick() => {
                let current = file_mtime(config_path);
                if current != last_mtime {
                    last_mtime = current;
                    reload(config_path, &conn.client, &live, &ops).await;
                }
            }
        }
    }

    // Quiet ops (queued-polish §13): no "shutting down" post either — a clean
    // stop is not an incident. Logged locally only.
    info!("shutting down");

    // Force-flush any albums still inside their quiet window so they are
    // delivered rather than dropped on shutdown.
    for batch in album.flush_all() {
        flush_album(&deliverer, &ops, &store, batch, contract_passthrough).await;
    }

    let _ = updates.sync_update_state().await;
    conn.handle.quit();
    let _ = conn.pool_task.await;
    Ok(())
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Re-load config, re-resolve routes, and swap the [`Live`] snapshot.
/// Credential/session changes still require a restart.
async fn reload(config_path: &Path, client: &Client, live: &ArcSwap<Live>, ops: &Ops) {
    let cfg = match Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "config reload: parse failed; keeping previous");
            return;
        }
    };
    match telegram::resolve_routes(client, &cfg).await {
        Ok(res) => {
            for f in &res.failures {
                warn!("reload skipping unresolved {f}");
                ops.notice(&format!("route skipped on reload (unresolved): {f}"))
                    .await;
            }
            let active = res.routes.len();
            live.store(Arc::new(Live::build(res.routes, &cfg)));
            info!("config reloaded; {active} route(s) active");
        }
        Err(e) => {
            warn!(error = %e, "config reload: route resolution failed; keeping previous");
            ops.notice(&format!("route resolution failed on reload: {e}"))
                .await;
        }
    }
}

/// Classify → dedup → route → filter → dispatch one update.
#[allow(clippy::too_many_arguments)]
async fn handle_update(
    update: grammers_client::update::Update,
    client: &Client,
    live: &ArcSwap<Live>,
    deliverer: &Arc<Deliverer>,
    ops: &Ops,
    store: &Arc<Store>,
    dedup: &mut Dedup,
    media_tx: &mpsc::Sender<MediaItem>,
    download_sem: &Arc<tokio::sync::Semaphore>,
    contract_passthrough: bool,
) {
    let Some(incoming) = telegram::classify(update) else {
        return;
    };

    let (chat, msg_id) = match &incoming {
        Incoming::Text { chat, msg_id, .. } => (*chat, *msg_id),
        Incoming::Media { chat, msg_id, .. } => (*chat, *msg_id),
    };
    if !dedup.check_and_insert(chat, msg_id) {
        return; // duplicate
    }

    let snapshot = live.load_full();
    let routes = snapshot.router.match_chat(chat);
    if routes.is_empty() {
        return;
    }

    match incoming {
        Incoming::Text {
            sender,
            body,
            reply_quote,
            edited,
            title,
            deep_link,
            date,
            ..
        } => {
            // Contract addresses echoed as plain content outside the embed so
            // scanning bots (e.g. Rickbot) can act on them. Computed once per msg.
            let content = contract_passthrough
                .then(|| render::contract_content(&body))
                .flatten();
            // Dedup destinations across routes: if two routes on this chat list
            // the same webhook, the text posts there ONCE. Without this the PK
            // (chat, msg, discord_id) admits two rows and the refresh worker
            // maintains both duplicates forever (the media path already dedups).
            let mut seen: HashSet<String> = HashSet::new();
            for route in routes {
                if let Some(f) = &route.filter {
                    if !passes_filter(&body, f) {
                        continue;
                    }
                }
                let username = display_name(sender.as_deref(), &route.name);
                let text = RelayText {
                    sender: sender.clone(),
                    body: body.clone(),
                    reply_quote: reply_quote.clone(),
                    edited,
                };
                // A fresh post has no reactions/comments yet; the refresh worker
                // fills those in later.
                let meta = EmbedMeta {
                    title: title.clone().unwrap_or_else(|| route.name.clone()),
                    avatar_url: None,
                    deep_link: deep_link.clone(),
                    reactions: Default::default(),
                    comment_count: 0,
                    deleted: false,
                    color: route.color,
                    timestamp: Some(date.clone()),
                };
                let embeds = render::embed(&text, &meta);
                let hash = content_hash(&body);
                for (name, url) in resolve_targets_named(route, &snapshot.webhooks) {
                    if !seen.insert(name.0.clone()) {
                        continue; // already delivered to this webhook this message
                    }
                    spawn_embed(
                        deliverer.clone(),
                        ops.clone(),
                        store.clone(),
                        url,
                        NewRecord {
                            chat_id: chat.0,
                            tg_msg_id: msg_id,
                            route: route.name.clone(),
                            webhook_name: name.0.clone(),
                            discord_msg_id: String::new(), // filled on delivery
                            content_hash: hash.clone(),
                            reactions: Default::default(),
                            comment_count: 0,
                            latency_ms: None,       // measured on delivery
                            image_urls: Vec::new(), // text post: no attachments
                        },
                        username.clone(),
                        embeds.clone(),
                        date.clone(),
                        content.clone(),
                    );
                }
            }
        }
        Incoming::Media {
            grouped_id,
            media,
            caption,
            sender,
            approx_size,
            deep_link,
            title,
            date,
            ..
        } => {
            // Union of every matching route's webhooks (filter on caption),
            // deduped by webhook name so a media post reaches each distinct
            // target exactly once — even when several routes select the same
            // webhook or an album fans out across routes. Each target carries its
            // route's EFFECTIVE media mode (route override or global default).
            let mut targets: Vec<MediaTarget> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for route in routes {
                if let Some(f) = &route.filter {
                    if !passes_filter(&caption, f) {
                        continue;
                    }
                }
                let mode = effective_media_mode(route.media_mode, snapshot.media.mode);
                for (name, url) in resolve_targets_named(route, &snapshot.webhooks) {
                    if seen.insert(name.0.clone()) {
                        targets.push(MediaTarget {
                            route: route.name.clone(),
                            webhook: name,
                            url,
                            color: route.color,
                            mode,
                        });
                    }
                }
            }
            if targets.is_empty() {
                return;
            }
            let title = title.unwrap_or_else(|| targets[0].route.clone());
            let oversized = approx_size > snapshot.media.max_bytes;
            let any_reupload = targets.iter().any(|t| t.mode == MediaMode::Reupload);

            // Download the bytes only when some target wants a re-upload AND the
            // file is not oversized. Otherwise (oversized, or every target is
            // placeholder) a FILE-LESS item still flows through the SAME album
            // buffer, so a mixed album coalesces as ONE Discord message with the
            // caption anchored to the right sibling (queued-polish §10a); the
            // per-target notice is rendered at delivery (queued-polish §11c).
            if any_reupload && !oversized {
                // Downloadable: move the download OFF the hot path. The album
                // buffer coalesces siblings and flush_album posts ONE rich embed
                // (files for re-upload targets, notice for placeholder targets).
                let client = client.clone();
                let media_tx = media_tx.clone();
                let ops = ops.clone();
                let max_bytes = snapshot.media.max_bytes;
                let sem = download_sem.clone();
                tokio::spawn(async move {
                    // Gate concurrent downloads: each buffers a whole file in RAM,
                    // so without this a burst (or a catch-up backlog) of media
                    // could OOM the host. Peak media memory ~= permits * max_bytes.
                    let _permit = sem.acquire_owned().await;
                    let bytes = match download_media(&client, &media, max_bytes).await {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(error = %e, msg_id, "media download failed");
                            ops.drop_notice(&format!("media download failed (msg {msg_id}): {e}"))
                                .await;
                            return;
                        }
                    };
                    let filename = media_filename(&media, msg_id);
                    let item = MediaItem {
                        grouped_id,
                        msg_id,
                        chat,
                        file: Some((filename, bytes)),
                        oversized: false,
                        caption,
                        deep_link,
                        title,
                        sender,
                        date,
                        targets,
                    };
                    // Never block the download task on a full channel: on
                    // full/closed, log and post a rate-limited ops drop notice.
                    if let Err(e) = media_tx.try_send(item) {
                        warn!(error = %e, msg_id, "media channel send failed; dropping item");
                        ops.drop_notice(&format!("media dropped (msg {msg_id}): channel {e}"))
                            .await;
                    }
                });
            } else {
                // No download (oversized or all-placeholder): a file-less item
                // still enters the buffer so the group stays intact.
                let item = MediaItem {
                    grouped_id,
                    msg_id,
                    chat,
                    file: None,
                    oversized,
                    caption,
                    deep_link,
                    title,
                    sender,
                    date,
                    targets,
                };
                if let Err(e) = media_tx.try_send(item) {
                    warn!(error = %e, msg_id, "media channel send failed; dropping item");
                    ops.drop_notice(&format!("media dropped (msg {msg_id}): channel {e}"))
                        .await;
                }
            }
        }
    }
}

/// Spawn a fire-and-forget embed delivery. On success, record the created
/// Discord message id in the store so the refresh worker can PATCH it later.
#[allow(clippy::too_many_arguments)]
fn spawn_embed(
    deliverer: Arc<Deliverer>,
    ops: Ops,
    store: Arc<Store>,
    url: WebhookUrl,
    mut record: NewRecord,
    username: String,
    embeds: serde_json::Value,
    telegram_date: String,
    content: Option<String>,
) {
    tokio::spawn(async move {
        // Durable dedup: the in-memory LRU is empty after a restart, and
        // `catch_up: true` replays messages seen before the restart — and a
        // `backfill` run records to this same store. Consulting it here makes
        // the live path idempotent across restarts and mutually idempotent
        // with backfill, so an overlapping message is never posted twice.
        match store.already_relayed(record.chat_id, record.tg_msg_id, &record.webhook_name) {
            Ok(true) => {
                info!(
                    chat = record.chat_id,
                    msg_id = record.tg_msg_id,
                    webhook = %record.webhook_name,
                    "already relayed (store); skipping"
                );
                return;
            }
            Ok(false) => {}
            Err(e) => warn!(error = %e, "dedup store check failed; posting anyway"),
        }

        match deliverer
            .post_embed(&url, &username, &embeds, content.as_deref())
            .await
        {
            PostResult::Delivered { discord_msg_id, .. } => {
                // Live relay: measure true end-to-end latency from the two
                // authoritative clocks (Discord snowflake vs Telegram date).
                let latency = relay_latency_ms(&discord_msg_id, &telegram_date);
                if let Some(ms) = latency {
                    info!(
                        tg_msg_id = record.tg_msg_id,
                        latency_ms = ms,
                        "relayed msg {} in {}ms",
                        record.tg_msg_id,
                        ms
                    );
                }
                record.discord_msg_id = discord_msg_id;
                record.latency_ms = latency;
                if let Err(e) = store.record(record) {
                    warn!(error = %e, "failed to record relayed message");
                }
            }
            PostResult::Dropped { reason } => {
                warn!(%reason, "embed delivery dropped");
                ops.drop_notice(&format!("delivery dropped: {reason}"))
                    .await;
            }
        }
    });
}

/// Flush an album (or a single ungrouped media item) as ONE rich-embed message
/// per distinct target: the caption embed plus every file, recorded to the store
/// so the refresh worker keeps it live and restart/backfill dedup works.
///
/// Two coupled de-duplications fix the old fan-out bugs: files are deduped by
/// `msg_id` (each physical photo uploaded once) and targets are the deduped
/// union across all items (every webhook any matching route selected gets the
/// album exactly once, instead of everything going to the first item's targets).
async fn flush_album(
    deliverer: &Deliverer,
    ops: &Ops,
    store: &Store,
    batch: Vec<MediaItem>,
    contract_passthrough: bool,
) {
    let Some(album) = coalesce_album(batch) else {
        return;
    };
    // Contract addresses in the caption, echoed as plain content so bots trigger.
    let content = if contract_passthrough {
        render::contract_content(&album.caption)
    } else {
        None
    };
    // Live posts have no reactions/comments yet; the refresh worker fills them.
    deliver_coalesced_media(
        deliverer,
        store,
        Some(ops),
        album.chat_id,
        album.anchor_msg_id,
        &album.title,
        &album.caption,
        album.deep_link.as_deref(),
        album.sender.as_deref(),
        &std::collections::BTreeMap::new(),
        0,
        Some(album.date.as_str()),
        &album.files,
        album.any_oversized,
        &album.targets,
        content.as_deref(),
    )
    .await;
}

/// The notice body shown to a target that receives no attached file — a
/// placeholder-mode route, or a reupload route whose media was too large.
fn media_notice_text(mode: MediaMode, any_oversized: bool) -> &'static str {
    match mode {
        MediaMode::Placeholder => "[media]",
        MediaMode::Reupload if any_oversized => "[media too large to relay]",
        MediaMode::Reupload => "[media]",
    }
}

/// Join a caption with a media notice, dropping the separator (and leading blank
/// line) when the caption is empty.
fn media_notice_body(caption: &str, notice: &str) -> String {
    if caption.trim().is_empty() {
        notice.to_string()
    } else {
        format!("{caption}\n{notice}")
    }
}

/// The pure result of coalescing an album batch: the deduped files, the anchor
/// (caption-bearing) message identity, and the deduped union of fan-out targets.
struct CoalescedAlbum {
    chat_id: i64,
    anchor_msg_id: i32,
    title: String,
    caption: String,
    deep_link: Option<String>,
    sender: Option<String>,
    /// The anchor message's ORIGINAL Telegram publish time, RFC3339.
    date: String,
    files: Vec<(String, Vec<u8>)>,
    /// Any sibling exceeded max_bytes (so a reupload target with no deliverable
    /// files shows "[media too large to relay]" rather than a generic notice).
    any_oversized: bool,
    targets: Vec<MediaTarget>,
}

/// Coalesce an album batch into a single message's worth of data (pure; no I/O).
///
/// Two de-duplications fix the old fan-out bugs: files are deduped by `msg_id`
/// (each physical photo uploaded once) and targets are the deduped union across
/// every item (every webhook any matching route selected gets the album exactly
/// once, instead of everything going to the first item's targets). Returns
/// `None` for an empty batch.
/// Insert `_{msg_id}` before a filename's extension to break a within-album
/// name collision (`report.pdf` → `report_42.pdf`), so `attachment://` refs and
/// multipart part names stay unique. Preserves the extension so
/// [`render::is_image_filename`] still classifies it correctly.
fn disambiguate_filename(name: &str, msg_id: i32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}_{msg_id}.{ext}"),
        None => format!("{name}_{msg_id}"),
    }
}

fn coalesce_album(mut batch: Vec<MediaItem>) -> Option<CoalescedAlbum> {
    // Canonical album order; also pairs the caption with the lowest msg_id.
    sort_album_batch(&mut batch);
    if batch.is_empty() {
        return None;
    }

    // De-duplicate files by msg_id (each physical photo once), preserving order.
    // Items with no file (oversized / placeholder-only) contribute no bytes but
    // still anchor the caption + fan-out targets below.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen_ids: HashSet<i32> = HashSet::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    for it in &batch {
        if let Some((filename, bytes)) = &it.file {
            if seen_ids.insert(it.msg_id) {
                // Two documents in one album can share an original filename;
                // that would collide the `attachment://name` embed references
                // (Discord resolves them ambiguously). Disambiguate by msg_id.
                let name = if seen_names.insert(filename.clone()) {
                    filename.clone()
                } else {
                    disambiguate_filename(filename, it.msg_id)
                };
                seen_names.insert(name.clone());
                files.push((name, bytes.clone()));
            }
        }
    }
    let any_oversized = batch.iter().any(|it| it.oversized);

    // Anchor = the caption-bearing message (its msg_id keys the store row +
    // content hash); fall back to the lowest msg_id for a captionless album.
    let (anchor_msg_id, caption, deep_link, date) = batch
        .iter()
        .find(|i| !i.caption.trim().is_empty())
        .map(|i| {
            (
                i.msg_id,
                i.caption.clone(),
                i.deep_link.clone(),
                i.date.clone(),
            )
        })
        .unwrap_or_else(|| {
            (
                batch[0].msg_id,
                String::new(),
                batch[0].deep_link.clone(),
                batch[0].date.clone(),
            )
        });

    // Union of distinct targets across every item, deduped by webhook name.
    let mut targets: Vec<MediaTarget> = Vec::new();
    let mut seen_hooks: HashSet<String> = HashSet::new();
    for it in &batch {
        for t in &it.targets {
            if seen_hooks.insert(t.webhook.0.clone()) {
                targets.push(t.clone());
            }
        }
    }

    Some(CoalescedAlbum {
        chat_id: batch[0].chat.0,
        anchor_msg_id,
        title: batch[0].title.clone(),
        caption,
        deep_link,
        sender: batch[0].sender.clone(),
        date,
        files,
        any_oversized,
        targets,
    })
}

/// Post one coalesced media message — a rich embed plus every attached file — to
/// each distinct target, recording each delivery. Shared by the live album flush
/// and `backfill` so their media rendering + store tracking are identical.
///
/// Reactions/comment counts start empty; the refresh worker fills them in on its
/// next tick (and PATCHes the embed in place, leaving the attachments intact).
/// Returns `(posted, dropped, skipped_dupe)` for the caller's counters.
#[allow(clippy::too_many_arguments)]
async fn deliver_coalesced_media(
    deliverer: &Deliverer,
    store: &Store,
    ops: Option<&Ops>,
    chat_id: i64,
    anchor_msg_id: i32,
    title: &str,
    caption: &str,
    deep_link: Option<&str>,
    sender: Option<&str>,
    reactions: &std::collections::BTreeMap<String, i32>,
    comment_count: i32,
    timestamp: Option<&str>,
    files: &[(String, Vec<u8>)],
    any_oversized: bool,
    targets: &[MediaTarget],
    content: Option<&str>,
) -> (usize, usize, usize) {
    // Hash the RAW caption: the refresh worker recomputes the hash from the
    // anchor message's text alone, so this matches and avoids a spurious edit.
    let hash = content_hash(caption);

    let (mut posted, mut dropped, mut skipped) = (0usize, 0usize, 0usize);
    for t in targets {
        match store.already_relayed(chat_id, anchor_msg_id, &t.webhook.0) {
            Ok(true) => {
                info!(
                    chat = chat_id,
                    msg_id = anchor_msg_id,
                    webhook = %t.webhook.0,
                    "media already relayed (store); skipping"
                );
                skipped += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(error = %e, chat = chat_id, msg_id = anchor_msg_id, "media dedup check failed; posting anyway")
            }
        }

        // Per-target media mode (queued-polish §11c): a reupload target with
        // deliverable files gets the attached album; a placeholder target — or a
        // reupload target whose media was oversized — gets a caption + notice
        // embed with the deep link and no file (queued-polish §10a keeps the
        // whole album one message either way).
        let deliver_files = t.mode == MediaMode::Reupload && !files.is_empty();
        let body = if deliver_files {
            // A mixed-size album can post some files while others were skipped
            // for being oversized — say so, rather than silently dropping them.
            if any_oversized {
                media_notice_body(caption, "⚠️ some files were too large to relay")
            } else {
                caption.to_string()
            }
        } else {
            media_notice_body(caption, media_notice_text(t.mode, any_oversized))
        };
        let text = RelayText {
            sender: None,
            body,
            reply_quote: None,
            edited: false,
        };
        // Built per target: the stripe color is the *route's*, and the refresh
        // worker re-derives it from the recorded row's route, so one shared
        // embed would let a multi-route fan-out flip color on its first PATCH.
        let meta = EmbedMeta {
            title: title.to_string(),
            avatar_url: None,
            deep_link: deep_link.map(|s| s.to_string()),
            reactions: reactions.clone(),
            comment_count,
            deleted: false,
            color: t.color,
            timestamp: timestamp.map(|s| s.to_string()),
        };
        let mut embeds = render::embed(&text, &meta);
        // Render images INSIDE the embed frame (attachment://) rather than as
        // bare attachments below it. Only images can be inlined; video/docs
        // still attach below. Filenames must match the parts post_media_embed
        // sends, so both derive from the same `files` list.
        if deliver_files {
            let image_names: Vec<&str> = files
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|name| render::is_image_filename(name))
                .collect();
            let refs = render::attachment_refs(&image_names);
            let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
            render::attach_image_urls(&mut embeds, &refs, deep_link);
        }

        let username = display_name(sender, &t.route);
        let result = if deliver_files {
            deliverer
                .post_media_embed(&t.url, &username, &embeds, files.to_vec(), content)
                .await
        } else {
            deliverer
                .post_embed(&t.url, &username, &embeds, content)
                .await
        };
        match result {
            PostResult::Delivered {
                discord_msg_id,
                image_urls,
            } => {
                // Latency is only meaningful for LIVE relays (ops present); the
                // backfill path passes `ops: None` and records NULL latency.
                let latency_ms = match (ops, timestamp) {
                    (Some(_), Some(ts)) => relay_latency_ms(&discord_msg_id, ts),
                    _ => None,
                };
                if let Some(ms) = latency_ms {
                    info!(
                        tg_msg_id = anchor_msg_id,
                        latency_ms = ms,
                        "relayed msg {} in {}ms",
                        anchor_msg_id,
                        ms
                    );
                }
                let record = NewRecord {
                    chat_id,
                    tg_msg_id: anchor_msg_id,
                    route: t.route.clone(),
                    webhook_name: t.webhook.0.clone(),
                    discord_msg_id,
                    content_hash: hash.clone(),
                    reactions: reactions.clone(),
                    comment_count,
                    latency_ms,
                    image_urls,
                };
                if let Err(e) = store.record(record) {
                    warn!(error = %e, "failed to record relayed media");
                }
                posted += 1;
            }
            PostResult::Dropped { reason } => {
                warn!(%reason, chat = chat_id, msg_id = anchor_msg_id, webhook = %t.webhook.0, "media delivery dropped");
                if let Some(ops) = ops {
                    ops.drop_notice(&format!("media delivery dropped: {reason}"))
                        .await;
                }
                dropped += 1;
            }
        }
    }
    (posted, dropped, skipped)
}

/// Resolve a route's webhook names into concrete `(name, url)` pairs against the
/// snapshot map. The name is kept so a delivery can be recorded (for dedup +
/// PATCHing) under the webhook it was posted to.
fn resolve_targets_named(
    route: &ResolvedRoute,
    webhooks: &HashMap<WebhookName, WebhookUrl>,
) -> Vec<(WebhookName, WebhookUrl)> {
    route
        .to
        .iter()
        .filter_map(|name| {
            let url = webhooks.get(name).cloned();
            if url.is_none() {
                warn!(webhook = %name.0, route = %route.name, "route references unknown webhook");
            }
            url.map(|u| (name.clone(), u))
        })
        .collect()
}

/// Discord display name: prefer the message sender, fall back to the route name.
fn display_name(sender: Option<&str>, route_name: &str) -> String {
    match sender {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => route_name.to_string(),
    }
}

/// Discord epoch (2015-01-01T00:00:00Z) in milliseconds — the offset a snowflake
/// timestamp is measured from.
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// Decode a Discord message snowflake into its creation time (ms since Unix
/// epoch): the top 42 bits are ms since the Discord epoch. `None` if the id is
/// not a plain integer.
fn snowflake_ms(discord_msg_id: &str) -> Option<i64> {
    let id: u64 = discord_msg_id.parse().ok()?;
    Some((id >> 22) as i64 + DISCORD_EPOCH_MS)
}

/// End-to-end relay latency in ms: Discord receive time (decoded from the
/// message snowflake) minus the Telegram publish time (`message.date()`, second
/// resolution, carried as the RFC3339 embed timestamp). `None` if either clock
/// can't be read. Accurate to ~1s since Telegram's date is second-resolution.
fn relay_latency_ms(discord_msg_id: &str, telegram_date_rfc3339: &str) -> Option<i64> {
    let discord_ms = snowflake_ms(discord_msg_id)?;
    let telegram_ms = chrono::DateTime::parse_from_rfc3339(telegram_date_rfc3339)
        .ok()?
        .timestamp_millis();
    Some(discord_ms - telegram_ms)
}

/// Download a media file fully into memory via the chunked download iterator.
async fn download_media(client: &Client, media: &Media, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let mut download = client.iter_download(media);
    let mut bytes = Vec::new();
    while let Some(chunk) = download.next().await? {
        bytes.extend_from_slice(&chunk);
        // Enforce the ceiling on ACTUAL bytes, not the declared size(): a media
        // whose size() is None reads as 0 and would otherwise download without
        // bound. Abort early so one file can't exhaust memory.
        if bytes.len() as u64 > max_bytes {
            anyhow::bail!(
                "media exceeded max_bytes ({} > {max_bytes}) mid-download",
                bytes.len()
            );
        }
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// webhook avatar/name sync
// ---------------------------------------------------------------------------

/// Stable content hash of a webhook's intended identity (title + photo bytes).
///
/// FNV-1a over the title, a separator, and the photo bytes — deterministic
/// across runs so it can be persisted and compared. A title rename OR a new
/// photo changes the hash and triggers a re-PATCH; nothing else does. The
/// separator prevents a title/photo boundary ambiguity.
fn avatar_identity_hash(title: &str, photo: Option<&[u8]>) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    mix(title.as_bytes());
    mix(&[0x00]); // domain separator between title and photo
    match photo {
        Some(bytes) => mix(bytes),
        None => mix(b"<no-photo>"),
    }
    format!("{h:016x}")
}

/// Encode raw JPEG photo bytes as a Discord `data:` avatar URI.
///
/// Telegram profile photos are JPEG, so the mime is `image/jpeg`; Discord sniffs
/// the actual bytes but the label must be an image type it accepts.
fn photo_data_uri(bytes: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:image/jpeg;base64,{b64}")
}

/// Best-effort: mirror each source channel's title + photo onto its destination
/// Discord webhook's persistent name + avatar, once at startup.
///
/// * Only routes with a resolved [`ChannelIdentity`] (i.e. `@username` routes)
///   participate; numeric-id routes leave the webhook untouched.
/// * A webhook shared by several routes is set by the FIRST route that can set
///   it; later routes log "shared; skipped" and move on (a webhook has exactly
///   one avatar).
/// * The PATCH only fires when the identity hash differs from the last-synced
///   one recorded in the store, so restarts don't churn Discord's rate limit.
/// * Every failure is logged (token-scrubbed) and swallowed — this must never
///   block or break message relay.
async fn sync_webhook_identities(
    deliverer: &Deliverer,
    store: &Store,
    webhooks: &HashMap<WebhookName, WebhookUrl>,
    routes: &[ResolvedRoute],
    identities: &HashMap<telegram_relay::config::ChatId, ChannelIdentity>,
) {
    let mut claimed: HashSet<String> = HashSet::new();
    for route in routes {
        let Some(identity) = identities.get(&route.chat) else {
            continue; // numeric-id route (or unresolved): don't touch its webhooks
        };
        for hook in &route.to {
            if claimed.contains(&hook.0) {
                info!(
                    webhook = %hook.0,
                    route = %route.name,
                    "webhook avatar already set by an earlier route; shared, skipping"
                );
                continue;
            }
            let Some(url) = webhooks.get(hook) else {
                continue; // unknown webhook already warned about elsewhere
            };
            claimed.insert(hook.0.clone());

            let hash = avatar_identity_hash(&identity.title, identity.photo.as_deref());
            match store.webhook_identity_hash(&hook.0) {
                Ok(Some(prev)) if prev == hash => {
                    info!(webhook = %hook.0, "webhook identity unchanged; skipping avatar PATCH");
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, webhook = %hook.0, "webhook identity hash check failed; syncing anyway");
                }
            }

            let data_uri = identity.photo.as_deref().map(photo_data_uri);
            match deliverer
                .patch_webhook(url, Some(&identity.title), data_uri.as_deref())
                .await
            {
                Outcome::Delivered => {
                    info!(webhook = %hook.0, title = %identity.title, had_photo = identity.photo.is_some(), "webhook identity synced");
                    if let Err(e) = store.set_webhook_identity_hash(&hook.0, &hash) {
                        warn!(error = %e, webhook = %hook.0, "failed to record webhook identity hash");
                    }
                }
                Outcome::Dropped { reason } => {
                    // Scrub any webhook token before it reaches the log, matching
                    // the redaction guarantee on every other egress path.
                    warn!(
                        reason = %scrub_webhook_tokens(&reason),
                        webhook = %hook.0,
                        "webhook identity sync failed; continuing"
                    );
                }
            }
        }
    }
}

/// Best-effort filename for an attachment.
fn media_filename(media: &Media, msg_id: i32) -> String {
    match media {
        Media::Document(d) => d
            .name()
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("file_{msg_id}.bin")),
        Media::Photo(_) => format!("photo_{msg_id}.jpg"),
        Media::Sticker(_) => format!("sticker_{msg_id}.webp"),
        _ => format!("media_{msg_id}.bin"),
    }
}

#[cfg(test)]
mod backfill_tests {
    use super::*;

    #[test]
    fn oldest_first_reverses_newest_to_oldest_order() {
        // grammers' iter_messages().limit(N) yields newest-to-oldest, e.g.
        // ids [103, 102, 101] for the 3 most recent posts; backfill must relay
        // them chronologically, i.e. [101, 102, 103].
        assert_eq!(oldest_first(vec![103, 102, 101]), vec![101, 102, 103]);
    }

    #[test]
    fn oldest_first_handles_empty_and_singleton() {
        assert_eq!(oldest_first::<i32>(vec![]), Vec::<i32>::new());
        assert_eq!(oldest_first(vec![42]), vec![42]);
    }

    #[test]
    fn oldest_first_is_involutive() {
        let original = vec![5, 4, 3, 2, 1];
        let once = oldest_first(original.clone());
        assert_eq!(once, vec![1, 2, 3, 4, 5]);
        // reversing back recovers the original (a pure Vec::reverse, so
        // applying it twice is a no-op on the sequence).
        assert_eq!(oldest_first(once), original);
    }

    #[test]
    fn clamp_backfill_count_matches_cli_defaults() {
        // Sanity-check main.rs is using the same clamp cli.rs exposes (rather
        // than a copy-pasted range), and that the documented default (3) and
        // max (25) haven't silently drifted apart.
        assert_eq!(
            clamp_backfill_count(telegram_relay::cli::BACKFILL_DEFAULT_COUNT),
            3
        );
        assert_eq!(
            clamp_backfill_count(telegram_relay::cli::BACKFILL_MAX_COUNT + 100),
            telegram_relay::cli::BACKFILL_MAX_COUNT
        );
    }
}

#[cfg(test)]
mod album_coalesce_tests {
    use super::*;
    use telegram_relay::config::ChatId;

    #[test]
    fn disambiguate_filename_breaks_collisions_keeping_extension() {
        assert_eq!(disambiguate_filename("report.pdf", 42), "report_42.pdf");
        assert_eq!(disambiguate_filename("a.b.png", 7), "a.b_7.png");
        assert_eq!(disambiguate_filename("noext", 5), "noext_5");
        // extension preserved so image detection still works
        assert!(render::is_image_filename(&disambiguate_filename(
            "x.jpg", 9
        )));
    }

    fn doc_item(msg_id: i32, filename: &str) -> MediaItem {
        MediaItem {
            grouped_id: Some(99),
            msg_id,
            chat: ChatId(-100),
            file: Some((filename.to_string(), vec![1, 2, 3])),
            oversized: false,
            caption: String::new(),
            deep_link: None,
            title: "t".into(),
            sender: None,
            date: "2026-07-20T12:00:00Z".into(),
            targets: vec![MediaTarget {
                route: "r1".into(),
                webhook: WebhookName("h".into()),
                url: WebhookUrl("https://x/h".into()),
                color: render::DEFAULT_EMBED_COLOR,
                mode: MediaMode::Reupload,
            }],
        }
    }

    #[test]
    fn same_named_documents_get_unique_filenames() {
        // Two docs in one album sharing "report.pdf" must not collide.
        let batch = vec![doc_item(1, "report.pdf"), doc_item(2, "report.pdf")];
        let album = coalesce_album(batch).unwrap();
        let names: Vec<&str> = album.files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1], "filenames must be disambiguated");
    }

    fn media_item(msg_id: i32, caption: &str, hooks: &[&str]) -> MediaItem {
        MediaItem {
            grouped_id: Some(42),
            msg_id,
            chat: ChatId(-100),
            file: Some((
                format!("photo_{msg_id}.jpg"),
                format!("bytes-{msg_id}").into_bytes(),
            )),
            oversized: false,
            caption: caption.to_string(),
            deep_link: Some(format!("https://t.me/c/{msg_id}")),
            title: "Rob's Channel".into(),
            sender: None,
            date: format!("2026-07-20T12:00:{msg_id:02}Z"),
            targets: hooks
                .iter()
                .map(|h| MediaTarget {
                    route: "r1".into(),
                    webhook: WebhookName(h.to_string()),
                    url: WebhookUrl(format!("https://discord.example/{h}")),
                    color: render::DEFAULT_EMBED_COLOR,
                    mode: MediaMode::Reupload,
                })
                .collect(),
        }
    }

    fn hook_names(album: &CoalescedAlbum) -> Vec<&str> {
        album.targets.iter().map(|t| t.webhook.0.as_str()).collect()
    }

    fn file_names(album: &CoalescedAlbum) -> Vec<String> {
        album.files.iter().map(|f| f.0.clone()).collect()
    }

    #[test]
    fn dedups_files_by_msgid_and_unions_targets() {
        // An album of two photos where each item carries the FULL target set
        // [alpha, beta] — mirroring the live path where every photo fans out to
        // both webhooks. The old bug uploaded every file per item and only to
        // the first item's targets; coalescing must yield 2 files delivered to
        // each of the 2 distinct targets exactly once.
        let batch = vec![
            media_item(101, "album caption", &["alpha", "beta"]),
            media_item(102, "", &["alpha", "beta"]),
        ];
        let album = coalesce_album(batch).expect("non-empty batch");
        assert_eq!(file_names(&album), vec!["photo_101.jpg", "photo_102.jpg"]);
        assert_eq!(hook_names(&album), vec!["alpha", "beta"]);
        assert_eq!(album.caption, "album caption");
        assert_eq!(album.anchor_msg_id, 101);
    }

    #[test]
    fn orders_files_by_msgid_and_anchors_on_caption() {
        // Downloads complete out of order and the caption rides a later msg_id:
        // files still sort by msg_id, and the anchor follows the caption.
        let batch = vec![
            media_item(203, "", &["alpha"]),
            media_item(201, "", &["alpha"]),
            media_item(202, "the caption", &["alpha"]),
        ];
        let album = coalesce_album(batch).expect("non-empty batch");
        assert_eq!(
            file_names(&album),
            vec!["photo_201.jpg", "photo_202.jpg", "photo_203.jpg"]
        );
        assert_eq!(album.anchor_msg_id, 202);
        assert_eq!(album.caption, "the caption");
    }

    #[test]
    fn dedups_repeated_msgid_and_unions_split_targets() {
        // The same physical photo appears twice (as it did pre-union, once per
        // route) but with different webhooks: one file, unioned targets.
        let batch = vec![
            media_item(301, "cap", &["alpha"]),
            media_item(301, "cap", &["beta"]),
        ];
        let album = coalesce_album(batch).expect("non-empty batch");
        assert_eq!(album.files.len(), 1);
        assert_eq!(hook_names(&album), vec!["alpha", "beta"]);
    }

    #[test]
    fn empty_batch_is_none() {
        assert!(coalesce_album(vec![]).is_none());
    }

    #[test]
    fn coalesce_anchors_date_on_the_caption_bearing_member() {
        // The album's timestamp must follow the caption/anchor member (msg 102),
        // not the first-downloaded item — the whole point is the SOURCE post
        // time, and the anchor is the message whose id keys the store row.
        let batch = vec![
            media_item(103, "", &["alpha"]),
            media_item(102, "the caption", &["alpha"]),
            media_item(101, "", &["alpha"]),
        ];
        let album = coalesce_album(batch).expect("non-empty batch");
        assert_eq!(album.anchor_msg_id, 102);
        assert_eq!(album.date, "2026-07-20T12:00:102Z");
    }

    #[test]
    fn coalesce_targets_preserve_route_color() {
        let mut a = media_item(101, "cap", &["alpha"]);
        a.targets[0].color = 0xFF8800;
        let album = coalesce_album(vec![a]).expect("non-empty batch");
        assert_eq!(album.targets[0].color, 0xFF8800);
    }
}

#[cfg(test)]
mod avatar_tests {
    use super::{avatar_identity_hash, photo_data_uri};

    #[test]
    fn identity_hash_is_stable_and_deterministic() {
        let a = avatar_identity_hash("Rob's Channel", Some(b"jpegbytes"));
        let b = avatar_identity_hash("Rob's Channel", Some(b"jpegbytes"));
        assert_eq!(a, b, "same title+photo must hash identically across calls");
    }

    #[test]
    fn identity_hash_changes_on_title_rename() {
        let before = avatar_identity_hash("Old Name", Some(b"photo"));
        let after = avatar_identity_hash("New Name", Some(b"photo"));
        assert_ne!(before, after, "a rename must trigger a re-PATCH");
    }

    #[test]
    fn identity_hash_changes_on_new_photo() {
        let before = avatar_identity_hash("Same", Some(b"photo-v1"));
        let after = avatar_identity_hash("Same", Some(b"photo-v2"));
        assert_ne!(before, after, "a new photo must trigger a re-PATCH");
    }

    #[test]
    fn identity_hash_distinguishes_no_photo_from_photo() {
        let none = avatar_identity_hash("Chan", None);
        let some = avatar_identity_hash("Chan", Some(b"anything"));
        assert_ne!(none, some);
        // ...and no-photo is itself stable.
        assert_eq!(none, avatar_identity_hash("Chan", None));
    }

    #[test]
    fn identity_hash_separator_prevents_title_photo_ambiguity() {
        // Without the domain separator, ("ab", "c") and ("a", "bc") would
        // collide once title and photo are concatenated.
        let x = avatar_identity_hash("ab", Some(b"c"));
        let y = avatar_identity_hash("a", Some(b"bc"));
        assert_ne!(x, y);
    }

    #[test]
    fn photo_data_uri_is_base64_jpeg() {
        // "Hi" -> base64 "SGk="
        assert_eq!(photo_data_uri(b"Hi"), "data:image/jpeg;base64,SGk=");
        assert!(photo_data_uri(&[0xFF, 0xD8, 0xFF]).starts_with("data:image/jpeg;base64,"));
    }
}

#[cfg(test)]
mod latency_tests {
    use super::{relay_latency_ms, snowflake_ms, DISCORD_EPOCH_MS};

    #[test]
    fn snowflake_decodes_to_discord_epoch_ms() {
        // id 0 -> exactly the Discord epoch; the low 22 bits are worker/seq and
        // must be masked off by the >>22 shift.
        assert_eq!(snowflake_ms("0"), Some(DISCORD_EPOCH_MS));
        // (1 << 22) ms after epoch = epoch + 1ms.
        assert_eq!(
            snowflake_ms(&(1u64 << 22).to_string()),
            Some(DISCORD_EPOCH_MS + 1)
        );
        // A realistic snowflake decodes to a sane 2020s timestamp (> epoch).
        let id = "1234567890123456789";
        assert!(snowflake_ms(id).unwrap() > DISCORD_EPOCH_MS);
    }

    #[test]
    fn snowflake_rejects_non_integer() {
        assert_eq!(snowflake_ms("not-a-number"), None);
        assert_eq!(snowflake_ms(""), None);
    }

    #[test]
    fn latency_is_discord_minus_telegram() {
        // Telegram published at 2021-01-01T00:00:00Z (unix 1609459200 -> *1000).
        // Build a Discord id whose snowflake time is 240ms later.
        let telegram_ms: i64 = 1_609_459_200_000;
        let discord_ms = telegram_ms + 240;
        let id = (((discord_ms - DISCORD_EPOCH_MS) as u64) << 22).to_string();
        assert_eq!(
            relay_latency_ms(&id, "2021-01-01T00:00:00Z"),
            Some(240),
            "latency must be Discord snowflake ms minus Telegram publish ms"
        );
    }

    #[test]
    fn latency_none_on_bad_inputs() {
        assert_eq!(relay_latency_ms("nope", "2021-01-01T00:00:00Z"), None);
        assert_eq!(relay_latency_ms("123", "not-a-date"), None);
    }
}

#[cfg(test)]
mod ops_scrub_tests {
    use super::scrub_webhook_tokens;

    #[test]
    fn redacts_webhook_token_but_keeps_context() {
        let s = "delivery dropped: error on \
                 https://discord.com/api/webhooks/12345/SECRETtok-EN_abc123 boom";
        let out = scrub_webhook_tokens(s);
        assert!(!out.contains("SECRETtok-EN_abc123"), "token leaked: {out}");
        assert!(out.contains("/webhooks/12345/«redacted»"), "got: {out}");
        assert!(out.contains("12345")); // the id is fine to keep
        assert!(out.contains("boom")); // trailing context preserved
    }

    #[test]
    fn redacts_token_at_end_of_string() {
        let out = scrub_webhook_tokens("https://discord.com/api/webhooks/99/TOKENxyz");
        assert!(!out.contains("TOKENxyz"), "token leaked: {out}");
        assert!(out.ends_with("/webhooks/99/«redacted»"));
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(
            scrub_webhook_tokens("all good, no urls here"),
            "all good, no urls here"
        );
    }
}
