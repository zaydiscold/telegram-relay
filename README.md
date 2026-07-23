# telegram-relay

> Mirror Telegram into Discord, live.

![Rust](https://img.shields.io/badge/rust-stable-CE422B?style=flat-square&logo=rust&labelColor=1a1a2e)
![CI](https://img.shields.io/github/actions/workflow/status/zaydiscold/telegram-relay/ci.yml?style=flat-square&labelColor=1a1a2e)
![License](https://img.shields.io/badge/license-MIT-9b7dff?style=flat-square&labelColor=1a1a2e)
![MTProto](https://img.shields.io/badge/telegram-MTProto-9b7dff?style=flat-square&logo=telegram&labelColor=1a1a2e)

A fast Telegram → Discord relay in Rust. It logs in as a real Telegram user over
MTProto — the same protocol the official Telegram Desktop app speaks — so it sees
exactly what your own client sees: **channels, groups, and DMs**. It mirrors the
chats you choose into Discord webhooks as branded, live-updating embeds. Single
binary, local-first, no hosted component.

A bot can't do this. Telegram bots only see chats they've been added to and are
blind to channels and DMs. Because this speaks MTProto as a user account, it can
watch any chat you can — which is the point of relaying a channel you read but
your Discord friends don't.

Built to forward a crypto channel into a friend's Discord. Runs unattended on a
box that's always on.

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
| State-aware colors | The stripe encodes state: regular posts are purple, an **edited** post turns orange, a **deleted** one turns red and says so. The color transitions in place as the source changes — you can read what happened without opening Telegram. |
| Reactions, edits, deletes | A background worker re-checks tracked posts and PATCHes the embed in place. Reactions settle over the first hour; edits and deletes are tracked for two days. |
| Real media, inline | Photos and videos are relayed as attachments *inside* the embed. Multi-image albums coalesce into one gallery message. Link-only posts relay as text so Discord renders its own preview. |
| Fan-in / fan-out | One source can feed several Discord channels; several sources can feed one channel. Per-webhook dedup means adding a webhook to an existing route doesn't re-spam the others. |
| Per-route identity | Each route gets its own webhook avatar (the channel photo) and stripe color, so several sources funneled into one Discord channel stay distinguishable. |
| Catch-up + dedup | Reconnects replay missed messages (Telegram's native catch-up); a durable store guards against posting the same message twice, even across restarts. |
| Hot-reload | Routes and filters re-read from `config.yaml` on a timer — add a channel without restarting. |
| Backfill | `backfill <route> --count N` relays the last N posts of a channel on demand. |
| Signature footer | Every embed carries a small rotating `by <name> — <line>` footer, drawn from a built-in list. |

## Security & isolation

This logs in as your account, so it's built to be careful with the reach that
grants:

- **Only the chats you configure are ever touched.** Every incoming update is
  matched against your routes *before* any work happens — a message from any
  other chat your account is in is dropped immediately, before a single byte of
  its media is fetched. The relay does no work for chats you didn't ask it to
  watch.
- **Media never lands on disk.** Attachments are streamed into memory, posted to
  Discord, and dropped — never decoded, parsed, or executed. For the cautious,
  per-route `mode: placeholder` relays a link instead and downloads nothing.
- **The session file is your account.** Written owner-only (`chmod 600`), never
  committed, never leaves the machine. Revoke it any time from Telegram →
  Settings → Devices.
- **Webhook tokens never leak.** The types that hold webhook URLs won't print
  them; every error, log line, and ops notice is stripped of the URL first —
  enforced in the type system, not by convention.
- **Hardened service.** The systemd unit runs `ProtectSystem=strict`,
  `PrivateTmp=true`, `NoNewPrivileges=true`.
- **No phone-home.** Messages go to the Discord webhooks you configure and
  nowhere else. No analytics, no external logging.

## Quickstart

1. Get API credentials at <https://my.telegram.org> (API Development Tools →
   create an app) — an `api_id` and `api_hash`.
2. Create `.env` next to the binary:
   ```
   TELEGRAM_API_ID=12345
   TELEGRAM_API_HASH=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
   DISCORD_WEBHOOK_MAIN=https://discord.com/api/webhooks/…
   DISCORD_WEBHOOK_OPS=https://discord.com/api/webhooks/…   # optional
   ```
3. Copy `config.example.yaml` to `config.yaml` and set your routes.
4. Log in once (phone → code → optional 2FA):
   ```
   telegram-relay login
   ```
5. Inspect and validate (none of these send anything):
   ```
   telegram-relay chats     # every dialog + its id, to fill in routes
   telegram-relay routes    # ASCII diagram of the wiring (fan-in / fan-out)
   telegram-relay check     # validate webhooks + route resolution; exit 0/1
   ```
6. Run it:
   ```
   telegram-relay run
   ```

## Commands

| Command | Purpose |
|---|---|
| `run` | Run the relay (what the service invokes). |
| `login` | One-time interactive login; writes the session file. |
| `chats` | List every dialog with its numeric id. |
| `routes` | Print an ASCII diagram of the routing (source → webhooks), no session needed. |
| `check` | Validate config + webhooks (+ route resolution); exit 0/1. Sends nothing. |
| `stats` | Tracked-post counts and measured relay latency (p50/p95/max). |
| `backfill <route> [--count N]` | Relay the last N posts of a route on demand. |

## Config reference (`config.yaml`)

| Key | Meaning |
|---|---|
| `routes[].name` | Label for the route (used in logs). |
| `routes[].from` | Source chat: `"@username"` or a numeric chat id. |
| `routes[].to` | Webhook names (from `webhooks:`) to fan out to. |
| `routes[].color` | Optional `"#RRGGBB"` stripe for regular posts; defaults to `#9b7dff`. Edited/deleted state colors override it. |
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

## Routing recipes

All three shapes work with no code — they're just how you write `routes`:

```yaml
# one source -> one channel
routes:
  - { name: solo, from: "@alpha", to: [chan_a] }

# many sources -> one channel (fan-in / aggregation)
routes:
  - { name: a, from: "@alpha",  to: [firehose] }
  - { name: b, from: "@beta",   to: [firehose] }

# one source -> many channels (fan-out)
routes:
  - { name: split, from: "@alpha", to: [chan_a, chan_b] }
```

`telegram-relay routes` prints the resulting wiring and flags every fan-in and
fan-out.

## Deploy

`deploy/` has a systemd unit (`telegram-relay.service`), a failure-alert unit
that posts to your ops webhook when the relay goes down
(`telegram-relay-alert@.service` + `relay-alert.sh`), and `status.sh` for an
at-a-glance health dashboard. Build with `cargo build --release`, put `.env`,
`config.yaml`, and the session file next to the binary, and point the unit at it.
`deploy/STATUS.md` documents the full boot chain.

Silence means healthy: the relay does not post routine start/stop notices — you
hear from it only when a message is relayed, or when something is actually wrong.

## Non-goals

- Not a full client — no sending, editing, or reacting from Discord back into
  Telegram.
- Not multi-account — one session per running instance.
- Not a hosted service — no dashboard, no cloud, no multi-tenant config.
- Not a bot integration — it uses MTProto as a user account on purpose, to watch
  chats a bot could never join.

## License

MIT. See [LICENSE](LICENSE).
