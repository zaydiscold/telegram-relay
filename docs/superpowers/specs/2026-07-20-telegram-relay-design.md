# telegram-relay — Design Spec

**Date:** 2026-07-20
**Repo:** `zaydiscold/telegram-relay` (public, MIT)
**One-liner:** A latency-obsessed Rust relay that mirrors chosen Telegram chats (channels, groups, DMs) to Discord webhooks over MTProto.

## Purpose & context

Mirror Telegram sources into Discord for people who don't use Telegram. First
concrete route: the public channel `@robthinks` ("Rob's Thoughts") into a
friend's Discord server via webhook. The relay logs in as the operator's own
Telegram account (MTProto userbot — the same protocol Telegram Desktop
speaks), which is the only API tier that can see DMs and channels without
adding bots as admins.

Survey of prior art (2026-07-20): the niche is bot-based (TediCross — can't
read DMs), archived (UrekD/Telegram-To-Discord, telebagger), or bloated
(hyp3rd/telegram-discord-bridge). No minimal, fast, actively-maintained
userbot relay exists. All competitors are Python/TS; none in Rust.

## Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (stable) | Single static binary deploy, ~5MB RSS, instant restart; public-repo credibility in a niche full of scripts |
| MTProto | `grammers-client` 0.10.x (pinned) | Active (July 2026, layer 222), by Telethon's author. 0.x — pin exact version |
| Async runtime | `tokio` | Standard |
| HTTP out | `reqwest` | Warm connection pool w/ keepalive to discord.com |
| Config | `serde` + `serde_yaml` + `.env` | Parse-don't-validate into owned types at boot |
| Logging | `tracing` + `tracing-subscriber` | Structured, journald-friendly |

Known grammers gaps we own: flood-wait/backoff handling, missed-update
catch-up, Telegram entity → Discord markdown conversion. Fixes that belong in
the library get upstreamed as PRs to grammers.

## Architecture

Single process, one tokio runtime.

```
Telegram DC ══push══> grammers client ──> router (chat id → routes)
                                            │
                              ┌─────────────┴──────────────┐
                        text lane (hot)              media lane (background)
                        render → POST now            download → multipart POST
                              │                             │
                              └──> warm reqwest pool ───────┘
                                   (per-webhook rate-limit guard)
```

**Hot-path contract:** the update handler does route-match + task-spawn only.
Text POSTs fire immediately on the pooled connection. Media never blocks
text. Expected end-to-end: ~150–400 ms (network-bound; both lanes async).

### Modules

- `config` — YAML + env → typed `Config` at boot or exit with the exact bad
  field named. Raw strings never cross this boundary. Hot-reload of routes on
  file change (creds/session changes require restart).
- `router` — `HashMap<ChatId, Vec<Route>>` lookup + per-route keyword/hashtag
  filters. Pure; unit-tested.
- `render` — Telegram entities → Discord markdown, sender prefix, reply
  context as `> ` quote. Pure; unit-tested.
- `deliver` — webhook POST with per-webhook token bucket, `Retry-After`
  honor, exponential backoff ×3 then drop-with-log. Returns
  `Delivered | RateLimited | Dropped` — errors are values.
- `media` — background download → multipart re-upload (≤ configured cap,
  default 10 MB). Oversized → text notice with `t.me/<chan>/<msg_id>`
  deep-link.
- `dedup` — LRU of recent (chat_id, msg_id) + content hashes; guards against
  reconnect replays double-posting.
- `catchup` — on reconnect, fetch messages since last-seen per watched chat;
  replay through the same pipeline (dedup makes this idempotent).
- `main` — wiring, signal handling (SIGTERM/SIGINT graceful), heartbeat log
  every 5 min.

## Config

```yaml
# config.yaml — safe to commit as config.example.yaml; real one gitignored
routes:
  - name: robs-thoughts
    from: "@robthinks"            # @username, t.me link, or numeric id
    to: [discord_robs]            # fan-out = list more names
    # filter: { any_keywords: [...], exclude_hashtags: [...] }  # optional

webhooks:
  discord_robs: { env: DISCORD_WEBHOOK_ROBS }

media: { mode: reupload, max_bytes: 10000000 }
```

Secrets (`TELEGRAM_API_ID`, `TELEGRAM_API_HASH`, `DISCORD_WEBHOOK_*`) live in
`.env`, chmod 600. `.env`, `*.session` gitignored from the first commit —
webhook URLs are credentials.

## Auth & first run

1. Operator gets `api_id`/`api_hash` from my.telegram.org → API development
   tools (free, official; registers the relay as a custom client).
2. First run: interactive phone + login-code prompt (code arrives in the
   Telegram app). Produces a session file, chmod 600.
3. Session appears under Telegram Settings → Devices; revocable anytime.
4. **Dialog enumerator:** run with `--list-chats` (or zero configured routes)
   → prints every dialog with name, type, and numeric id. This is how
   further Rob channels get discovered and added to config.

## Message rendering

- Webhook `username`/`avatar_url` overrides per route so each source is
  visually distinct in Discord.
- Text as plain `content` (no embed assembly on the hot path).
- Edits relayed as new message prefixed `(edited)`. No edit-in-place in v1
  (needs a message-id map; add later if wanted).
- Media follows as a second POST on the same route.

## Error handling & resilience

- Telegram disconnect → auto-reconnect + `catchup` replay; dedup guarantees
  idempotence.
- Discord 429 → honor `Retry-After`; 5xx → backoff ×3 → drop-with-log
  (a relay never builds an unbounded backlog).
- Poison messages (odd media types) → per-message catch, log, continue.
- systemd `Restart=always`, `RestartSec=2`; binary restart is ~ms.
- Heartbeat log line every 5 min for at-a-glance liveness in journalctl.

## Code style

Lightly Jane Street: illegal states unrepresentable via enums/newtypes
(`ChatId`, `WebhookUrl`), parse-don't-validate at the config boundary,
errors-as-values in module APIs, small single-purpose modules. Enforced by
`cargo clippy -- -D warnings` + `rustfmt` + tests in CI (GitHub Actions).
Not enforced: no dogmatic combinator towers; readability wins.

## Testing

- Unit: `router`, `render`, `dedup`, config parsing (pure, no network).
- Integration: `deliver` against a local mock HTTP server (rate-limit and
  retry behavior).
- Acceptance (manual): Saved Messages → test webhook round-trip < 1 s; kill
  process → systemd revives; reboot mothership → relay returns unattended.

## Deployment

- **Dev:** frostbyte (macOS), `cargo run`. Interactive login happens here.
- **Prod:** mothership WSL Ubuntu (`ssh mothership-wsl`). Cross-compile to
  `x86_64-unknown-linux-gnu` (or `cargo build` in WSL); scp binary + config +
  session file. systemd unit `telegram-relay.service` (`Restart=always`).
  Windows Task Scheduler boot entry keeps WSL resident without login.
- New-IP session use will trigger a Telegram "new login" notification once —
  expected, it's the session moving to mothership.

## Non-goals (v1)

- Discord → Telegram direction. Edit-in-place. Multi-account. Web UI.
  Translation/AI anything. Bot-API mode.

## Consent & scope

Passive mirror of chats visible to the operator's own account, into Discord
servers the operator/channel-owner controls, with the channel owner's
blessing (route #1 is the owner's own request). No mass-joining, no
scraping of third-party channels at volume, no evasion — the patterns that
trip Telegram anti-abuse are all out of scope by design.
