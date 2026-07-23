<h1 align="center">telegram-relay</h1>

<p align="center">mirror telegram into discord, live — channels, groups, and DMs, as a real user over MTProto.</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-stable-CE422B?style=flat-square&logo=rust&labelColor=1a1a2e" alt="rust" />
  <img src="https://img.shields.io/github/actions/workflow/status/zaydiscold/telegram-relay/ci.yml?style=flat-square&labelColor=1a1a2e" alt="ci" />
  <img src="https://img.shields.io/badge/license-MIT-9b7dff?style=flat-square&labelColor=1a1a2e" alt="license" />
  <img src="https://img.shields.io/badge/telegram-MTProto-9b7dff?style=flat-square&logo=telegram&labelColor=1a1a2e" alt="mtproto" />
</p>

<p align="center">
  <a href="#how-fast">how fast</a> · <a href="#features">features</a> · <a href="#security--isolation">security</a> · <a href="#quickstart">quickstart</a> · <a href="#config-reference-configyaml">config</a> · <a href="#deploy">deploy</a>
</p>

<br>

A fast Telegram → Discord relay in Rust. It logs in as a real Telegram user over
MTProto — the same protocol the official Telegram Desktop app speaks — so it sees
exactly what your own client sees: **channels, groups, and DMs**. It mirrors the
chats you choose into Discord webhooks as branded, live-updating embeds. Single
binary, local-first, no hosted component.

A bot can't do this. Telegram bots only see chats they've been added to and are
blind to channels and DMs. Because this speaks MTProto as a user account, it can
watch any chat you can — the point of relaying a channel you read but your Discord
friends don't. Built to forward a crypto channel into a friend's Discord, running
unattended on a box that's always on.

## How fast

There's nothing to poll. MTProto holds a persistent connection and Telegram
*pushes* new messages down it, so the relay reacts the instant a message is
published rather than on an interval. End to end — Telegram publish to Discord
accept — is typically well under a second on a warm connection. The `stats`
command reports the real measured distribution (p50/p95/max) from your own
traffic, computed from two independent authoritative clocks (Telegram's publish
timestamp and Discord's message snowflake) — so it never depends on the relay
host's own clock.

## Features

| Feature | What it does |
|---|---|
| Live-updating embeds | Each post becomes a Discord embed with the source channel's name + photo as the webhook identity, a colored stripe, the original Telegram timestamp, a link back to Telegram, and a reaction/comment stats line. |
| State-aware colors | The stripe encodes state: regular posts are purple, an **edited** post turns orange, a **deleted** one turns red and says so. The color transitions in place as the source changes — read what happened without opening Telegram. |
| Reactions, edits, deletes | A background worker re-checks tracked posts and PATCHes the embed in place. Reactions settle over the first hour; edits and deletes tracked for two days. |
| Real media, inline | Photos and videos relay as attachments *inside* the embed. Multi-image albums coalesce into one gallery. Link-only posts relay as text so Discord renders its own preview. |
| Fan-in / fan-out | One source can feed several channels; several sources can feed one. Per-webhook dedup means adding a webhook to a route doesn't re-spam the others. |
| Per-route identity | Each route gets its own webhook avatar (the channel photo) and stripe color, so several sources funneled into one channel stay distinguishable. |
| Catch-up + dedup | Reconnects replay missed messages (Telegram's native catch-up); a durable store guards against double-posting, even across restarts. |
| Hot-reload | Routes and filters re-read from `config.yaml` on a timer — add a channel without restarting. |
| Backfill | `backfill <route> --count N` relays the last N posts of a channel on demand. |

## Security & isolation

Logging in as your account means being careful with the reach that grants:

- **Only the chats you configure are ever touched.** Every incoming update is
  matched against your routes *before* any work happens — a message from any
  other chat your account is in is dropped immediately, before a byte of its
  media is fetched.
- **Media never lands on disk.** Attachments are streamed into memory, posted,
  and dropped — never decoded, parsed, or executed. Per-route `mode: placeholder`
  relays a link instead and downloads nothing.
- **The session file is your account.** Written owner-only (`chmod 600`), never
  committed, never leaves the machine. Revoke any time from Telegram → Settings →
  Devices.
- **Webhook tokens never leak.** The types holding webhook URLs won't print them;
  every error, log line, and ops notice is URL-stripped first — enforced in the
  type system, not by convention.
- **Hardened service** (`ProtectSystem=strict`, `PrivateTmp=true`,
  `NoNewPrivileges=true`) and **no phone-home** — messages go only to the webhooks
  you configure.

## Quickstart

1. Get API credentials at <https://my.telegram.org> (API Development Tools → create
   an app) — an `api_id` and `api_hash`.
2. Create `.env` next to the binary:
   ```
   TELEGRAM_API_ID=12345
   TELEGRAM_API_HASH=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
   DISCORD_WEBHOOK_MAIN=https://discord.com/api/webhooks/…
   DISCORD_WEBHOOK_OPS=https://discord.com/api/webhooks/…   # optional
   ```
3. Copy `config.example.yaml` to `config.yaml` and set your routes.
4. Log in once (phone → code → optional 2FA): `telegram-relay login`
5. Inspect and validate (none of these send anything):
   ```
   telegram-relay chats     # every dialog + its id
   telegram-relay routes    # ASCII wiring diagram (fan-in / fan-out)
   telegram-relay check     # validate webhooks + routes; exit 0/1
   ```
6. Run it: `telegram-relay run`

## Commands

| Command | Purpose |
|---|---|
| `run` | Run the relay (what the service invokes). |
| `login` | One-time interactive login; writes the session file. |
| `chats` | List every dialog with its numeric id. |
| `routes` | ASCII diagram of the routing (source → webhooks); no session needed. |
| `check` | Validate config + webhooks (+ routes); exit 0/1. Sends nothing. |
| `stats` | Tracked-post counts and measured relay latency (p50/p95/max). |
| `backfill <route> [--count N]` | Relay the last N posts of a route on demand. |

## Config reference (`config.yaml`)

| Key | Meaning |
|---|---|
| `routes[].name` | Label for the route (used in logs). |
| `routes[].from` | Source chat: `"@username"` or a numeric chat id. |
| `routes[].to` | Webhook names (from `webhooks:`) to fan out to. |
| `routes[].color` | Optional `"#RRGGBB"` stripe for regular posts; defaults `#9b7dff`. Edited/deleted state colors override it. |
| `routes[].mode` | Optional `reupload` / `placeholder`; overrides the global `media.mode`. |
| `routes[].filter` | Optional `any_keywords` / `exclude_hashtags`. |
| `webhooks.<name>.env` | Env var holding that webhook's URL. |
| `ops_webhook.env` | Optional webhook for error/failure notices only. |
| `media.mode` | `reupload` (download + inline) or `placeholder` (link only). |
| `media.max_bytes` | Above this, fall back to a link instead of re-uploading. |
| `refresh.interval_mins` | Edit/delete re-check cadence (default 30). |
| `refresh.horizon_hours` | Stop tracking posts older than this (default 48). |
| `refresh.reaction_horizon_mins` | Stop refreshing reactions after this (default 60). |
| `store.path` | SQLite file tracking relayed posts (default `relay.db`). |

### Routing recipes

All three shapes work with no code — they're just how you write `routes`:

```yaml
# one source -> one channel
- { name: solo, from: "@alpha", to: [chan_a] }
# many sources -> one channel (fan-in)
- { name: a, from: "@alpha", to: [firehose] }
- { name: b, from: "@beta",  to: [firehose] }
# one source -> many channels (fan-out)
- { name: split, from: "@alpha", to: [chan_a, chan_b] }
```

## Deploy

`deploy/` has a systemd unit, a failure-alert unit that posts to your ops webhook
when the relay goes down (`telegram-relay-alert@.service` + `relay-alert.sh`), and
`status.sh` for a health dashboard. Build with `cargo build --release`, put `.env`,
`config.yaml`, and the session file next to the binary. `deploy/STATUS.md` documents
the boot chain. Silence means healthy — it posts only when a message is relayed or
something is actually wrong.

## Non-goals

- Not a full client (no sending from Discord back into Telegram).
- Not multi-account (one session per instance).
- Not a hosted service (no dashboard, no cloud).
- Not a bot integration — MTProto as a user account, on purpose, to watch chats a
  bot could never join.

<br>
<br>

<p align="left"><strong>zayd / cold</strong></p>

<p align="center">
  <a href="https://zayd.wtf">zayd.wtf</a> · <a href="https://x.com/coldcooks">twitter</a> · <a href="https://github.com/zaydiscold">github</a>
  <br>
  <em>icarus only fell because he flew</em>
</p>

<p align="right">
  <strong>to do</strong><br>
  <sub>
  ☑ live relay — channels, groups, DMs over MTProto<br>
  ☑ real media inline in the embed (single + album gallery)<br>
  ☑ state colors — purple / orange edited / red deleted<br>
  ☑ live-updating reactions, comments, edits, deletes<br>
  ☑ fan-in / fan-out routing with per-webhook dedup<br>
  ☑ catch-up + durable cross-restart dedup<br>
  ☑ measured end-to-end latency (<code>stats</code>)<br>
  ☑ systemd deploy + failure watchdog<br>
  ☐ cold-reboot test of the boot chain<br>
  ☐ refresh CDN image urls past the 24h signature window<br>
  ☐ optional Docker isolation for media<br>
  ☐ Telegram → Discord entity/markdown conversion
  </sub>
</p>

<p align="center"><sub>MIT — see <a href="LICENSE">LICENSE</a></sub></p>
