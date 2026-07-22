use crate::config::ChatId;
use lru::LruCache;
use std::num::NonZeroUsize;

pub struct Dedup(LruCache<(ChatId, i32), ()>);

impl Dedup {
    pub fn new(cap: usize) -> Self {
        Dedup(LruCache::new(NonZeroUsize::new(cap.max(1)).unwrap()))
    }
    /// Returns true if (chat, msg_id) was not seen before.
    pub fn check_and_insert(&mut self, chat: ChatId, msg_id: i32) -> bool {
        self.0.put((chat, msg_id), ()).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_then_duplicate() {
        let mut d = Dedup::new(8);
        assert!(d.check_and_insert(ChatId(1), 100));
        assert!(!d.check_and_insert(ChatId(1), 100));
        assert!(d.check_and_insert(ChatId(2), 100)); // same msg id, other chat
    }

    #[test]
    fn evicts_at_capacity() {
        let mut d = Dedup::new(2);
        d.check_and_insert(ChatId(1), 1);
        d.check_and_insert(ChatId(1), 2);
        d.check_and_insert(ChatId(1), 3); // evicts (1,1)
        assert!(d.check_and_insert(ChatId(1), 1)); // fresh again
    }
}
