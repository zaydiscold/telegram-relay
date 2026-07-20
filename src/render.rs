#![allow(dead_code)]

use crate::config::Filter;

const DISCORD_LIMIT: usize = 2000;

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
