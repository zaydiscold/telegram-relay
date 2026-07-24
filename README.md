<p align="center">
  <img src="./assets/banner.svg" alt="telegram-relay" />
</p>

<h1 align="center">telegram-relay</h1>

<p align="center">forwards telegram messages into discord, as a real logged-in user.</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-stable-CE422B?style=flat-square&logo=rust&labelColor=1a1a2e" alt="rust" />
  <img src="https://img.shields.io/github/actions/workflow/status/zaydiscold/telegram-relay/ci.yml?style=flat-square&labelColor=1a1a2e" alt="ci" />
  <img src="https://img.shields.io/badge/license-MIT-9b7dff?style=flat-square&labelColor=1a1a2e" alt="license" />
  <img src="https://img.shields.io/badge/telegram-MTProto-9b7dff?style=flat-square&logo=telegram&labelColor=1a1a2e" alt="mtproto" />
</p>

<p align="center">
  <a href="#what-it-does">what it does</a> · <a href="#features">features</a> · <a href="#security">security</a> · <a href="#install">install</a> · <a href="#quickstart">quickstart</a> · <a href="#config">config</a>
</p>

<br>

<blockquote>
<strong>heads up — this is a userbot.</strong> it signs in as your real telegram
account over MTProto (your <code>api_id</code>/<code>api_hash</code> + phone login), not the bot api.
that's the whole point (a bot can't see what your account sees), but automating a
personal account is against telegram's ToS and <em>can</em> get it limited or banned. run it
on an account you're willing to risk. if you only forward channels or groups you
admin, the official <a href="https://core.telegram.org/bots">bot api</a> is the ToS-clean route — use that instead.
</blockquote>

<br>
<br>

<p align="center">
  <img src="./assets/stars1.svg" alt="·" />
</p>

<br>
<br>

## what it does

forwards telegram messages into discord: words, media, photos, groups, channels, DMs.
one telegram source can route to many discord servers and channels, and many sources can
funnel into one. it emulates a logged-in telegram device (MTProto, a real user account),
so it sees everything your own client sees, which a bot can't.

fast, too. roughly 150 to 400ms end to end, depending on your connection. telegram pushes
messages down a live connection, so there's nothing to poll. `telegram-relay stats` reports
the real p50/p95/max latency from your own traffic, measured off telegram's publish time
and discord's message snowflake (never the relay host's clock).

<br>
<br>

<p align="center">
  <img src="./assets/stars2.svg" alt="·" />
</p>

<br>
<br>

## features

| feature | what it does |
|---|---|
| live-updating embeds | each post becomes a discord embed with the source channel's name and photo as the webhook identity, a colored stripe, the original telegram timestamp, a link back, and a reaction/comment stats line. |
| state-aware colors | the stripe shows state: regular posts are purple, an edited post turns orange, a deleted one turns red and says so. it transitions in place, so you read what happened without opening telegram. |
| reactions, edits, deletes | a background worker re-checks tracked posts and patches the embed in place. reactions settle over the first hour; edits and deletes stay tracked for two days. |
| real media, inline | photos and videos relay as attachments inside the embed. multi-image albums coalesce into one gallery. link-only posts relay as text so discord renders its own preview. |
| fan-in / fan-out | one source can feed several channels; several sources can feed one. per-webhook dedup means adding a webhook to a route never re-spams the others. |
| per-route identity | each route gets its own webhook avatar (the channel photo) and stripe color, so sources funneled into one channel stay distinguishable. |
| catch-up + dedup | reconnects replay missed messages (telegram's native catch-up); a durable store stops double-posting, even across restarts. |
| hot-reload | routes and filters re-read from `config.yaml` on a timer. add a channel without restarting. |
| backfill | `backfill <route> --count N` relays the last N posts of a channel on demand. |

<br>
<br>

<p align="center">
  <img src="./assets/stars3.svg" alt="·" />
</p>

<br>
<br>

## security

logs in as your account, so it's careful with that reach:

- only chats you route are ever touched. anything else is dropped before a byte is downloaded.
- media streams through memory, never hits disk, is never executed. `mode: placeholder` downloads nothing.
- egress is one-way, to *your* webhooks only — nothing phones home to anyone, no cloud, fully self-hosted.
- treat each discord webhook url as a secret: it's an unauthenticated "post to this channel" capability. keep it in `.env`.
- the session file is your account: `chmod 600`, never committed, revoke from telegram settings.
- webhook tokens never appear in any log, error, or notice (enforced by the type system).
- hardened systemd unit (`ProtectSystem=strict`, `PrivateTmp`, `NoNewPrivileges`), no phone-home.

<br>
<br>

<p align="center">
  <img src="./assets/stars4.svg" alt="·" />
</p>

<br>
<br>

## install

needs a rust toolchain ([rustup.rs](https://rustup.rs)).

```bash
git clone https://github.com/zaydiscold/telegram-relay
cd telegram-relay
cargo build --release          # binary at target/release/telegram-relay
```

or put it on your PATH so `telegram-relay` just works:

```bash
cargo install --path .
```

## quickstart

1. get api credentials at [my.telegram.org](https://my.telegram.org) (api development tools, create an app) for an `api_id` and `api_hash`.
2. create `.env` next to the binary:
   ```
   TELEGRAM_API_ID=12345
   TELEGRAM_API_HASH=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
   DISCORD_WEBHOOK_MAIN=https://discord.com/api/webhooks/…
   DISCORD_WEBHOOK_OPS=https://discord.com/api/webhooks/…   # optional
   ```
3. copy `config.example.yaml` to `config.yaml` and set your routes.
4. log in once (phone, code, optional 2FA): `telegram-relay login`
5. inspect and validate (none of these send anything):
   ```
   telegram-relay chats     # every dialog and its id
   telegram-relay routes    # ASCII wiring diagram (fan-in / fan-out)
   telegram-relay check     # validate webhooks and routes; exit 0/1
   ```
6. run it: `telegram-relay run`

| command | purpose |
|---|---|
| `run` | run the relay (what the service invokes). |
| `login` | one-time interactive login; writes the session file. |
| `chats` | list every dialog with its numeric id. |
| `routes` | ASCII diagram of the routing (source to webhooks); no session needed. |
| `check` | validate config and webhooks (and routes); exit 0/1. sends nothing. |
| `stats` | tracked-post counts and measured relay latency (p50/p95/max). |
| `backfill <route> [--count N]` | relay the last N posts of a route on demand. |

## deploy

`deploy/` has a systemd unit, a failure-alert unit that posts to your ops webhook when the
relay goes down (`telegram-relay-alert@.service` + `relay-alert.sh`), and `status.sh` for a
health dashboard. build with `cargo build --release`, put `.env`, `config.yaml`, and the
session file next to the binary. `deploy/STATUS.md` documents the boot chain. silence means
healthy: it posts only when a message is relayed, or when something is wrong.

not a full client (no sending from discord back to telegram), not multi-account (one session
per instance), not a hosted service (no dashboard, no cloud), not a bot integration.

<br>
<br>

<p align="center">
  <img src="./assets/stars5.svg" alt="·" />
</p>

<br>
<br>

## config

`config.yaml`:

| key | meaning |
|---|---|
| `routes[].name` | label for the route (used in logs). |
| `routes[].label` | optional friendly source name for the `routes` diagram (e.g. `News`); falls back to the `@handle`. |
| `routes[].from` | source chat: `"@username"` or a numeric chat id. |
| `routes[].to` | webhook names (from `webhooks:`) to fan out to. |
| `routes[].color` | optional `"#RRGGBB"` stripe for regular posts; defaults `#9b7dff`. edited/deleted colors override it. |
| `routes[].mode` | optional `reupload` / `placeholder`; overrides the global `media.mode`. |
| `routes[].filter` | optional `any_keywords` / `exclude_hashtags`. |
| `webhooks.<name>.env` | env var holding that webhook's url. |
| `webhooks.<name>.label` | optional friendly destination name for the diagram (e.g. `"lock-in #news"`); falls back to the webhook key. |
| `ops_webhook.env` | optional webhook for error/failure notices only. |
| `contract_passthrough` | `true` also posts any Solana/ETH contract address as plain content outside the embed (for scanning bots). default `false`. |
| `media.mode` | `reupload` (download and inline) or `placeholder` (link only). |
| `media.max_bytes` | above this, fall back to a link instead of re-uploading. |
| `refresh.interval_mins` | edit/delete re-check cadence (default 30). |
| `refresh.horizon_hours` | stop tracking posts older than this (default 48). |
| `refresh.reaction_horizon_mins` | stop refreshing reactions after this (default 60). |
| `refresh.reaction_early_check_secs` | early reaction re-check, in seconds (default 60). |
| `store.path` | sqlite file tracking relayed posts (default `relay.db`). |

all three routing shapes work with no code. they're just how you write `routes`:

```yaml
# one source to one channel
- { name: solo, from: "@alpha", to: [chan_a] }
# many sources to one channel (fan-in)
- { name: a, from: "@alpha", to: [firehose] }
- { name: b, from: "@beta",  to: [firehose] }
# one source to many channels (fan-out)
- { name: split, from: "@alpha", to: [chan_a, chan_b] }
```

`telegram-relay routes` draws the wiring both ways, so fan-in and fan-out are unambiguous:

```
telegram-relay — routing (2 source(s) → 3 destination(s))

by source — where each telegram channel goes:
  Alpha (@alpha_news)  ──▶  server · #crypto · hub
  Beta (@beta_wire)    ──▶  server · #news · hub

by destination — what each discord channel receives:
  hub              ◀──  Alpha · Beta   (all sources)
  server · #crypto  ◀──  Alpha
  server · #news    ◀──  Beta
```

the friendly names come from the optional `label:` keys; without them you get bare `@handles`.

<br>
<br>

<p align="center">
  <img src="./assets/wisps.svg" alt="" />
</p>

<br>
<br>

<p align="left"><strong>zayd / cold</strong></p>

<p align="center">
  <a href="https://zayd.wtf">zayd.wtf</a> · <a href="https://x.com/coldcooks">twitter</a> · <a href="https://github.com/zaydiscold">github</a>
  <br>
  <em>icarus only fell because he flew</em>
</p>

<p align="center"><sub>MIT, see <a href="LICENSE">LICENSE</a></sub></p>
