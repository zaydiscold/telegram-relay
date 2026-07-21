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

## Task 6 corrections (2026-07-20 — verified by compiling telegram.rs against 0.10.0 source)

Written while implementing `src/telegram.rs`. All confirmed against the vendored
crate source in `~/.cargo/registry/.../grammers-{client,session,mtsender}-0.10.0`
and `cargo check`/`clippy`.

1. **`SenderPool` IS re-exported at the crate root.** The earlier note said it is
   not. `grammers_client/src/lib.rs` has:
   `pub use grammers_mtsender::{self as sender, InvocationError, SenderPool};`
   So `use grammers_client::{Client, SenderPool, SignInError};` works directly.

2. **`ClientConfiguration` / `UpdatesConfiguration` live under `grammers_client::client`,
   NOT the crate root.** Import path: `grammers_client::client::{ClientConfiguration, UpdatesConfiguration}`.

3. **`Client` is built with `Client::new(handle)` or `Client::with_configuration(handle, ClientConfiguration)`**
   where `handle: SenderPoolFatHandle` (Clone). `SenderPool::new(Arc<S>, api_id: i32)`
   takes only the api_id — NOT api_hash. `api_hash` is only needed for
   `request_login_code(phone, api_hash)`.

4. **`UpdatesConfiguration` has two fields**: `catch_up: bool` and
   `update_queue_limit: Option<usize>`; it impls `Default` (catch_up=false,
   limit=Some(100)), so `UpdatesConfiguration { catch_up: true, ..Default::default() }`
   is correct. `catch_up: true` is confirmed to drive `MessageBoxes::load(session.updates_state())`.

5. **Catch-up needs NO `state.json`.** `SqliteSession` (feature `sqlite-storage`,
   which is a *default* feature of `grammers-session`, so enabled via our direct
   dep even though `grammers-client` pulls session with `default-features=false`)
   persists auth key, cached peers AND update state to the sqlite file
   automatically (each trait method commits its own transaction). So the library
   handles catch-up; the plan's `state.json` fallback is unnecessary and was not
   implemented. `SqliteSession::open(path).await? -> Result<Self, SqliteSessionError>`.

6. **`connect()` cannot return a bare `Client`.** In 0.10.0 the update stream is a
   separate channel (`SenderPool { runner, handle, updates }`) driven by a
   background `runner.run()` task. `telegram::connect()` therefore returns a
   `Connection { client, updates, handle, pool_task }`. A `stream_updates(client,
   updates)` helper wraps `client.stream_updates(..)` with `catch_up: true`.
   `updates` type: `tokio::sync::mpsc::UnboundedReceiver<grammers_session::updates::UpdatesLike>`
   (re-exportable as `grammers_client::session::updates::UpdatesLike`).

7. **`Incoming::Media.media` is `grammers_client::media::Media`**, not
   `grammers_client::types::Media` (there is no `types` module — the brief's type
   path is stale). Media size via `Media::size(&self) -> Option<usize>`.

8. **Peer / ids.** `Dialog { raw, peer: Peer, last_message }`; `dialog.peer() -> &Peer`.
   `Peer` enum variants are `User(User) | Group(Group) | Channel(Channel)`.
   `Peer::id() -> PeerId`, `PeerId::bot_api_dialog_id_unchecked() -> i64` gives the
   Bot-API-style id (e.g. `-100…` for channels) that matches config numeric ids —
   used everywhere for `ChatId` so router keys line up. `Peer::name() -> Option<&str>`,
   `Peer::username() -> Option<&str>`.

9. **Message accessors** (on `crate::message::Message`, reached via `Deref` from
   `Update::{NewMessage,MessageEdited}(update::Message)`): `id() -> i32`,
   `peer_id() -> PeerId`, `peer() -> Option<&Peer>`, `sender() -> Option<&Peer>`,
   `text() -> &str`, `media() -> Option<Media>`, `grouped_id() -> Option<i64>`,
   `reply_to_message_id() -> Option<i32>`. The quoted *body* of a reply is NOT
   carried inline (only the id) — `classify` sets `reply_quote: None`; enriching it
   needs a follow-up fetch (deferred to the enrichment lane / Task 7b).

10. **`SignInError` impls `Display + std::error::Error`** and is `Send + Sync`, so it
    works with `?`/`anyhow`. `PasswordToken::hint() -> Option<&str>`;
    `check_password(password_token: PasswordToken, password: impl AsRef<[u8]>)`.

11. **Minor clippy gate**: `println!("{:>14} {:8} {}", "id","type","title")` trips
    `clippy::print_literal` under `-D warnings`; inline the last literal into the
    format string. `Incoming` gets `#[allow(clippy::large_enum_variant)]` (the
    `Media` variant is large, mirroring the upstream `Media` enum's own allow).

## Task 7b additions (2026-07-20 — verified against vendored 0.10.0 source)

Written while implementing `src/store.rs`, `src/refresh.rs`, embed rendering,
and the refresh worker. Confirmed against
`~/.cargo/registry/.../grammers-{client,tl-types,session}-0.10.0` and
`cargo clippy -D warnings` / `cargo test`.

1. **`get_messages_by_id` exists and is the right call.**
   `Client::get_messages_by_id<C: Into<PeerRef>>(peer, ids: &[i32]) ->
   Result<Vec<Option<Message>>, InvocationError>` (`src/client/messages.rs`).
   Docs cap it at 100 ids. Result is index-aligned with the input ids; `None`
   = not retrievable (deleted / not in peer). It auto-routes to
   `channels::GetMessages` vs `messages::GetMessages` by peer kind.

2. **Reactions ARE cleanly exposed — no stub needed.** `Message::raw` is a
   `pub` field (`tl::enums::Message`). Per-emoji breakdown:
   `tl::enums::Message::Message(m)` → `m.reactions:
   Option<MessageReactions>` → `MessageReactions::Reactions(r)` →
   `r.results: Vec<ReactionCount>`. Each `ReactionCount::Count(c)` has
   `c.count: i32` and `c.reaction: tl::enums::Reaction`, where
   `Reaction::Emoji(ReactionEmoji { emoticon: String })` gives the unicode
   emoji. Other variants: `Paid`, `CustomEmoji(document_id)` (no unicode —
   mapped to markers `⭐` / `🎨` in `refresh::extract_reactions`), `Empty`
   (skipped). There is also a convenience `Message::reaction_count() ->
   Option<i32>` (sum only). `tl` is re-exported as `grammers_client::tl`.

3. **Comment/discussion count IS exposed:** `Message::reply_count() ->
   Option<i32>` (from `m.replies: MessageReplies::Replies { replies }`). Used
   as the 💬 count. `Message::view_count()` / `forward_count()` /
   `edit_date()` are also available if needed later.

4. **Fetch-by-id needs a `PeerRef` (identity + access hash).** A bare bot-API
   `i64` is not enough for channels. `PeerId::from_bot_api_dialog_id` yields
   only identity, not `auth`. The working path: at route resolution call
   `Peer::to_ref().await -> Result<Option<PeerRef>, _>` on the
   `resolve_username` result and cache `HashMap<ChatId, PeerRef>` (`PeerRef`
   is `Copy`). Numeric-id (`ChatRef::Id`) routes have no resolved `Peer`, so
   they are relayed live but NOT refreshed (documented limitation). `PeerRef`
   lives at `grammers_client::session::types::PeerRef`.

5. **`PeerRef: Into<PeerRef>`** via the std blanket `impl<T> From<T> for T`,
   so it passes straight into `get_messages_by_id`.

## Embed-identity additions (2026-07-21 — verified against vendored 0.10.0 source)

Written while adding webhook avatar, embed color, and exact timestamp. All
confirmed against `~/.cargo/registry/.../grammers-client-0.10.0/src` and
`cargo clippy -D warnings` / `cargo test`.

1. **`Message::date() -> chrono::DateTime<Utc>`** (`message/message.rs:344`)
   returns the ORIGINAL publish time (the raw `message.date` TL field via
   `date_timestamp()`); edit time is separate (`edit_date()`). `chrono` is
   already in the tree transitively via grammers; we name it as a direct dep
   (`default-features = false, features = ["std"]`) only to format that value —
   we never call `Utc::now()`, so the `clock` feature is intentionally not
   requested. Format for Discord's embed `timestamp` with
   `dt.to_rfc3339_opts(SecondsFormat::Secs, true)` → `2026-07-20T18:04:05Z`.

2. **Chat/channel profile photo download.** `Peer::photo(big: bool) -> Result<
   Option<ChatPhoto>, InvocationError>` (`peer/mod.rs:185`) builds an
   `InputPeerPhotoFileLocation` from the peer's cached `photo_id` — it does NOT
   hit the network itself (may query the session for peer info). `ChatPhoto`
   (`media/media.rs:94`) implements `Downloadable` (`media/media.rs:883`), so
   the bytes come down through the SAME `client.iter_download(&chat_photo)` +
   `download.next().await` chunk loop used for message media. `big=false` is the
   small avatar (Discord scales down anyway). Per-kind photo accessors exist too
   (`Channel::photo()`, `Group::photo()`, `User::photo()`), but `Peer::photo()`
   is the uniform entry point. Telegram profile photos are JPEG, so the Discord
   avatar `data:` URI uses `image/jpeg` (Discord sniffs the actual bytes; the
   mime just has to be an accepted image type).

3. **Discord webhook avatar/name are set by PATCHing the webhook resource
   itself** — `PATCH {webhook_url}` with `{"name": "...", "avatar":
   "data:image/jpeg;base64,..."}` — NOT a per-message override and NOT the
   `.../messages/{id}` endpoint (that PATCHes a posted message). This persists
   on the webhook, so every later post shows the channel's name + photo. This is
   a Discord API fact, not a grammers one, but recorded here since it pairs with
   (2). Guard churn with a stored identity hash; a webhook has exactly one
   avatar, so a webhook shared by multiple routes is set once (first route wins).
