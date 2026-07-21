# telegram-relay — Operations & Status Reference

_Where it runs, what it watches, how it boots, and how to check on it._

## Where it runs

| | |
|---|---|
| **Host** | `mothership` (always-on Windows desktop) → **WSL Ubuntu** |
| **Service** | systemd unit `telegram-relay.service` (`Restart=always`, `RestartSec=2`) |
| **Working dir** | `/home/zaydk/telegram-relay/` |
| **Binary** | `target/release/telegram-relay` (native x86_64 Linux build) |
| **Identity** | logged in as Zayd's Telegram account (MTProto userbot) via `relay.session` |

Frostbyte (the laptop) is **dev only** — never run `... run` there while mothership
is live; two machines sharing one session split the update stream. One session,
one host.

## Boot chain (fully automatic, no login required)

```
Windows powers on
  └─ Task Scheduler task "WSL-keepalive-telegram-relay" (At startup, S4U)
       └─ launches WSL Ubuntu, keeps it resident
            └─ WSL systemd (systemd=true in /etc/wsl.conf)
                 └─ telegram-relay.service  (enabled, After=network-online)
                      └─ ./target/release/telegram-relay run
```

If the process dies, systemd restarts it in ~2s. If WSL drops, the scheduled
task brings it back. If the box reboots, the whole chain re-arms itself.

## What it watches (routes)

Source of truth: `config.yaml` (gitignored — contains the live routing table).

| Route | Telegram source | → Discord |
|---|---|---|
| `robs-thoughts` | **Rob's Thoughts** channel (`-1002840386812`, `@robthinks`) | `#rc` webhook |

To add a route: edit `config.yaml` and save. **Hot-reload picks it up within 5s —
no restart.** (Adding/removing credentials or the session still needs a restart.)

## Settings (the "intermittent" knobs)

| Setting | Value | What it does |
|---|---|---|
| `media.mode` | `reupload` | download Telegram media, re-upload to Discord (vs `placeholder` = link only) |
| `media.max_bytes` | `10 MB` | above this, post a `t.me` deep-link instead of re-uploading |
| `refresh.interval_mins` | `30` | how often tracked posts are re-checked for reaction/comment/edit/delete changes |
| `refresh.horizon_hours` | `48` | stop updating (and prune) posts older than this |
| `store.path` | `relay.db` | SQLite map of relayed posts (enables the live embed updates) |
| heartbeat | `300s` | liveness log line |
| media tick | `250ms` | album-coalescing poll |
| reload tick | `5s` | config.yaml mtime check |
| dedup LRU | `8192` | recent (chat,msg) ids kept to prevent reconnect double-posts |
| 429 budget | 20 attempts / 60s | Discord rate-limit backoff ceiling before drop-with-log |

## How to check on it

**Full status dashboard** (service state · routes · settings · tracked-post count · recent logs):
```bash
ssh mothership-wsl 'bash ~/telegram-relay/deploy/status.sh'
```

**Live logs:**
```bash
ssh mothership-wsl 'journalctl -u telegram-relay -f -o short-iso'
```

**In Discord:** the relay posts lifecycle notices (started / shutting down / route
resolution failures / delivery drops) and can carry heartbeats to the ops webhook —
`#rc` is your at-a-glance monitor without opening a terminal.

## Common operations

```bash
# restart / stop / start
ssh mothership-wsl 'sudo systemctl restart telegram-relay'

# discover new channel ids to add as routes (stop the service first — single session!)
ssh mothership-wsl 'sudo systemctl stop telegram-relay && cd ~/telegram-relay && ./target/release/telegram-relay chats; sudo systemctl start telegram-relay'

# validate config + webhooks without sending anything
ssh mothership-wsl 'cd ~/telegram-relay && ./target/release/telegram-relay check'

# relay the last N posts of a route on demand (acceptance / catch-up)
ssh mothership-wsl 'sudo systemctl stop telegram-relay && cd ~/telegram-relay && ./target/release/telegram-relay backfill robs-thoughts --count 3; sudo systemctl start telegram-relay'
```

## Deploying a new build (from frostbyte)

```bash
cd ~/Desktop/telegram-relay
rsync -az --exclude target --exclude .env --exclude '*.session' \
      --exclude relay.db --exclude config.yaml --exclude .superpowers \
      ./ mothership-wsl:~/telegram-relay/
ssh mothership-wsl 'cd ~/telegram-relay && ~/.cargo/bin/cargo build --release && sudo systemctl restart telegram-relay'
```

## Security notes

- `relay.session` **is** account access — mode `600`, never committed, never leaves
  the two machines. Revoke anytime: Telegram → Settings → Devices.
- `.env` (api_id/hash + webhook URLs) — mode `600`, gitignored.
- Egress contract: messages go **only** to the webhooks in `config.yaml`. Nothing else.

## Files

| File | Committed? | Purpose |
|---|---|---|
| `config.yaml` | no (gitignored) | live routing table + settings |
| `.env` | no (gitignored) | api_id/hash, webhook URLs |
| `relay.session` | no (gitignored) | Telegram auth (chmod 600) |
| `relay.db` | no (gitignored) | relayed-post tracking store |
| `config.example.yaml` | yes | copy → `config.yaml` to configure |
| `deploy/telegram-relay.service` | yes | systemd unit |
| `deploy/status.sh` | yes | status dashboard script |
| `deploy/STATUS.md` | yes | this document |
