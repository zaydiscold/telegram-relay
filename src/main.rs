//! telegram-relay daemon entrypoint + CLI dispatch.
//!
//! Wires everything built so far into a running relay:
//! `run` connects to Telegram, resolves routes, and runs a `tokio::select!` loop
//! over the update stream + SIGTERM/SIGINT + a heartbeat + a media-flush tick +
//! a config hot-reload tick. Each update is classified → deduped → routed →
//! filtered → dispatched (text rendered+posted, media downloaded+coalesced).
//!
//! See `docs/superpowers/plans/api-notes.md` for the grammers 0.10.0 API shape.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context};
use arc_swap::ArcSwap;
use clap::Parser;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use telegram_relay::cli::{clamp_backfill_count, Cli, Command, SESSION_FILE};
use telegram_relay::config::{Config, MediaCfg, MediaMode, WebhookName, WebhookUrl};
use telegram_relay::dedup::Dedup;
use telegram_relay::deliver::{Deliverer, Outcome, PostResult};
use telegram_relay::media::{sort_album_batch, AlbumBuffer, MediaItem};
use telegram_relay::refresh::{self, content_hash, GrammersFetcher};
use telegram_relay::render::{self, passes_filter, EmbedMeta, RelayText};
use telegram_relay::router::{ResolvedRoute, Router};
use telegram_relay::store::{NewRecord, Store};
use telegram_relay::telegram::{self, Incoming};

use grammers_client::media::Media;
use grammers_client::Client;

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
                println!("{name:<24}  {:<8}  {e}", "FAIL");
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
    let store = Store::open(&cfg.store.path).context("opening message store")?;

    let conn = telegram::connect(api_id, Path::new(SESSION_FILE)).await?;
    if !conn.client.is_authorized().await? {
        conn.handle.quit();
        let _ = conn.pool_task.await;
        return Err(anyhow!("not authorized; run `telegram-relay login` first"));
    }

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

    for msg in &messages {
        let msg_id = msg.id();
        let raw_body = msg.text().to_string();
        if let Some(f) = &route.filter {
            if !passes_filter(&raw_body, f) {
                skipped_filter += 1;
                continue;
            }
        }

        let fetched = refresh::to_fetched(msg);
        let mut body = fetched.body.clone();
        if msg.media().is_some() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str("media omitted (backfill)");
        }

        let sender = msg.sender().and_then(|p| p.name()).map(|s| s.to_string());
        let username = display_name(sender.as_deref(), &route.name);
        let title = if fetched.title.is_empty() {
            route.name.clone()
        } else {
            fetched.title.clone()
        };
        let text = RelayText {
            sender: sender.clone(),
            body: body.clone(),
            reply_quote: None,
            edited: false,
        };
        let meta = EmbedMeta {
            title,
            avatar_url: None,
            deep_link: fetched.deep_link.clone(),
            reactions: fetched.reactions.clone(),
            comment_count: fetched.comment_count,
            deleted: false,
        };
        let embeds = render::embed(&text, &meta);
        let hash = content_hash(&body);

        for (name, url) in &targets {
            match store.already_relayed(resolved.chat.0, msg_id, &name.0) {
                Ok(true) => {
                    info!(
                        chat = resolved.chat.0,
                        msg_id,
                        webhook = %name.0,
                        "backfill: already relayed; skipping"
                    );
                    skipped_dupe += 1;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(error = %e, chat = resolved.chat.0, msg_id, "backfill: dedup check failed; posting anyway");
                }
            }

            match deliverer.post_embed(url, &username, &embeds).await {
                PostResult::Delivered { discord_msg_id } => {
                    let record = NewRecord {
                        chat_id: resolved.chat.0,
                        tg_msg_id: msg_id,
                        route: route.name.clone(),
                        webhook_name: name.0.clone(),
                        discord_msg_id,
                        content_hash: hash.clone(),
                        reactions: fetched.reactions.clone(),
                        comment_count: fetched.comment_count,
                    };
                    if let Err(e) = store.record(record) {
                        warn!(error = %e, "backfill: failed to record relayed message");
                    }
                    posted += 1;
                }
                PostResult::Dropped { reason } => {
                    warn!(%reason, chat = resolved.chat.0, msg_id, webhook = %name.0, "backfill: delivery dropped");
                    dropped += 1;
                }
            }
        }
    }

    conn.handle.quit();
    let _ = conn.pool_task.await;

    info!(
        fetched = fetched_total,
        posted, skipped_filter, skipped_dupe, dropped, "backfill complete"
    );
    println!(
        "backfill '{route_name}': {fetched_total} fetched, {posted} posted, \
         {skipped_filter} skipped (filter), {skipped_dupe} skipped (already relayed), \
         {dropped} dropped"
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
    async fn notice(&self, msg: &str) {
        if let Some(url) = &self.url {
            let _ = self
                .deliverer
                .post_text(url, "relay-ops", &[msg.to_string()])
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

async fn run(config_path: &Path) -> anyhow::Result<()> {
    let (api_id, _api_hash) = api_credentials()?;
    let cfg = Config::load(config_path).context("loading config")?;

    let deliverer = Arc::new(Deliverer::new());
    let ops = Ops::new(deliverer.clone(), cfg.ops_webhook.clone());

    // Telegram must connect BEFORE the store opens: grammers-session (libsql)
    // calls sqlite3_config(), which fails with SQLITE_MISUSE if rusqlite has
    // already initialized the linked-in SQLite. Order is load-bearing.
    let conn = telegram::connect(api_id, Path::new(SESSION_FILE)).await?;

    let store = Arc::new(Store::open(&cfg.store.path).context("opening message store")?);
    info!("message store at {}", cfg.store.path.display());

    if !conn.client.is_authorized().await? {
        conn.handle.quit();
        let _ = conn.pool_task.await;
        return Err(anyhow!("not authorized; run `telegram-relay login` first"));
    }

    // Initial route resolution — fatal if it fails (nothing to route).
    let resolved = match telegram::resolve_routes(&conn.client, &cfg).await {
        Ok(r) => r,
        Err(e) => {
            ops.notice(&format!("startup route resolution failed: {e}"))
                .await;
            conn.handle.quit();
            let _ = conn.pool_task.await;
            return Err(e);
        }
    };
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
        let refresh_cfg = cfg.refresh;
        tokio::spawn(async move {
            refresh::run(fetcher, store, deliverer, webhooks, refresh_cfg).await;
        });
        info!(
            interval_mins = cfg.refresh.interval_mins,
            horizon_hours = cfg.refresh.horizon_hours,
            "refresh worker started"
        );
    }

    let mut updates = telegram::stream_updates(&conn.client, conn.updates).await?;
    let mut dedup = Dedup::new(DEDUP_CAP);
    let mut album = AlbumBuffer::new(ALBUM_WINDOW);

    // Downloads run on spawned tasks and hand finished items back to the loop
    // over this channel; `album` stays single-owned by the loop.
    let (media_tx, mut media_rx) = mpsc::channel::<MediaItem>(MEDIA_CHANNEL_CAP);

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut media_tick = tokio::time::interval(MEDIA_TICK);
    let mut reload_tick = tokio::time::interval(RELOAD_TICK);
    let mut last_mtime = file_mtime(config_path);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;

    info!("relay started; watching {} route(s)", cfg.routes.len());
    ops.notice("relay started").await;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { info!("SIGINT received"); break; }
            _ = sigterm.recv() => { info!("SIGTERM received"); break; }

            update = updates.next() => {
                match update {
                    Ok(u) => {
                        handle_update(
                            u, &conn.client, &live, &deliverer, &ops, &store,
                            &mut dedup, &media_tx,
                        ).await;
                    }
                    Err(e) => warn!(error = %e, "update stream error"),
                }
            }

            // A download task finished: push the ready item into the album
            // buffer. The buffer stays single-owned by this loop.
            Some(item) = media_rx.recv() => {
                if let Some(batch) = album.push(item).await {
                    let d = deliverer.clone();
                    let ops = ops.clone();
                    tokio::spawn(async move { flush_album(&d, &ops, batch).await; });
                }
            }

            _ = media_tick.tick() => {
                while let Some(batch) = album.tick().await {
                    let d = deliverer.clone();
                    let ops = ops.clone();
                    tokio::spawn(async move { flush_album(&d, &ops, batch).await; });
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

    info!("shutting down");
    ops.notice("relay shutting down").await;

    // Force-flush any albums still inside their quiet window so they are
    // delivered rather than dropped on shutdown.
    for batch in album.flush_all() {
        flush_album(&deliverer, &ops, batch).await;
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
            live.store(Arc::new(Live::build(res.routes, &cfg)));
            info!("config reloaded; {} route(s)", cfg.routes.len());
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
            ..
        } => {
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
                };
                let embeds = render::embed(&text, &meta);
                let hash = content_hash(&body);
                for (name, url) in resolve_targets_named(route, &snapshot.webhooks) {
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
                        },
                        username.clone(),
                        embeds.clone(),
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
            ..
        } => {
            // Determine which routes actually want this (filter on caption).
            let mut wanted: Vec<(&ResolvedRoute, Vec<WebhookUrl>)> = Vec::new();
            for route in routes {
                if let Some(f) = &route.filter {
                    if !passes_filter(&caption, f) {
                        continue;
                    }
                }
                let targets = resolve_targets(route, &snapshot.webhooks);
                if !targets.is_empty() {
                    wanted.push((route, targets));
                }
            }
            if wanted.is_empty() {
                return;
            }

            // Oversized (or placeholder mode): post a text notice with the deep
            // link instead of downloading/re-uploading.
            let oversized = approx_size > snapshot.media.max_bytes;
            if oversized || snapshot.media.mode == MediaMode::Placeholder {
                let link = deep_link.unwrap_or_else(|| "(no public link)".to_string());
                let note = if oversized {
                    format!("[media too large to relay — {approx_size} bytes]\n{link}")
                } else {
                    format!("[media]\n{link}")
                };
                let body = if caption.is_empty() {
                    note
                } else {
                    format!("{caption}\n{note}")
                };
                for (route, targets) in &wanted {
                    let username = display_name(sender.as_deref(), &route.name);
                    let text = RelayText {
                        sender: sender.clone(),
                        body: body.clone(),
                        reply_quote: None,
                        edited: false,
                    };
                    let chunks = render::render(&text);
                    for url in targets {
                        spawn_text(
                            deliverer.clone(),
                            ops.clone(),
                            url.clone(),
                            username.clone(),
                            chunks.clone(),
                        );
                    }
                }
                return;
            }

            // Precompute owned routing data (username + targets) per matching
            // route so the download task needs nothing borrowed from the loop.
            let route_items: Vec<(String, Vec<WebhookUrl>)> = wanted
                .into_iter()
                .map(|(route, targets)| (display_name(sender.as_deref(), &route.name), targets))
                .collect();

            // Move the download OFF the hot path: spawn a task that downloads,
            // builds one MediaItem per route, and hands each back over the
            // channel. handle_update never awaits the download, so text
            // delivery, timers, and shutdown are never blocked by it.
            let client = client.clone();
            let media_tx = media_tx.clone();
            let ops = ops.clone();
            tokio::spawn(async move {
                let bytes = match download_media(&client, &media).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, msg_id, "media download failed");
                        ops.drop_notice(&format!("media download failed (msg {msg_id}): {e}"))
                            .await;
                        return;
                    }
                };
                let filename = media_filename(&media, msg_id);

                // One MediaItem per matching route (each carries its own
                // targets); only the first route's item keeps the caption to
                // avoid duplicate caption posts across fan-out.
                for (i, (username, targets)) in route_items.into_iter().enumerate() {
                    let item = MediaItem {
                        grouped_id,
                        msg_id,
                        chat,
                        username,
                        filename: filename.clone(),
                        bytes: bytes.clone(),
                        caption: if i == 0 {
                            caption.clone()
                        } else {
                            String::new()
                        },
                        targets,
                    };
                    // Never block the download task on a full channel: on
                    // full/closed, log and post a rate-limited ops drop notice.
                    if let Err(e) = media_tx.try_send(item) {
                        warn!(error = %e, msg_id, "media channel send failed; dropping item");
                        ops.drop_notice(&format!("media dropped (msg {msg_id}): channel {e}"))
                            .await;
                    }
                }
            });
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
) {
    tokio::spawn(async move {
        match deliverer.post_embed(&url, &username, &embeds).await {
            PostResult::Delivered { discord_msg_id } => {
                record.discord_msg_id = discord_msg_id;
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

/// Spawn a fire-and-forget text delivery, reporting drops to ops.
fn spawn_text(
    deliverer: Arc<Deliverer>,
    ops: Ops,
    url: WebhookUrl,
    username: String,
    chunks: Vec<String>,
) {
    tokio::spawn(async move {
        if let Outcome::Dropped { reason } = deliverer.post_text(&url, &username, &chunks).await {
            warn!(%reason, "text delivery dropped");
            ops.drop_notice(&format!("delivery dropped: {reason}"))
                .await;
        }
    });
}

/// Post an album batch: optional caption text first, then each file, per target.
async fn flush_album(deliverer: &Deliverer, ops: &Ops, mut batch: Vec<MediaItem>) {
    // Sort by msg_id to ensure album items are posted in canonical order.
    // Concurrent downloads may complete out of order; sorting guarantees the
    // caption (which rides on the first item) is paired with the correct msg_id.
    sort_album_batch(&mut batch);

    let Some(first) = batch.first() else {
        return;
    };
    for target in &first.targets {
        if !first.caption.is_empty() {
            let text = RelayText {
                sender: None,
                body: first.caption.clone(),
                reply_quote: None,
                edited: false,
            };
            let chunks = render::render(&text);
            if let Outcome::Dropped { reason } =
                deliverer.post_text(target, &first.username, &chunks).await
            {
                warn!(%reason, "album caption dropped");
                ops.drop_notice(&format!("album caption dropped: {reason}"))
                    .await;
            }
        }
        for item in &batch {
            if let Outcome::Dropped { reason } = deliverer
                .post_file(target, &item.username, &item.filename, item.bytes.clone())
                .await
            {
                warn!(%reason, filename = %item.filename, "media upload dropped");
                ops.drop_notice(&format!("media upload dropped: {reason}"))
                    .await;
            }
        }
    }
}

/// Resolve a route's webhook names into concrete URLs against the snapshot map.
fn resolve_targets(
    route: &ResolvedRoute,
    webhooks: &HashMap<WebhookName, WebhookUrl>,
) -> Vec<WebhookUrl> {
    route
        .to
        .iter()
        .filter_map(|name| {
            let url = webhooks.get(name).cloned();
            if url.is_none() {
                warn!(webhook = %name.0, route = %route.name, "route references unknown webhook");
            }
            url
        })
        .collect()
}

/// Like [`resolve_targets`] but also returns each webhook's name, needed to
/// record which webhook a message was posted to for later PATCHing.
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

/// Download a media file fully into memory via the chunked download iterator.
async fn download_media(client: &Client, media: &Media) -> anyhow::Result<Vec<u8>> {
    let mut download = client.iter_download(media);
    let mut bytes = Vec::new();
    while let Some(chunk) = download.next().await? {
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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
