#![allow(dead_code)]

use std::collections::BTreeMap;

use rand::Rng;
use serde_json::{json, Value};

use crate::config::Filter;

const DISCORD_LIMIT: usize = 2000;
/// Discord embed description hard limit.
pub const EMBED_DESC_LIMIT: usize = 4096;
/// Discord allows at most 10 embeds per message.
pub const MAX_EMBEDS: usize = 10;

/// Default embed stripe color: a vivid Telegram-ish neon blue (`#29B6F6`).
///
/// Used for every route that does not set `color: "#RRGGBB"` in config.yaml.
/// Deliberately not the Discord-default gray — the left bar is the strongest
/// at-a-glance signal that a post came from the Telegram relay.
pub const DEFAULT_EMBED_COLOR: u32 = 0x29B6F6;

/// Format a Telegram publish time for Discord's embed `timestamp` field.
///
/// Discord wants ISO8601/RFC3339; second precision with a `Z` suffix is what it
/// renders (it shows a localized date/time next to the footer). This is the
/// message's ORIGINAL publish time, never our relay time.
pub fn embed_timestamp(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Image extensions Discord can render *inside* an embed via `attachment://`.
/// Video and documents can't be embedded — Discord attaches them below.
pub fn is_image_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Reference attached image files from INSIDE the embed(s), so Discord renders
/// them within the embed frame (with the stripe, stats, and footer) instead of
/// as bare attachments dangling below it.
///
/// - **One image:** set `image.url = attachment://<file>` on the primary embed,
///   so it sits under the caption in the same framed embed.
/// - **An album (multiple images):** Discord merges embeds that share the same
///   top-level `url` into a single gallery, so each image gets its own embed
///   with a shared `gallery_url` and an `attachment://` reference. Capped at
///   [`MAX_EMBEDS`].
///
/// `image_filenames` MUST byte-match the `file_name` of the attached multipart
/// part, or Discord silently shows nothing. Non-image files are not referenced
/// here (they attach below as a player/download — all Discord can do).
pub fn attach_image_attachments(
    embeds: &mut Value,
    image_filenames: &[&str],
    gallery_url: Option<&str>,
) {
    if image_filenames.is_empty() {
        return;
    }
    let Some(arr) = embeds.as_array_mut() else {
        return;
    };
    // First image goes on the existing primary (last description) embed so it
    // renders under the text.
    if let Some(primary) = arr.last_mut() {
        primary["image"] = json!({ "url": format!("attachment://{}", image_filenames[0]) });
    }
    if image_filenames.len() == 1 {
        return;
    }
    // Album: group into one gallery by giving every embed the same `url`.
    // Discord needs a valid http(s) url to group on; fall back to a stable one.
    let url = gallery_url.unwrap_or("https://t.me");
    if let Some(primary) = arr.last_mut() {
        primary["url"] = json!(url);
    }
    for name in image_filenames.iter().skip(1) {
        if arr.len() >= MAX_EMBEDS {
            break;
        }
        arr.push(json!({
            "url": url,
            "image": { "url": format!("attachment://{name}") },
        }));
    }
}

/// "Let's get this bag" footer variations. One is chosen at random per post.
///
/// Rendered uniformly as `by zayd — {variant}`. Loosely themed in four groups —
/// bag, trenches, tips, and game/mission callouts — so the rotation stays varied
/// across a busy day of relaying.
pub static FOOTERS: &[&str] = &[
    // -- bag --
    "let's get this bag",
    "bag secured, next",
    "the bag does not sleep",
    "gm, bag",
    "one bag at a time",
    "in pursuit of the bag",
    "the bag is the way",
    "eyes on the bag",
    "no days off, only bags",
    "bag loading…",
    "stay bag-pilled",
    "the bag remembers",
    "another day, another bag",
    "the bag is patient",
    "wagmi, bag included",
    "the bag waits for no one",
    "secure the bag, then sleep",
    "bag in, bag out",
    "chasing the bag since block zero",
    "no bag, no glory",
    "the bag compounds",
    "eyes on the bag, always",
    "all gas, no bag left behind",
    "the bag is a lifestyle",
    "bag acquired",
    // -- trenches --
    "survived the trenches",
    "we go again — the trenches don't quit",
    "born in the trenches",
    "the trenches remember",
    "another day in the trenches",
    "no retreat, no surrender, no paper hands",
    "hold the line",
    "war. war never changes.",
    "war changes everything",
    "the bag never changes",
    // -- tips --
    "tip: the trend is your friend until it ends",
    "tip: never risk the bag you can't replace",
    "tip: green candles lie, red candles teach",
    "tip: the exit is a skill, not an afterthought",
    "tip: zoom out",
    // -- game / mission --
    "respawn: back in the trenches",
    "mission failed successfully",
    "objective: extract the bag",
    "tactical bag secured",
    "stay frosty",
];

/// Pick a random footer variant.
pub fn footer_variant() -> &'static str {
    let i = rand::thread_rng().gen_range(0..FOOTERS.len());
    FOOTERS[i]
}

/// Context needed to build a rich embed beyond the message body itself.
///
/// `Default` is hand-written rather than derived so `color` lands on
/// [`DEFAULT_EMBED_COLOR`]; a derived `u32::default()` would be `0x000000`,
/// i.e. a black stripe on every embed built from `..Default::default()`.
#[derive(Debug, Clone)]
pub struct EmbedMeta {
    /// Channel/chat title shown as the embed author.
    pub title: String,
    /// Optional author icon (channel avatar) URL.
    pub avatar_url: Option<String>,
    /// Optional `t.me` deep link to the source message.
    pub deep_link: Option<String>,
    /// emoji -> count.
    pub reactions: BTreeMap<String, i32>,
    /// Discussion-thread comment count.
    pub comment_count: i32,
    /// Whether the source message was deleted on Telegram.
    pub deleted: bool,
    /// Left-bar stripe color as a 24-bit RGB integer (Discord wants a decimal
    /// int in the payload; `serde_json` emits it that way).
    pub color: u32,
    /// The source message's ORIGINAL Telegram publish time, RFC3339
    /// (see [`embed_timestamp`]). `None` when it is not recoverable.
    pub timestamp: Option<String>,
}

impl Default for EmbedMeta {
    fn default() -> Self {
        EmbedMeta {
            title: String::new(),
            avatar_url: None,
            deep_link: None,
            reactions: BTreeMap::new(),
            comment_count: 0,
            deleted: false,
            color: DEFAULT_EMBED_COLOR,
            timestamp: None,
        }
    }
}

/// Format the stats line, e.g. `❤️ 47 · 🔥 12 · 💬 8`.
///
/// Reactions are ordered by count (desc) then emoji (asc) for determinism;
/// comments, if any, are appended with a 💬 marker. Empty when nothing to show.
pub fn stats_line(reactions: &BTreeMap<String, i32>, comment_count: i32) -> String {
    let mut items: Vec<(&String, i32)> = reactions
        .iter()
        .filter(|(_, &c)| c > 0)
        .map(|(k, &c)| (k, c))
        .collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut parts: Vec<String> = items.iter().map(|(e, c)| format!("{e} {c}")).collect();
    if comment_count > 0 {
        parts.push(format!("💬 {comment_count}"));
    }
    parts.join(" · ")
}

/// Wrap every non-empty line of `s` in markdown strikethrough.
fn strike(s: &str) -> String {
    s.lines()
        .map(|l| {
            if l.trim().is_empty() {
                l.to_string()
            } else {
                format!("~~{l}~~")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a string into `<= limit`-char chunks (char-safe, no word wrapping).
fn split_desc(s: &str, limit: usize) -> Vec<String> {
    if s.chars().count() <= limit {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(limit)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// Build the Discord `embeds` array (a JSON array of embed objects) for a post.
///
/// The body becomes the description, split across multiple embeds when it
/// exceeds [`EMBED_DESC_LIMIT`] (capped at [`MAX_EMBEDS`]). The first embed
/// carries the author (channel title + optional avatar/deep-link); the last
/// carries the stats line, the masked `↗ View on Telegram` link, the
/// `by zayd — {variant}` footer, and the source post's timestamp (Discord
/// renders it beside the footer). The stripe `color` is set on EVERY embed so a
/// split post reads as one block rather than a colored head and gray tail.
pub fn embed(post: &RelayText, meta: &EmbedMeta) -> Value {
    let mut body = String::new();
    if post.edited {
        body.push_str("(edited) ");
    }
    body.push_str(&post.body);

    let mut description = if meta.deleted {
        let struck = strike(&body);
        if struck.is_empty() {
            "🗑 deleted on Telegram".to_string()
        } else {
            format!("{struck}\n🗑 deleted on Telegram")
        }
    } else {
        body
    };
    if description.is_empty() {
        description.push('\u{200b}'); // zero-width space: Discord rejects empty descriptions
    }

    let mut chunks = split_desc(&description, EMBED_DESC_LIMIT);
    if chunks.len() > MAX_EMBEDS {
        chunks.truncate(MAX_EMBEDS);
    }

    let last = chunks.len() - 1;
    let mut embeds = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut e = json!({ "description": chunk, "color": meta.color });

        if i == 0 {
            let mut author = json!({ "name": meta.title });
            if let Some(link) = &meta.deep_link {
                author["url"] = json!(link);
            }
            if let Some(icon) = &meta.avatar_url {
                author["icon_url"] = json!(icon);
            }
            e["author"] = author;
        }

        if i == last {
            let stats = stats_line(&meta.reactions, meta.comment_count);
            let mut value = String::new();
            if !stats.is_empty() {
                value.push_str(&stats);
            }
            if let Some(link) = &meta.deep_link {
                if !value.is_empty() {
                    value.push('\n');
                }
                value.push_str(&format!("[↗ View on Telegram]({link})"));
            }
            if !value.is_empty() {
                e["fields"] = json!([{ "name": "\u{200b}", "value": value, "inline": false }]);
            }
            e["footer"] = json!({ "text": format!("by zayd — {}", footer_variant()) });
            if let Some(ts) = &meta.timestamp {
                e["timestamp"] = json!(ts);
            }
        }

        embeds.push(e);
    }

    Value::Array(embeds)
}

#[derive(Debug, Clone)]
pub struct RelayText {
    pub sender: Option<String>,
    pub body: String,
    pub reply_quote: Option<String>,
    pub edited: bool,
}

pub fn passes_filter(body: &str, f: &Filter) -> bool {
    let lower = body.to_lowercase();
    if f.exclude_hashtags
        .iter()
        .any(|h| lower.contains(&h.to_lowercase()))
    {
        return false;
    }
    if f.any_keywords.is_empty() {
        return true;
    }
    f.any_keywords
        .iter()
        .any(|k| lower.contains(&k.to_lowercase()))
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
            let para_chars: Vec<char> = para.chars().collect();
            for piece_chars in para_chars.chunks(limit) {
                let piece: String = piece_chars.iter().collect();
                if !cur.is_empty() {
                    chunks.push(std::mem::take(&mut cur));
                }
                cur = piece;
            }
        } else {
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(para);
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Filter;

    #[test]
    fn image_filename_detection() {
        for ok in ["a.jpg", "b.JPEG", "c.png", "d.gif", "e.WEBP"] {
            assert!(is_image_filename(ok), "{ok} should be an image");
        }
        for no in ["clip.mp4", "doc.pdf", "v.mov", "noext", "song.mp3"] {
            assert!(!is_image_filename(no), "{no} should NOT be an image");
        }
    }

    #[test]
    fn single_image_referenced_inside_embed() {
        let mut embeds = json!([{ "description": "gm" }]);
        attach_image_attachments(&mut embeds, &["photo_1.jpg"], Some("https://t.me/x/1"));
        let e = &embeds.as_array().unwrap()[0];
        // image.url must EXACTLY match the attached filename, or Discord shows nothing.
        assert_eq!(e["image"]["url"], "attachment://photo_1.jpg");
        // A single image needs no gallery url.
        assert!(e.get("url").is_none());
        assert_eq!(embeds.as_array().unwrap().len(), 1);
    }

    #[test]
    fn album_images_form_a_shared_url_gallery() {
        let mut embeds = json!([{ "description": "album" }]);
        let names = ["a.jpg", "b.png", "c.webp"];
        attach_image_attachments(&mut embeds, &names, Some("https://t.me/x/1"));
        let arr = embeds.as_array().unwrap();
        assert_eq!(arr.len(), 3, "one primary + two image-only embeds");
        // Every embed shares the gallery url so Discord merges them.
        for e in arr {
            assert_eq!(e["url"], "https://t.me/x/1");
        }
        assert_eq!(arr[0]["image"]["url"], "attachment://a.jpg");
        assert_eq!(arr[1]["image"]["url"], "attachment://b.png");
        assert_eq!(arr[2]["image"]["url"], "attachment://c.webp");
    }

    #[test]
    fn no_images_leaves_embeds_untouched() {
        let mut embeds = json!([{ "description": "just text" }]);
        attach_image_attachments(&mut embeds, &[], None);
        assert!(embeds.as_array().unwrap()[0].get("image").is_none());
    }

    fn rt(body: &str) -> RelayText {
        RelayText {
            sender: Some("Rob".into()),
            body: body.into(),
            reply_quote: None,
            edited: false,
        }
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
        let t = RelayText {
            sender: None,
            body: "hi".into(),
            reply_quote: Some("orig".into()),
            edited: true,
        };
        let out = render(&t);
        assert!(out[0].starts_with("> orig\n"));
        assert!(out[0].contains("(edited) hi"));
    }

    #[test]
    fn filter_keyword_and_hashtag() {
        let f = Filter {
            any_keywords: vec!["entry".into()],
            exclude_hashtags: vec!["#ad".into()],
        };
        assert!(passes_filter("ENTRY now", &f));
        assert!(!passes_filter("random", &f));
        assert!(!passes_filter("entry #ad", &f)); // exclusion wins
    }

    #[test]
    fn empty_filter_passes_everything() {
        assert!(passes_filter("anything", &Filter::default()));
    }

    fn reactions(pairs: &[(&str, i32)]) -> BTreeMap<String, i32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn meta() -> EmbedMeta {
        EmbedMeta {
            title: "Rob's Channel".into(),
            avatar_url: None,
            deep_link: Some("https://t.me/robthinks/42".into()),
            reactions: reactions(&[("❤️", 47), ("🔥", 12)]),
            comment_count: 8,
            deleted: false,
            color: DEFAULT_EMBED_COLOR,
            timestamp: Some("2026-07-20T12:00:00Z".into()),
        }
    }

    #[test]
    fn stats_line_orders_by_count_then_appends_comments() {
        let line = stats_line(&reactions(&[("❤️", 47), ("🔥", 12)]), 8);
        assert_eq!(line, "❤️ 47 · 🔥 12 · 💬 8");
    }

    #[test]
    fn stats_line_empty_when_nothing() {
        assert_eq!(stats_line(&BTreeMap::new(), 0), "");
    }

    #[test]
    fn footer_variant_is_from_list() {
        for _ in 0..50 {
            assert!(FOOTERS.contains(&footer_variant()));
        }
    }

    #[test]
    fn footer_rotation_is_wide_and_unique() {
        // The rotation was expanded from the original 15 to 45; a shrinking
        // list (or a copy-paste duplicate) makes the footer repeat noticeably
        // on a busy relay day, so both are locked down here.
        assert_eq!(FOOTERS.len(), 45, "footer rotation changed size");
        let unique: std::collections::BTreeSet<&&str> = FOOTERS.iter().collect();
        assert_eq!(unique.len(), FOOTERS.len(), "duplicate footer variant");
        assert!(
            FOOTERS.iter().all(|f| !f.trim().is_empty()),
            "empty footer variant"
        );
    }

    #[test]
    fn footer_rotation_actually_varies() {
        // A stuck RNG (or an accidental FOOTERS[0]) would still pass
        // `footer_variant_is_from_list`; this catches it.
        let seen: std::collections::BTreeSet<&str> = (0..500).map(|_| footer_variant()).collect();
        assert!(
            seen.len() > 10,
            "footer_variant looks stuck: only {} distinct in 500 draws",
            seen.len()
        );
    }

    #[test]
    fn embed_has_author_footer_stats_and_deep_link() {
        let out = embed(&rt("gm frens"), &meta());
        let arr = out.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        let e = &arr[0];
        assert_eq!(e["author"]["name"], "Rob's Channel");
        assert_eq!(e["author"]["url"], "https://t.me/robthinks/42");
        assert_eq!(e["description"], "gm frens");
        let footer = e["footer"]["text"].as_str().unwrap();
        assert!(footer.starts_with("by zayd — "));
        assert!(FOOTERS.contains(&&footer["by zayd — ".len()..]));
        let field_val = e["fields"][0]["value"].as_str().unwrap();
        assert!(field_val.contains("❤️ 47 · 🔥 12 · 💬 8"));
        assert!(field_val.contains("[↗ View on Telegram](https://t.me/robthinks/42)"));
    }

    #[test]
    fn embed_carries_color_and_source_timestamp() {
        let out = embed(&rt("gm frens"), &meta());
        let e = &out[0];
        // Discord wants a decimal int, not a "#RRGGBB" string.
        assert_eq!(e["color"], json!(DEFAULT_EMBED_COLOR));
        assert_eq!(e["color"].as_u64(), Some(2733814)); // 0x29B6F6
        assert_eq!(e["timestamp"], "2026-07-20T12:00:00Z");
    }

    #[test]
    fn embed_honors_custom_color() {
        let mut m = meta();
        m.color = 0xFF0000;
        let out = embed(&rt("red"), &m);
        assert_eq!(out[0]["color"].as_u64(), Some(0xFF0000));
    }

    #[test]
    fn embed_omits_timestamp_when_unknown() {
        let mut m = meta();
        m.timestamp = None;
        let out = embed(&rt("no clock"), &m);
        assert!(out[0].get("timestamp").is_none());
        // ...but the stripe is still there.
        assert_eq!(out[0]["color"].as_u64(), Some(DEFAULT_EMBED_COLOR as u64));
    }

    #[test]
    fn split_embeds_all_share_color_but_timestamp_rides_the_last() {
        let big = "x".repeat(EMBED_DESC_LIMIT * 2 + 10);
        let out = embed(&rt(&big), &meta());
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert!(
            arr.iter()
                .all(|e| e["color"].as_u64() == Some(DEFAULT_EMBED_COLOR as u64)),
            "every chunk must carry the stripe, not just the first"
        );
        assert!(arr[0].get("timestamp").is_none());
        assert!(arr[1].get("timestamp").is_none());
        assert_eq!(arr[2]["timestamp"], "2026-07-20T12:00:00Z");
    }

    #[test]
    fn default_embed_meta_is_neon_blue_not_black() {
        // A derived Default would silently yield 0x000000 here.
        assert_eq!(EmbedMeta::default().color, DEFAULT_EMBED_COLOR);
        assert_ne!(EmbedMeta::default().color, 0);
    }

    #[test]
    fn embed_timestamp_formats_rfc3339_utc_seconds() {
        use chrono::TimeZone;
        let dt = chrono::Utc.with_ymd_and_hms(2026, 7, 20, 18, 4, 5).unwrap();
        assert_eq!(embed_timestamp(dt), "2026-07-20T18:04:05Z");
    }

    #[test]
    fn embed_timestamp_truncates_subsecond_precision() {
        use chrono::TimeZone;
        let dt = chrono::Utc.timestamp_millis_opt(1_753_000_000_123).unwrap();
        let s = embed_timestamp(dt);
        assert!(s.ends_with('Z'), "expected a Z-suffixed UTC stamp: {s}");
        assert!(!s.contains('.'), "sub-second precision leaked: {s}");
    }

    #[test]
    fn embed_edited_prefix_in_description() {
        let t = RelayText {
            sender: None,
            body: "updated text".into(),
            reply_quote: None,
            edited: true,
        };
        let out = embed(&t, &meta());
        assert_eq!(out[0]["description"], "(edited) updated text");
    }

    #[test]
    fn embed_deleted_strikes_and_marks() {
        let mut m = meta();
        m.deleted = true;
        let out = embed(&rt("bye"), &m);
        let desc = out[0]["description"].as_str().unwrap();
        assert!(desc.contains("~~bye~~"));
        assert!(desc.contains("🗑 deleted on Telegram"));
    }

    #[test]
    fn embed_splits_body_over_4096_into_multiple_embeds() {
        let big = "x".repeat(EMBED_DESC_LIMIT * 2 + 10);
        let out = embed(&rt(&big), &meta());
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // author only on first, footer only on last
        assert!(arr[0].get("author").is_some());
        assert!(arr[0].get("footer").is_none());
        assert!(arr[2].get("author").is_none());
        assert!(arr[2].get("footer").is_some());
        assert!(arr
            .iter()
            .all(|e| e["description"].as_str().unwrap().chars().count() <= EMBED_DESC_LIMIT));
    }

    #[test]
    fn embed_caps_at_max_embeds() {
        let huge = "y".repeat(EMBED_DESC_LIMIT * (MAX_EMBEDS + 5));
        let out = embed(&rt(&huge), &meta());
        assert_eq!(out.as_array().unwrap().len(), MAX_EMBEDS);
    }

    #[test]
    fn oversized_paragraph_hard_splits_cleanly() {
        // Create a paragraph that exceeds the limit
        let oversized_para = "x".repeat(3000);
        let chunks = split_chunks(&oversized_para, DISCORD_LIMIT);

        // Assert all chunks are non-empty
        assert!(chunks.iter().all(|c| !c.is_empty()), "found empty chunk");

        // Assert chunks are in order (just verify they exist in sequence)
        assert!(!chunks.is_empty(), "should have at least one chunk");

        // Assert each chunk is <= limit
        assert!(
            chunks.iter().all(|c| c.chars().count() <= DISCORD_LIMIT),
            "chunk exceeds limit"
        );

        // Assert concatenation reconstructs the original (without boundaries)
        let reconstructed = chunks.join("");
        assert_eq!(
            reconstructed, oversized_para,
            "concatenation should reconstruct the input"
        );
    }
}
