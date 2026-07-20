# telegram-relay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust daemon that mirrors chosen Telegram chats to Discord webhooks with a hot text lane (~150-400ms) and a background media lane.

**Architecture:** Single tokio process. grammers-client receives pushed MTProto updates; a pure router maps chat→routes; text renders and POSTs immediately on a warm reqwest pool; media (with album coalescing) uploads on a background lane. Pure modules (`config`, `router`, `render`, `dedup`) are unit-tested without network; `deliver` tests against a local mock HTTP server.

**Tech Stack:** Rust stable, grammers-client 0.10.x (pinned), tokio, reqwest, serde/serde_yaml, clap, tracing, lru.

## Global Constraints

- Crate versions pinned exactly in Cargo.toml (grammers is 0.x): `grammers-client = "=0.10.0"`, `grammers-session = "=0.10.0"` (adjust patch to latest 0.10.x at Task 1, then freeze).
- `.env`, `*.session`, `config.yaml` gitignored from first commit; only `config.example.yaml` committed. Webhook URLs are credentials.
- No AI attribution anywhere: no Co-Authored-By, no generated-with footers, in any commit or file.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` must pass before every commit.
- Errors are values in module APIs (`enum` outcomes); `panic!`/`unwrap` only in tests and `main` startup.
- Newtypes at boundaries: `ChatId(i64)`, `WebhookName(String)`; raw YAML types never leave `config`.
- Discord `content` hard limit 2000 chars; Telegram max 4096 — renderer must split.
- Binary name and repo name: `telegram-relay`. Repo stays local until user says ship.

---

### Task 1: Scaffold, CI, and grammers API verification

**Files:**
- Create: `Cargo.toml`, `src/main.rs` (stub), `.gitignore`, `config.example.yaml`, `.github/workflows/ci.yml`, `rustfmt.toml`
- Test: none (scaffold; `cargo check` is the gate)

**Interfaces:**
- Produces: workspace layout `src/{main,config,router,render,dedup,deliver,media,telegram}.rs` (modules added by later tasks); pinned dependency set.

- [ ] **Step 1: Verify toolchain and create project**

```bash
rustup show active-toolchain || rustup default stable
cd ~/Desktop/telegram-relay && cargo init --name telegram-relay
```
Expected: `Created binary (application) package`

- [ ] **Step 2: Check latest grammers 0.10.x patch and examples**

```bash
curl -s "https://crates.io/api/v1/crates/grammers-client" -H "User-Agent: telegram-relay-build" | python3 -c "import sys,json; print(json.load(sys.stdin)['crate']['max_version'])"
```
Then skim the client examples for the 0.10 API shape (Client::connect, sign-in flow, `client.next_update()`, `Update::NewMessage`) — repo moved off GitHub; use docs.rs: https://docs.rs/grammers-client/0.10.0 (Client, Update, types::Message). Record any signature differences from this plan in `docs/superpowers/plans/api-notes.md` and adapt later tasks accordingly.

- [ ] **Step 3: Write Cargo.toml**

```toml
[package]
name = "telegram-relay"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Fast Telegram → Discord webhook relay (MTProto userbot)"

[dependencies]
grammers-client = "=0.10.0"
grammers-session = "=0.10.0"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal", "sync", "time", "fs"] }
reqwest = { version = "0.12", features = ["json", "multipart", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
serde_json = "1"
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
lru = "0.12"
dotenvy = "0.15"
anyhow = "1"
thiserror = "2"

[dev-dependencies]
axum = "0.8"
```
(If Step 2 found a newer 0.10.x, pin that instead — with `=`.)

- [ ] **Step 4: .gitignore and config.example.yaml**

`.gitignore`:
```
/target
.env
*.session
config.yaml
```

`config.example.yaml`:
```yaml
routes:
  - name: robs-thoughts
    from: "@robthinks"          # @username or numeric chat id
    to: [discord_robs]          # fan-out: list more webhook names
    # filter:
    #   any_keywords: ["entry", "alert"]
    #   exclude_hashtags: ["#ad"]

webhooks:
  discord_robs: { env: DISCORD_WEBHOOK_ROBS }

ops_webhook: { env: DISCORD_WEBHOOK_OPS }   # optional: startup/error notices

media:
  mode: reupload            # reupload | placeholder
  max_bytes: 10000000
```

- [ ] **Step 5: Stub main.rs and CI**

`src/main.rs`:
```rust
fn main() {
    println!("telegram-relay: scaffold");
}
```

`.github/workflows/ci.yml`:
```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
```

`rustfmt.toml`: (empty file — defaults, committed so editors agree)

- [ ] **Step 6: Verify and commit**

```bash
cargo check && cargo fmt --check
git add -A && git commit -m "Scaffold cargo project, CI, config example"
```
Expected: `cargo check` finishes without errors (grammers compiles).

### Task 2: config module — parse, don't validate

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs` (add `mod config;`)
- Test: inline `#[cfg(test)] mod tests` in `src/config.rs`

**Interfaces:**
- Produces:
  - `ChatId(pub i64)` (derive Debug, Clone, Copy, PartialEq, Eq, Hash)
  - `WebhookName(pub String)`, `WebhookUrl(pub String)` (Url newtype keeps secrecy: manual `Debug` impl prints `WebhookUrl(«redacted»)`)
  - `ChatRef` enum: `Username(String)` | `Id(ChatId)` — config sources before resolution
  - `RouteCfg { name: String, from: ChatRef, to: Vec<WebhookName>, filter: Option<Filter> }`
  - `Filter { any_keywords: Vec<String>, exclude_hashtags: Vec<String> }`
  - `MediaCfg { mode: MediaMode, max_bytes: u64 }`, `enum MediaMode { Reupload, Placeholder }`
  - `Config { routes: Vec<RouteCfg>, webhooks: HashMap<WebhookName, WebhookUrl>, ops_webhook: Option<WebhookUrl>, media: MediaCfg }`
  - `Config::load(path: &Path) -> Result<Config, ConfigError>` — reads YAML + resolves `env:` refs via std::env; `ConfigError` (thiserror) names the exact bad field.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(yaml: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("tr-cfg-{}.yaml", std::process::id()));
        std::fs::write(&p, yaml).unwrap();
        p
    }

    #[test]
    fn loads_valid_config() {
        std::env::set_var("TEST_HOOK", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "routes:\n  - name: r1\n    from: \"@chan\"\n    to: [h1]\nwebhooks:\n  h1: { env: TEST_HOOK }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let c = Config::load(&p).unwrap();
        assert_eq!(c.routes.len(), 1);
        assert!(matches!(c.routes[0].from, ChatRef::Username(ref u) if u == "chan"));
    }

    #[test]
    fn numeric_from_parses_to_id() {
        std::env::set_var("TEST_HOOK", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "routes:\n  - name: r1\n    from: -1001234\n    to: [h1]\nwebhooks:\n  h1: { env: TEST_HOOK }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let c = Config::load(&p).unwrap();
        assert!(matches!(c.routes[0].from, ChatRef::Id(ChatId(-1001234))));
    }

    #[test]
    fn missing_env_names_the_field() {
        std::env::remove_var("NOPE_HOOK");
        let p = write_tmp(
            "routes: []\nwebhooks:\n  h1: { env: NOPE_HOOK }\nmedia: { mode: reupload, max_bytes: 1 }\n",
        );
        let e = Config::load(&p).unwrap_err().to_string();
        assert!(e.contains("NOPE_HOOK"), "error should name the env var: {e}");
    }

    #[test]
    fn route_to_unknown_webhook_rejected() {
        let p = write_tmp(
            "routes:\n  - name: r1\n    from: \"@c\"\n    to: [ghost]\nwebhooks: {}\nmedia: { mode: reupload, max_bytes: 1 }\n",
        );
        let e = Config::load(&p).unwrap_err().to_string();
        assert!(e.contains("ghost"));
    }

    #[test]
    fn webhook_url_debug_is_redacted() {
        let u = WebhookUrl("https://discord.com/api/webhooks/1/SECRET".into());
        assert!(!format!("{u:?}").contains("SECRET"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test config` — Expected: compile error (types not defined).

- [ ] **Step 3: Implement**

```rust
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WebhookName(pub String);

#[derive(Clone)]
pub struct WebhookUrl(pub String);

impl std::fmt::Debug for WebhookUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookUrl(«redacted»)")
    }
}

#[derive(Debug, Clone)]
pub enum ChatRef {
    Username(String), // stored without leading '@'
    Id(ChatId),
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub any_keywords: Vec<String>,
    pub exclude_hashtags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RouteCfg {
    pub name: String,
    pub from: ChatRef,
    pub to: Vec<WebhookName>,
    pub filter: Option<Filter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode { Reupload, Placeholder }

#[derive(Debug, Clone)]
pub struct MediaCfg { pub mode: MediaMode, pub max_bytes: u64 }

#[derive(Debug)]
pub struct Config {
    pub routes: Vec<RouteCfg>,
    pub webhooks: HashMap<WebhookName, WebhookUrl>,
    pub ops_webhook: Option<WebhookUrl>,
    pub media: MediaCfg,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("webhook '{name}': env var {env} is not set")]
    MissingEnv { name: String, env: String },
    #[error("route '{route}' references unknown webhook '{hook}'")]
    UnknownWebhook { route: String, hook: String },
    #[error("route '{route}': 'to' must list at least one webhook")]
    EmptyTo { route: String },
}

// ---- raw (serde) layer: never leaves this module ----
mod raw {
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Deserialize)]
    pub struct Root {
        pub routes: Vec<Route>,
        pub webhooks: HashMap<String, EnvRef>,
        pub ops_webhook: Option<EnvRef>,
        pub media: Media,
    }
    #[derive(Deserialize)]
    pub struct Route {
        pub name: String,
        pub from: serde_yaml::Value, // "@name" | "name" | -100123
        pub to: Vec<String>,
        pub filter: Option<Filter>,
    }
    #[derive(Deserialize, Default)]
    pub struct Filter {
        #[serde(default)] pub any_keywords: Vec<String>,
        #[serde(default)] pub exclude_hashtags: Vec<String>,
    }
    #[derive(Deserialize)]
    pub struct EnvRef { pub env: String }
    #[derive(Deserialize)]
    pub struct Media { pub mode: String, pub max_bytes: u64 }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(), source: e,
        })?;
        let raw: raw::Root = serde_yaml::from_str(&text)?;

        let mut webhooks = HashMap::new();
        for (name, r) in raw.webhooks {
            let url = std::env::var(&r.env).map_err(|_| ConfigError::MissingEnv {
                name: name.clone(), env: r.env.clone(),
            })?;
            webhooks.insert(WebhookName(name), WebhookUrl(url));
        }
        let ops_webhook = match raw.ops_webhook {
            None => None,
            Some(r) => Some(WebhookUrl(std::env::var(&r.env).map_err(|_| {
                ConfigError::MissingEnv { name: "ops_webhook".into(), env: r.env.clone() }
            })?)),
        };

        let mut routes = Vec::new();
        for r in raw.routes {
            if r.to.is_empty() {
                return Err(ConfigError::EmptyTo { route: r.name });
            }
            let to: Vec<WebhookName> = r.to.into_iter().map(WebhookName).collect();
            for h in &to {
                if !webhooks.contains_key(h) {
                    return Err(ConfigError::UnknownWebhook { route: r.name.clone(), hook: h.0.clone() });
                }
            }
            let from = match &r.from {
                serde_yaml::Value::Number(n) => ChatRef::Id(ChatId(n.as_i64().unwrap_or(0))),
                serde_yaml::Value::String(s) => ChatRef::Username(s.trim_start_matches('@').to_string()),
                other => ChatRef::Username(format!("{other:?}")), // rejected at resolve time
            };
            routes.push(RouteCfg {
                name: r.name, from, to,
                filter: r.filter.map(|f| Filter { any_keywords: f.any_keywords, exclude_hashtags: f.exclude_hashtags }),
            });
        }

        let mode = match raw.media.mode.as_str() {
            "placeholder" => MediaMode::Placeholder,
            _ => MediaMode::Reupload,
        };
        Ok(Config { routes, webhooks, ops_webhook, media: MediaCfg { mode, max_bytes: raw.media.max_bytes } })
    }
}
```

- [ ] **Step 4: Run tests** — `cargo test config` Expected: all PASS. (Note: env-var tests can race under the parallel test runner; use unique var names per test as shown.)

- [ ] **Step 5: Commit** — `git add -A && git commit -m "Add config module: typed parse with named errors"`

### Task 3: render module — markdown, prefixes, splitting

**Files:**
- Create: `src/render.rs`
- Modify: `src/main.rs` (add `mod render;`)
- Test: inline tests

**Interfaces:**
- Consumes: nothing (pure).
- Produces:
  - `RelayText { sender: Option<String>, body: String, reply_quote: Option<String>, edited: bool }`
  - `render(t: &RelayText) -> Vec<String>` — Discord-ready chunks, each ≤ 2000 chars, split on paragraph then char boundaries; sender prefix `**{sender}**: ` on first chunk; `(edited) ` prefix when `edited`; reply quote as leading `> {quote}\n` (quote truncated to 200 chars).
  - `passes_filter(body: &str, f: &Filter) -> bool` (case-insensitive keyword any-match; hashtag exclusion wins).

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Filter;

    fn rt(body: &str) -> RelayText {
        RelayText { sender: Some("Rob".into()), body: body.into(), reply_quote: None, edited: false }
    }

    #[test]
    fn short_message_single_chunk_with_sender() {
        let out = render(&rt("gm"));
        assert_eq!(out, vec!["**Rob**: gm".to_string()]);
    }

    #[test]
    fn long_message_splits_under_2000() {
        let body = "para\n\n".repeat(400); // ~2400 chars
        let out = render(&rt(&body));
        assert!(out.len() >= 2);
        assert!(out.iter().all(|c| c.chars().count() <= 2000));
    }

    #[test]
    fn edited_prefix_and_reply_quote() {
        let t = RelayText { sender: None, body: "hi".into(), reply_quote: Some("orig".into()), edited: true };
        let out = render(&t);
        assert!(out[0].starts_with("> orig\n"));
        assert!(out[0].contains("(edited) hi"));
    }

    #[test]
    fn filter_keyword_and_hashtag() {
        let f = Filter { any_keywords: vec!["entry".into()], exclude_hashtags: vec!["#ad".into()] };
        assert!(passes_filter("ENTRY now", &f));
        assert!(!passes_filter("random", &f));
        assert!(!passes_filter("entry #ad", &f)); // exclusion wins
    }

    #[test]
    fn empty_filter_passes_everything() {
        assert!(passes_filter("anything", &Filter::default()));
    }
}
```

- [ ] **Step 2: Run** — `cargo test render` Expected: FAIL (unresolved).

- [ ] **Step 3: Implement**

```rust
use crate::config::Filter;

const DISCORD_LIMIT: usize = 2000;

#[derive(Debug, Clone)]
pub struct RelayText {
    pub sender: Option<String>,
    pub body: String,
    pub reply_quote: Option<String>,
    pub edited: bool,
}

pub fn passes_filter(body: &str, f: &Filter) -> bool {
    let lower = body.to_lowercase();
    if f.exclude_hashtags.iter().any(|h| lower.contains(&h.to_lowercase())) {
        return false;
    }
    if f.any_keywords.is_empty() {
        return true;
    }
    f.any_keywords.iter().any(|k| lower.contains(&k.to_lowercase()))
}

pub fn render(t: &RelayText) -> Vec<String> {
    let mut head = String::new();
    if let Some(q) = &t.reply_quote {
        let q: String = q.chars().take(200).collect();
        head.push_str(&format!("> {}\n", q.replace('\n', " ")));
    }
    if let Some(s) = &t.sender {
        head.push_str(&format!("**{s}**: "));
    }
    if t.edited {
        head.push_str("(edited) ");
    }
    let full = format!("{head}{}", t.body);
    split_chunks(&full, DISCORD_LIMIT)
}

fn split_chunks(s: &str, limit: usize) -> Vec<String> {
    if s.chars().count() <= limit {
        return vec![s.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for para in s.split("\n\n") {
        let candidate_len = cur.chars().count() + para.chars().count() + 2;
        if !cur.is_empty() && candidate_len > limit {
            chunks.push(std::mem::take(&mut cur));
        }
        if para.chars().count() > limit {
            // paragraph itself too big: hard-split on chars
            for piece in para.chars().collect::<Vec<_>>().chunks(limit) {
                let piece: String = piece.iter().collect();
                if !cur.is_empty() { chunks.push(std::mem::take(&mut cur)); }
                cur = piece;
                chunks.push(std::mem::take(&mut cur));
            }
        } else {
            if !cur.is_empty() { cur.push_str("\n\n"); }
            cur.push_str(para);
        }
    }
    if !cur.is_empty() { chunks.push(cur); }
    chunks
}
```

- [ ] **Step 4: Run** — `cargo test render` Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -am "Add render module: chunking, prefixes, filters"`

### Task 4: dedup + router

**Files:**
- Create: `src/dedup.rs`, `src/router.rs`
- Modify: `src/main.rs` (mods)
- Test: inline in each

**Interfaces:**
- Consumes: `config::{ChatId, RouteCfg, WebhookName}`.
- Produces:
  - `Dedup::new(cap: usize)`; `Dedup::check_and_insert(&mut self, chat: ChatId, msg_id: i32) -> bool` (true = fresh, false = duplicate).
  - `Router::new(routes: Vec<ResolvedRoute>)`; `Router::match_chat(&self, chat: ChatId) -> &[ResolvedRoute]`
  - `ResolvedRoute { name: String, chat: ChatId, to: Vec<WebhookName>, filter: Option<Filter> }` — produced at startup by resolving `ChatRef::Username` via Telegram (Task 6); router itself is pure.

- [ ] **Step 1: Failing tests**

```rust
// src/dedup.rs tests
#[test]
fn fresh_then_duplicate() {
    let mut d = Dedup::new(8);
    assert!(d.check_and_insert(ChatId(1), 100));
    assert!(!d.check_and_insert(ChatId(1), 100));
    assert!(d.check_and_insert(ChatId(2), 100)); // same msg id, other chat
}

#[test]
fn evicts_at_capacity() {
    let mut d = Dedup::new(2);
    d.check_and_insert(ChatId(1), 1);
    d.check_and_insert(ChatId(1), 2);
    d.check_and_insert(ChatId(1), 3); // evicts (1,1)
    assert!(d.check_and_insert(ChatId(1), 1)); // fresh again
}

// src/router.rs tests
#[test]
fn matches_only_configured_chat() {
    let r = Router::new(vec![ResolvedRoute {
        name: "r".into(), chat: ChatId(5), to: vec![WebhookName("h".into())], filter: None,
    }]);
    assert_eq!(r.match_chat(ChatId(5)).len(), 1);
    assert!(r.match_chat(ChatId(6)).is_empty());
}

#[test]
fn one_chat_two_routes_fan_out() {
    let mk = |n: &str| ResolvedRoute { name: n.into(), chat: ChatId(5), to: vec![WebhookName(n.into())], filter: None };
    let r = Router::new(vec![mk("a"), mk("b")]);
    assert_eq!(r.match_chat(ChatId(5)).len(), 2);
}
```

- [ ] **Step 2: Run** — `cargo test dedup router` Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
// src/dedup.rs
use crate::config::ChatId;
use lru::LruCache;
use std::num::NonZeroUsize;

pub struct Dedup(LruCache<(ChatId, i32), ()>);

impl Dedup {
    pub fn new(cap: usize) -> Self {
        Dedup(LruCache::new(NonZeroUsize::new(cap.max(1)).unwrap()))
    }
    /// Returns true if (chat, msg_id) was not seen before.
    pub fn check_and_insert(&mut self, chat: ChatId, msg_id: i32) -> bool {
        self.0.put((chat, msg_id), ()).is_none()
    }
}
```

```rust
// src/router.rs
use crate::config::{ChatId, Filter, WebhookName};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub name: String,
    pub chat: ChatId,
    pub to: Vec<WebhookName>,
    pub filter: Option<Filter>,
}

pub struct Router(HashMap<ChatId, Vec<ResolvedRoute>>);

impl Router {
    pub fn new(routes: Vec<ResolvedRoute>) -> Self {
        let mut m: HashMap<ChatId, Vec<ResolvedRoute>> = HashMap::new();
        for r in routes {
            m.entry(r.chat).or_default().push(r);
        }
        Router(m)
    }
    pub fn match_chat(&self, chat: ChatId) -> &[ResolvedRoute] {
        self.0.get(&chat).map(Vec::as_slice).unwrap_or(&[])
    }
}
```

- [ ] **Step 4: Run** — `cargo test dedup router` Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -am "Add dedup LRU and pure router"`

### Task 5: deliver module — webhook POST, rate limit, retries

**Files:**
- Create: `src/deliver.rs`
- Modify: `src/main.rs` (mod)
- Test: `tests/deliver_test.rs` (integration, axum mock server)

**Interfaces:**
- Consumes: `config::WebhookUrl`.
- Produces:
  - `Deliverer::new() -> Deliverer` (owns a `reqwest::Client` built with `pool_idle_timeout(None)`, `tcp_keepalive(Some(Duration::from_secs(30)))`).
  - `enum Outcome { Delivered, Dropped { reason: String } }`
  - `async fn post_text(&self, url: &WebhookUrl, username: &str, chunks: &[String]) -> Outcome` — sequential POSTs (order matters); per-request: on 429 read `retry_after` (Discord returns JSON body `retry_after` seconds and/or `Retry-After` header), sleep, retry; on 5xx exponential backoff 250ms/1s/4s then `Dropped`; on other 4xx `Dropped` immediately (body logged).
  - `async fn post_file(&self, url: &WebhookUrl, username: &str, filename: &str, bytes: Vec<u8>) -> Outcome` — multipart `files[0]`.
  - JSON body: `{ "content": ..., "username": ... }` with `?wait=true` appended to URL (makes Discord return 200+body instead of 204, and surfaces errors).

- [ ] **Step 1: Failing integration test**

```rust
// tests/deliver_test.rs
use axum::{extract::State, http::StatusCode, routing::post, Router};
use std::sync::{Arc, Mutex};
use telegram_relay::config::WebhookUrl;
use telegram_relay::deliver::{Deliverer, Outcome};

#[derive(Clone, Default)]
struct Hits(Arc<Mutex<Vec<String>>>);

async fn ok_handler(State(h): State<Hits>, body: String) -> StatusCode {
    h.0.lock().unwrap().push(body);
    StatusCode::OK
}

async fn ratelimit_then_ok(State(h): State<Hits>, body: String) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let mut g = h.0.lock().unwrap();
    g.push(body);
    if g.len() == 1 {
        (StatusCode::TOO_MANY_REQUESTS, [("Retry-After", "0")], r#"{"retry_after": 0.05}"#.into())
    } else {
        (StatusCode::OK, [("Retry-After", "0")], "{}".into())
    }
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/hook")
}

#[tokio::test]
async fn posts_chunks_in_order() {
    let hits = Hits::default();
    let url = spawn(Router::new().route("/hook", post(ok_handler)).with_state(hits.clone())).await;
    let d = Deliverer::new();
    let out = d.post_text(&WebhookUrl(url), "Rob", &["one".into(), "two".into()]).await;
    assert!(matches!(out, Outcome::Delivered));
    let bodies = hits.0.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("one") && bodies[1].contains("two"));
}

#[tokio::test]
async fn retries_on_429() {
    let hits = Hits::default();
    let url = spawn(Router::new().route("/hook", post(ratelimit_then_ok)).with_state(hits.clone())).await;
    let d = Deliverer::new();
    let out = d.post_text(&WebhookUrl(url), "Rob", &["x".into()]).await;
    assert!(matches!(out, Outcome::Delivered));
    assert_eq!(hits.0.lock().unwrap().len(), 2); // 429 then 200
}
```
Note: this requires `src/main.rs` → move shared mods into `src/lib.rs` (`pub mod config; pub mod render; pub mod dedup; pub mod router; pub mod deliver; pub mod media;`) with `main.rs` using `telegram_relay::…`. Do that in this task.

- [ ] **Step 2: Run** — `cargo test --test deliver_test` Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
// src/deliver.rs
use crate::config::WebhookUrl;
use std::time::Duration;
use tracing::warn;

pub struct Deliverer {
    http: reqwest::Client,
}

#[derive(Debug)]
pub enum Outcome {
    Delivered,
    Dropped { reason: String },
}

impl Default for Deliverer {
    fn default() -> Self { Self::new() }
}

impl Deliverer {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .pool_idle_timeout(None)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Deliverer { http }
    }

    pub async fn post_text(&self, url: &WebhookUrl, username: &str, chunks: &[String]) -> Outcome {
        for chunk in chunks {
            let body = serde_json::json!({ "content": chunk, "username": username });
            match self.send(url, |u| self.http.post(u).json(&body)).await {
                Outcome::Delivered => {}
                dropped => return dropped,
            }
        }
        Outcome::Delivered
    }

    pub async fn post_file(&self, url: &WebhookUrl, username: &str, filename: &str, bytes: Vec<u8>) -> Outcome {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let payload = serde_json::json!({ "username": username }).to_string();
        let form = reqwest::multipart::Form::new()
            .part("files[0]", part)
            .text("payload_json", payload);
        // multipart Form is not clonable; single attempt + one retry on 429 only
        self.send_once_with_429_retry(url, form).await
    }

    async fn send<F>(&self, url: &WebhookUrl, build: F) -> Outcome
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let target = format!("{}?wait=true", url.0);
        let backoffs = [Duration::from_millis(250), Duration::from_secs(1), Duration::from_secs(4)];
        let mut attempt = 0;
        loop {
            let resp = build(&target).send().await;
            match resp {
                Ok(r) if r.status().is_success() => return Outcome::Delivered,
                Ok(r) if r.status().as_u16() == 429 => {
                    let wait = retry_after_secs(&r).await.unwrap_or(1.0);
                    warn!(wait, "discord 429; honoring retry_after");
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    // 429 does not consume a backoff attempt
                }
                Ok(r) if r.status().is_server_error() => {
                    if attempt >= backoffs.len() {
                        return Outcome::Dropped { reason: format!("5xx after retries: {}", r.status()) };
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    return Outcome::Dropped { reason: format!("{status}: {body}") };
                }
                Err(e) => {
                    if attempt >= backoffs.len() {
                        return Outcome::Dropped { reason: format!("network: {e}") };
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn send_once_with_429_retry(&self, url: &WebhookUrl, form: reqwest::multipart::Form) -> Outcome {
        let target = format!("{}?wait=true", url.0);
        let resp = self.http.post(&target).multipart(form).send().await;
        match resp {
            Ok(r) if r.status().is_success() => Outcome::Delivered,
            Ok(r) => Outcome::Dropped { reason: format!("upload failed: {}", r.status()) },
            Err(e) => Outcome::Dropped { reason: format!("upload network: {e}") },
        }
    }
}

async fn retry_after_secs(r: &reqwest::Response) -> Option<f64> {
    r.headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}
```
(Note: reading JSON `retry_after` from the body consumes the response; header-only is sufficient — Discord sets both. Keep the helper header-based.)

- [ ] **Step 4: Run** — `cargo test --test deliver_test` Expected: PASS. Also `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit** — `git commit -am "Add deliver module: warm pool, 429 honor, bounded retries; split lib/bin"`

### Task 6: telegram module — client, login, resolve, updates, catch-up

This task is the grammers integration; it is deliberately thin and mostly untestable without a live account. Verify signatures against docs.rs (Task 1 notes) and adapt.

**Files:**
- Create: `src/telegram.rs`
- Modify: `src/lib.rs` (mod), `src/main.rs`
- Test: manual (live) — automated tests cover everything around this module.

**Interfaces:**
- Consumes: `config::{Config, ChatRef, ChatId}`, `router::ResolvedRoute`.
- Produces:
  - `async fn connect(api_id: i32, api_hash: &str, session_path: &Path) -> anyhow::Result<grammers_client::Client>` — uses `grammers_session::Session::load_file_or_create`, `Client::connect`, `client.is_authorized()`.
  - `async fn interactive_login(client: &Client) -> anyhow::Result<()>` — phone prompt → `request_login_code` → code prompt → `sign_in`; on `SignInError::PasswordRequired` prompt for the 2FA password via `rpassword`-style hidden stdin (add `rpassword = "7"` dep). Saves session after.
  - `async fn resolve_routes(client: &Client, cfg: &Config) -> anyhow::Result<Vec<ResolvedRoute>>` — `client.resolve_username` for `ChatRef::Username`; logs `route '{name}' watching '{title}' ({id})` per route.
  - `async fn list_chats(client: &Client) -> anyhow::Result<()>` — iterate `client.iter_dialogs()`, print `id  type  title` table (the `chats` CLI verb).
  - `enum Incoming { Text { chat: ChatId, msg_id: i32, sender: Option<String>, body: String, reply_quote: Option<String>, edited: bool }, Media { chat: ChatId, msg_id: i32, grouped_id: Option<i64>, media: grammers_client::types::Media, caption: String, sender: Option<String>, approx_size: u64, deep_link: Option<String> } }`
  - `fn classify(update: grammers_client::Update) -> Option<Incoming>` — NewMessage/MessageEdited → Incoming; everything else None. Deep link built as `https://t.me/{username}/{msg_id}` when the chat has a public username.
- Catch-up: enable grammers' update catch-up if exposed by 0.10 (`InitParams { catch_up: true, .. }` on `Client::connect` config); if the API differs, record in api-notes.md and implement get-history-since-last-seen per watched chat at startup (last-seen persisted to `state.json` beside the session, written at most every 10s).

- [ ] **Step 1: Implement `connect` + `interactive_login` + `list_chats`** (code per docs.rs; commit checkpoint after `cargo check`)
- [ ] **Step 2: Implement `resolve_routes` + `classify`**
- [ ] **Step 3: Manual smoke test (requires api creds in `.env`):**

Run: `cargo run -- chats` (after Task 7 CLI exists — if executing tasks in order, defer this smoke test to Task 7 Step 4 and just `cargo check` here).
Expected: dialog table including `@robthinks`.

- [ ] **Step 4: Commit** — `git commit -am "Add telegram module: login, resolve, classify, catch-up"`

### Task 7: CLI + main wiring + media lane + ops notices

**Files:**
- Create: `src/media.rs`, `src/cli.rs`
- Modify: `src/main.rs` (full wiring), `src/lib.rs`
- Test: `render`/`router` level covered; `media` album coalescing unit-tested with a fake clock (tokio `test(start_paused = true)`).

**Interfaces:**
- Consumes: everything prior.
- Produces:
  - clap CLI: `telegram-relay run [--config config.yaml]`, `login`, `chats`, `check`.
  - `check`: load config → resolve env → (if session exists) connect+resolve routes → GET each webhook URL (Discord returns webhook JSON on GET without sending a message) → print table, exit 0/1.
  - `media::AlbumBuffer::push(item: MediaItem) -> Option<Vec<MediaItem>>` semantics: items with `grouped_id=None` flush immediately; grouped items buffer until 1s passes with no new sibling (tokio timer), then flush as one batch → sequential `post_file` calls.
  - main loop (run): `loop { tokio::select! { upd = client.next_update() => dispatch(upd), _ = sigterm => break } }`; dispatch = classify → dedup → router → filter → spawn text task (render+post_text) / push media task. Ops webhook (if set) receives `started`, `shutting down`, `route resolution failed`, and drop notices (rate-limited to 1/min). Heartbeat `info!` every 300s via `tokio::time::interval`.
- Hot-reload: `run` watches config.yaml mtime every 5s; on change, re-load + re-resolve routes, swap `Router` behind `arc_swap` (add `arc-swap = "1"`) — creds/session changes still require restart.

- [ ] **Step 1: Failing album test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn ungrouped_flushes_immediately() {
        let mut b = AlbumBuffer::new(std::time::Duration::from_secs(1));
        let out = b.push(fake_item(None, 1)).await;
        assert_eq!(out.map(|v| v.len()), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn grouped_coalesces_within_window() {
        let mut b = AlbumBuffer::new(std::time::Duration::from_secs(1));
        assert!(b.push(fake_item(Some(7), 1)).await.is_none());
        assert!(b.push(fake_item(Some(7), 2)).await.is_none());
        tokio::time::advance(std::time::Duration::from_millis(1100)).await;
        let out = b.tick().await; // timer-driven flush
        assert_eq!(out.map(|v| v.len()), Some(2));
    }
}
```
(`fake_item(grouped_id, msg_id)` constructs a `MediaItem` with stub bytes; `MediaItem` holds pre-downloaded bytes + filename + route info so the buffer is testable without grammers types.)

- [ ] **Step 2: Implement `media.rs`** (AlbumBuffer with `HashMap<i64, (Vec<MediaItem>, Instant)>` + `tick()` polled from the main select loop every 250ms; oversized check happens before download: `approx_size > cfg.max_bytes` → render text notice with deep link instead).
- [ ] **Step 3: Implement `cli.rs` + full `main.rs` wiring** (clap derive; `run` assembles Config→connect→resolve→Router→loop; `login`/`chats`/`check` as described).
- [ ] **Step 4: Full check + live smoke:**

```bash
cargo clippy --all-targets -- -D warnings && cargo test
cargo run -- login        # one-time, interactive
cargo run -- chats | head -40
cargo run -- check
```
Expected: all green; `chats` lists dialogs; `check` exit 0.

- [ ] **Step 5: Commit** — `git commit -am "Add CLI, media lane with album coalescing, main wiring, ops notices"`

### Task 7b: store module + refresh worker + embed rendering

**Files:**
- Create: `src/store.rs`, `src/refresh.rs`
- Modify: `src/render.rs` (embed builder + footer), `src/deliver.rs` (capture `discord_msg_id` from `?wait=true` JSON; add `patch_embed`), `src/main.rs` (spawn refresh task), `Cargo.toml` (`rusqlite = { version = "0.32", features = ["bundled"] }`, `rand = "0.8"`)
- Test: inline (store CRUD against a temp-file DB; render embed snapshot tests; refresh diff logic with fake fetcher)

**Interfaces:**
- Produces:
  - `Store::open(path: &Path) -> Result<Store>` — creates schema (WAL mode):
    ```sql
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
    ```
  - `Store::record(&self, rec: NewRecord)`, `Store::due(&self, horizon_hours: u64) -> Vec<TrackedMsg>`, `Store::update_stats(&self, ...)`, `Store::mark_deleted(&self, ...)`, `Store::prune(&self, horizon_hours: u64)`
  - `render::embed(post: &RelayText, meta: &EmbedMeta) -> serde_json::Value` — Discord embed JSON: author = channel title + avatar url, description = body (split across multiple embeds if > 4096), fields line `❤️ 47 · 🔥 12 · 💬 8`, `↗ View on Telegram` masked link, footer `by zayd — {variant}` with `variant = FOOTERS[rand::random_range(0..FOOTERS.len())]`; `FOOTERS: &[&str]` ~15 "let's get this bag" variations.
  - `deliver::post_embed(...) -> PostResult { Delivered { discord_msg_id: String }, Dropped { reason } }` (parse `id` from `?wait=true` response body)
  - `deliver::patch_embed(&self, url: &WebhookUrl, discord_msg_id: &str, embed: serde_json::Value) -> Outcome` — `PATCH {webhook}/messages/{id}`
  - `refresh::run(client, store, deliverer, cfg)` — `tokio::time::interval(cfg.refresh.interval_mins * 60s)`; per chat: batch `client.get_messages_by_id` (≤100 ids); missing → `mark_deleted` + PATCH strikethrough `🗑 deleted on Telegram`; hash change → PATCH `(edited)` body; reactions/comment-count change → PATCH stats line; update `last_checked`; then `prune`.
- Config additions (`config.example.yaml`): `refresh: { interval_mins: 30, horizon_hours: 48 }`; `store: { path: relay.db }`.

- [ ] **Step 1: failing store tests** (record → due → update → prune roundtrip on tempfile DB)
- [ ] **Step 2: implement store.rs; tests pass; commit** `git commit -am "Add sqlite message store"`
- [ ] **Step 3: failing embed render tests** (footer from list, deep-link present, >4096 body splits, stats line formatting)
- [ ] **Step 4: implement embed builder + FOOTERS; switch hot path from content to embed; tests pass; commit** `git commit -am "Render as embeds with footer + deep link"`
- [ ] **Step 5: implement post_embed id capture + patch_embed; extend deliver_test with PATCH mock; commit** `git commit -am "Capture discord message ids; add embed PATCH"`
- [ ] **Step 6: implement refresh worker with fetcher trait (`trait PostFetcher` so tests inject fakes); diff-logic tests pass; wire into main; commit** `git commit -am "Add refresh worker: reactions, comments, edits, deletes"`

### Task 8: Live acceptance on frostbyte

**Files:**
- Create: `config.yaml` (local, gitignored) with a test route: your own Saved Messages → a throwaway test webhook.

- [ ] **Step 1:** `cargo run -- run` with test route; send yourself a text message → appears in Discord < 1s.
- [ ] **Step 2:** Send a photo album (3 images) → arrives as one coalesced batch after ~1s.
- [ ] **Step 3:** Send a 3000-char message → arrives as 2 ordered chunks.
- [ ] **Step 4:** Switch route to `@robthinks` + real webhook; observe next real post relay.
- [ ] **Step 5:** Kill -TERM the process → ops webhook shows "shutting down"; restart → "started", no duplicate posts (dedup + catch-up).
- [ ] **Step 6:** Commit any fixes; tag `v0.1.0`: `git tag v0.1.0`.

### Task 9: Deploy to mothership WSL + systemd + boot hardening

**Files:**
- Create: `deploy/telegram-relay.service`, `deploy/deploy.sh`, `deploy/README.md`

**Interfaces:**
- Consumes: the release binary; frostbyte `~/.ssh/config` alias `mothership-wsl` (port 2222).

- [ ] **Step 1: Build for Linux x86_64** — simplest reliable path: build **on** the target: `ssh mothership-wsl 'command -v cargo || curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'`, then `git bundle` or rsync the repo over and `cargo build --release` there. (Cross-compiling macOS-arm→linux-x86_64 needs a linker toolchain; native build is the boring, correct choice.)
- [ ] **Step 2: Ship state** — `scp .env config.yaml *.session mothership-wsl:~/telegram-relay/` ; `chmod 600` both there.
- [ ] **Step 3: systemd unit** (`/etc/systemd/system/telegram-relay.service`):

```ini
[Unit]
Description=Telegram to Discord relay
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
User=zaydk
WorkingDirectory=/home/zaydk/telegram-relay
ExecStart=/home/zaydk/telegram-relay/target/release/telegram-relay run
Restart=always
RestartSec=2
Environment=RUST_LOG=info
# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/home/zaydk/telegram-relay
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```
`sudo systemctl enable --now telegram-relay && systemctl status telegram-relay`

- [ ] **Step 4: WSL boot keepalive (Windows side)** — Task Scheduler entry "WSL-keepalive": trigger At startup, action `wsl.exe -d Ubuntu --exec /bin/true`, run whether user is logged on or not. Verify WSL systemd is enabled (`/etc/wsl.conf` has `[boot]\nsystemd=true`).
- [ ] **Step 5: Acceptance** — reboot mothership; confirm relay posts the "started" ops notice unattended; frostbyte dev session revoked or kept per user preference; note the Telegram "new login from new IP" notification is expected once.
- [ ] **Step 6: Commit deploy artifacts** — `git add deploy && git commit -m "Add systemd unit and deployment runbook"`

### Task 10: README + ship gate (deferred until user says ship)

- [ ] README: what it is, 4 CLI verbs, my.telegram.org walkthrough, config reference, systemd/WSL runbook, session-security note (Active Sessions revocation), non-goals. MIT LICENSE file.
- [ ] `gh repo create zaydiscold/telegram-relay --public --source . --push` — **only after explicit go-ahead.**
- [ ] Any grammers bugs found during Tasks 6-8 → minimal repro → upstream PR (no AI attribution).

## Self-Review Notes

- Spec coverage: config/router/render/deliver/media/dedup/catchup/CLI/systemd all mapped (Tasks 2-9); ops webhook Task 7; enumerate-chats Task 6/7; blind-spot items (albums Task 7, splitting Task 3, ops alerting Task 7, 2FA Task 6, boot ordering Task 9) covered. Deleted-message mirroring: documented non-goal (Task 10 README).
- grammers API risk isolated to Task 6 with explicit docs.rs verification and api-notes.md escape hatch.
- Types consistent: `ChatId(i64)` everywhere; `Incoming` msg ids `i32` (Telegram message ids are i32); `Dedup` keyed on `(ChatId, i32)`.
