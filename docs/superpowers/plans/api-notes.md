# grammers 0.10.0 API notes (verified 2026-07-20)

Source of truth used: the actual crate source on the canonical repo
(https://codeberg.org/Lonami/grammers, `master` branch, which is pinned at
version `0.10.0` in its own `Cargo.toml` — confirmed via
`grammers-client/Cargo.toml` on that branch) plus docs.rs pages for
`grammers-client/0.10.0`. GitHub (`github.com/Lonami/grammers`) is a stale
mirror frozen at `v0.8.0` — do not trust it for 0.10 API shape.

Verified crates.io versions (both pin cleanly with `=0.10.0`):
- `grammers-client` → max_version `0.10.0`
- `grammers-session` → max_version `0.10.0`

## Headline finding: the API in the task-1 brief is stale

The brief's plan (`Client::connect(config)`, `client.next_update()`) matches an
older (~0.7-era) grammers API. **0.10.0 has a materially different shape**:
there is no `Client::connect` and no bare `client.next_update()` loop anymore.
Later tasks (telegram.rs, router.rs, etc.) need to be written against the
shape below, not the brief's pseudocode.

## Construction / connection

There is no single `Client::connect(...)`. Instead:

1. Open a session (persistent, SQLite-backed):
   ```rust
   use grammers_session::storages::SqliteSession;
   let session = Arc::new(SqliteSession::open(SESSION_FILE).await?); // *.session file — already gitignored
   ```
   `SqliteSession: Session` (there's a `Session` trait bound: `S: Session,
   <S as Session>::Error: Error + Send + Sync + 'static`).

2. Build a `SenderPool` (from `grammers_mtsender::SenderPool`, re-exported at
   `grammers_client::sender::SenderPool`) — this is the actual connection/IO
   driver, and it destructures into three parts:
   ```rust
   use grammers_mtsender::SenderPool;
   let SenderPool { runner, handle, updates } = SenderPool::new(Arc::clone(&session), api_id);
   // or: SenderPool::with_configuration(session, api_id, connection_params) for custom ConnectionParams
   ```

3. Build the `Client` from the pool's `handle`:
   ```rust
   use grammers_client::Client;
   let client = Client::new(handle.clone()); // handle: SenderPoolFatHandle (Clone)
   // Client::with_configuration(handle, ClientConfiguration) for custom config
   ```

4. Spawn the pool's IO runner as a background task — nothing happens on the
   wire until this is running:
   ```rust
   let pool_task = tokio::spawn(runner.run());
   ```

5. Graceful shutdown: `handle.quit()` then `pool_task.await`. Drop-based
   disconnect also works but won't flush update state.

## Sign-in flow (user account — this project is a userbot, not a bot)

```rust
use grammers_client::SignInError;

if !client.is_authorized().await? {
    let token = client.request_login_code(&phone, api_hash).await?; // -> LoginToken
    let signed_in = client.sign_in(&token, &code).await; // -> Result<User, SignInError>
    match signed_in {
        Ok(_user) => {}
        Err(SignInError::PasswordRequired(password_token)) => {
            // 2FA: password_token.hint() -> Option<&str>
            client.check_password(password_token, password.trim()).await?; // -> Result<User, SignInError>
        }
        Err(e) => return Err(e.into()),
    }
}
```

Bot accounts (not our use case, but noted): `client.bot_sign_in(&token, api_hash).await? -> Result<User, InvocationError>`.

`SignInError` and `Client` are both re-exported at the crate root
(`grammers_client::{Client, SignInError}`); `SenderPool` is NOT re-exported at
the crate root in 0.10.0 — import it from `grammers_mtsender::SenderPool`
directly (or via `grammers_client::sender::SenderPool`, since
`grammers_client::sender` is a re-export of the whole `grammers_mtsender`
crate).

Other auth-adjacent methods on `Client`: `is_authorized(&self) -> Result<bool, InvocationError>`,
`sign_out(&self) -> Result<LoggedOut, InvocationError>`, `disconnect(&self)`.

## Update loop — `stream_updates`, not `next_update()`

There is no bare `client.next_update().await` loop as the primary API anymore
(a comment in the official `echo.rs` example even calls out `next_update` as
the old/simpler alternative people might expect). The current pattern:

```rust
use grammers_client::client::UpdatesConfiguration;

let mut updates = client
    .stream_updates(
        updates, // the `updates` receiver returned by SenderPool::new() destructuring, step 2 above
        UpdatesConfiguration { catch_up: true, ..Default::default() },
    )
    .await?; // -> Result<UpdateStream, Box<dyn Error + Send + Sync>>

loop {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => break,
        update = updates.next() => {
            let update: Update = update?; // UpdateStream::next() yields Result<Update, _>
            // dispatch update
        }
    }
}

updates.sync_update_state().await?; // call before shutting down / closing the session
```

**`catch_up: true` on `UpdatesConfiguration` is confirmed to exist** — this is
the flag the brief needs for "catch up on missed messages after downtime."
`UpdatesConfiguration` implements `Default`, so `..Default::default()` works
for the other fields.

## `Update` enum (`grammers_client::update::Update`)

Non-exhaustive enum, module path `grammers_client::update`:

```rust
pub enum Update {
    NewMessage(Message),        // new text/media message
    MessageEdited(Message),     // message updated
    MessageDeleted(MessageDeletion), // message deleted (may lack full context — Telegram doesn't always tell you which chat)
    CallbackQuery(CallbackQuery),    // bot inline button press
    InlineQuery(InlineQuery),        // bot inline query
    InlineSend(InlineSend),          // bot inline result chosen
    Raw(Raw),                        // unwrapped raw TL update
    // #[non_exhaustive] — match must have a catch-all arm
}
```

For this relay, `Update::NewMessage(Message)` is the primary case (and
possibly `MessageEdited` later if we want edit-relay). `Message` (the type
inside the variant) lives at `grammers_client::update::Message` — this is
distinct from `grammers_client::message::Message` used elsewhere for sent/received
message representations; check which one the update variant actually carries
when router.rs is built (both modules define a `Message` struct — verify with
`cargo doc --open` locally at implementation time to avoid ambiguity).

Relevant `Message` accessors seen in the official `echo.rs` example:
- `message.outgoing() -> bool`
- `message.peer() -> Option<...>` (has `.to_ref().await` for something
  send_message-compatible, and `.name()`)
- `message.peer_id() -> ...`
- `message.text() -> &str`

## `types::Media` → actually `grammers_client::media::Media`

There is no top-level `types` module in 0.10.0; media types live under
`grammers_client::media`. The `Media` enum (from `media.rs` source, confirmed
verbatim):

```rust
pub enum Media {
    Photo(Photo),       // compressed JPEG photo
    Document(Document), // file, may or may not render directly
    Sticker(Sticker),   // document with sticker attributes
    Contact(Contact),
    Poll(Poll),
    Geo(Geo),
    Dice(Dice),          // dice-like built-in sticker media
    Venue(Venue),
    GeoLive(GeoLive),    // self-updating location
    WebPage(WebPage),    // instant-view web page
}
```

Also in `grammers_client::media`: `ChatPhoto`, `InputMedia`, `Uploaded`,
`Attribute` (document attributes), `PhotoSize` (thumbnail variant), and a
`Downloadable` trait implemented by anything fetchable via the client's
download API. Exact `Client` download method names (e.g. something like
`download_media`) were not yet confirmed — re-check via `cargo doc --open`
when media.rs (task covering media re-upload) is implemented, since docs.rs
page rendering for the `Client` struct's full method list was incomplete via
automated fetch and only the subset shown above (auth + stream_updates +
invoke) was independently verified against source examples.

## Session storage

`grammers-session` 0.10.0 exposes `storages::SqliteSession` (async, backed by
a real SQLite file) as the go-to persistent session backend used in every
official 0.10 example. This produces a `*.session`-pattern file on disk,
which matches this repo's existing `.gitignore` entry (`*.session`) — no
`.gitignore` changes needed for this.

## Cargo.toml delta from the brief

None needed — brief's `grammers-client = "=0.10.0"` / `grammers-session =
"=0.10.0"` pins are exactly the current max versions on crates.io. No newer
0.10.x patch exists to prefer instead.

One thing to watch for later tasks: `grammers-client` 0.10.0 itself requires
Rust edition 2024 and `tokio 1.52.3`+ with `rt`/`macros` (and optionally
`fs` — it's a default feature of grammers-client itself, unrelated to our own
`fs` request). Our crate stays on edition 2021 per the brief; that's fine —
edition is per-crate, not transitively forced by a dependency.

## Open questions for later tasks (not blocking task 1)

- Full signature list for `Client`'s message-sending/downloading methods
  (`send_message`, `download_media`-equivalent) — confirm via `cargo doc
  --open` once telegram.rs/media.rs are actually being written; docs.rs's
  rendered method table for `Client` could not be fully enumerated through
  automated fetching (the page is large and the fetch tool truncates/summarizes).
- Whether `Message` inside `Update::NewMessage` is
  `grammers_client::update::Message` or re-exports/aliases
  `grammers_client::message::Message` — both module names appeared in the
  crate's module listing; disambiguate with `cargo doc --open` or `cargo expand`
  when router.rs is implemented.
