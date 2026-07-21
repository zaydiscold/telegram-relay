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
#[derive(Debug, Clone, Default)]
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
/// carries the stats line, the masked `↗ View on Telegram` link, and the
/// `by zayd — {variant}` footer.
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
        let mut e = json!({ "description": chunk });

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
