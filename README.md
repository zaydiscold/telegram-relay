# telegram-relay

A fast Telegram → Discord webhook relay. It logs in as a real Telegram user
(MTProto userbot, via [grammers](https://github.com/Lonami/grammers)) and
mirrors messages from watched chats into Discord as live-updating embeds.
Rust, single binary, local-first.

## Features

- **Express text lane** — plain text posts are rendered and delivered with
  minimal latency.
- **Album coalescing** — multi-photo/video Telegram posts are buffered on a
  short quiet window and re-posted as a single Discord message.
- **Live-updating embeds** — each relayed post gets a footer, a deep link back
  to Telegram, and a stats line (reactions, comments). A background refresh
  worker re-checks tracked posts on a cadence and PATCHes the embed in place
  when reactions change, the comment count changes, the source is edited, or
  the source is deleted.
- **Catch-up + dedup** — reconnects replay missed updates (Telegram's native
  catch-up), and an LRU dedup guards against re-processing the same message.
- **Hot-reload** — `config.yaml` is re-read on a timer; route/webhook changes
  apply without restarting the process.
- **Ops notices** — an optional webhook receives startup/error notices
  separate from the relayed content channels.

## Quickstart

1. Get API credentials at <https://my.telegram.org> (API Development Tools →
   create an app) — you'll get an `api_id` and `api_hash`.
2. Copy `.env.example` to `.env` (create it if it doesn't exist) and set:
   ```
   TELEGRAM_API_ID=12345
   TELEGRAM_API_HASH=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
   DISCORD_WEBHOOK_ROBS=https://discord.com/api/webhooks/...
   DISCORD_WEBHOOK_OPS=https://discord.com/api/webhooks/...
   ```
3. Copy `config.example.yaml` to `config.yaml` and edit routes/webhooks to
   taste.
4. First-time interactive login (phone number → code → optional 2FA):
   ```
   cargo run -- login
   ```
5. Sanity-check what the account can see, and that config resolves cleanly:
   ```
   cargo run -- chats     # list all dialogs with their ids
   cargo run -- check     # validate config.yaml + route resolution
   ```
6. Run the relay:
   ```
   cargo run -- run
   ```

## Config reference (`config.yaml`)

| Key | Meaning |
|---|---|
| `routes[].name` | Label for the route (used in logs). |
| `routes[].from` | Source chat: `"@username"` or a numeric chat id. |
| `routes[].to` | List of webhook names (from `webhooks:`) to fan out to. |
| `routes[].filter` | Optional `any_keywords` / `exclude_hashtags` allow/deny lists. |
| `webhooks.<name>.env` | Env var holding that webhook's URL. |
| `ops_webhook.env` | Optional env var for a startup/error notices webhook. |
| `media.mode` | `reupload` (download + re-post) or `placeholder` (link only). |
| `media.max_bytes` | Skip re-upload above this size; falls back per `mode`. |
| `refresh.interval_mins` | How often tracked posts are re-checked (default 30). |
| `refresh.horizon_hours` | Stop refreshing/prune posts older than this (default 48). |
| `store.path` | SQLite file tracking relayed posts (default `relay.db`). |

## Deploy

A systemd unit + install script are coming in `deploy/`. Until then, `cargo
build --release` and run the resulting binary with your `.env` and
`config.yaml` alongside it.

## Egress

This relay does not phone home. Your messages go to the Discord webhooks you
configure and nowhere else — no third-party analytics, no external logging
service. Everything runs on your own machine against your own Telegram
account and your own Discord webhooks.

## Session security

The Telegram session file (and the local SQLite store) are written with
owner-only permissions (`chmod 600`) as soon as they're created, so other
local users on the same machine can't read your auth key or relayed message
history.

If you ever need to revoke this relay's access without changing your
password, go to Telegram → Settings → Devices → **Active Sessions** and
terminate the session for this app. That immediately invalidates the local
session file; you'll need to `cargo run -- login` again.

## Non-goals

- Not a full Telegram client — no sending, editing, or reacting from the
  Discord side back into Telegram.
- Not multi-account. One userbot session per running instance.
- Not a hosted service. There's no dashboard, no multi-tenant config, no
  cloud component — it's a CLI daemon you run yourself.
- Not a bot-API integration. This uses MTProto as a user account specifically
  so it can watch channels/chats a bot could never join.
