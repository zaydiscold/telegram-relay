#![allow(dead_code)]

use crate::config::{ChatId, Filter, MediaMode, WebhookName};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub name: String,
    pub chat: ChatId,
    pub to: Vec<WebhookName>,
    pub filter: Option<Filter>,
    /// Embed stripe color for posts relayed by this route.
    pub color: u32,
    /// Optional per-route media mode; `None` falls back to the global
    /// `media.mode` (resolved at use against the live [`crate::config::MediaCfg`]
    /// snapshot). (queued-polish §11c)
    pub media_mode: Option<MediaMode>,
}

pub struct Router(HashMap<ChatId, Vec<ResolvedRoute>>);

impl Router {
    pub fn new(routes: Vec<ResolvedRoute>) -> Self {
        let mut m: HashMap<ChatId, Vec<ResolvedRoute>> = HashMap::new();
        for r in routes {
            m.entry(r.chat).or_default().push(r);
        }
        Router(m)
    }
    pub fn match_chat(&self, chat: ChatId) -> &[ResolvedRoute] {
        self.0.get(&chat).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_configured_chat() {
        let r = Router::new(vec![ResolvedRoute {
            name: "r".into(),
            chat: ChatId(5),
            to: vec![WebhookName("h".into())],
            filter: None,
            color: crate::render::DEFAULT_EMBED_COLOR,
            media_mode: None,
        }]);
        assert_eq!(r.match_chat(ChatId(5)).len(), 1);
        assert!(r.match_chat(ChatId(6)).is_empty());
    }

    #[test]
    fn one_chat_two_routes_fan_out() {
        let mk = |n: &str| ResolvedRoute {
            name: n.into(),
            chat: ChatId(5),
            to: vec![WebhookName(n.into())],
            filter: None,
            color: crate::render::DEFAULT_EMBED_COLOR,
            media_mode: None,
        };
        let r = Router::new(vec![mk("a"), mk("b")]);
        assert_eq!(r.match_chat(ChatId(5)).len(), 2);
    }
}
