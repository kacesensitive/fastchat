use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{filter::FilterEngine, model::{ChatMessage, FilterDecision}};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredChatEntry {
    pub message: ChatMessage,
    pub filter: FilterDecision,
    pub inserted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct ChatStoreStats {
    pub total_messages: usize,
    pub visible_messages: usize,
    pub capacity: usize,
}

#[derive(Debug)]
pub struct ChatStore {
    capacity: usize,
    entries: VecDeque<StoredChatEntry>,
    visible_count: usize,
}

impl ChatStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity.min(1024)),
            visible_count: 0,
        }
    }

    pub fn push(&mut self, message: ChatMessage, filters: &FilterEngine) {
        if self.entries.len() == self.capacity {
            if let Some(old) = self.entries.pop_front() {
                if old.filter.visible {
                    self.visible_count = self.visible_count.saturating_sub(1);
                }
            }
        }

        let decision = filters.evaluate(&message);
        if decision.visible {
            self.visible_count += 1;
        }
        self.entries.push_back(StoredChatEntry {
            message,
            filter: decision,
            inserted_at: Utc::now(),
        });
    }

    pub fn mark_deleted(&mut self, message_id: &str) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.message.id == message_id {
                entry.message.flags.is_deleted = true;
                changed = true;
            }
        }
        changed
    }

    pub fn clear_visible_view(&mut self) {
        self.entries.clear();
        self.visible_count = 0;
    }

    pub fn recompute_filters(&mut self, filters: &FilterEngine) {
        let mut visible_count = 0;
        for entry in &mut self.entries {
            entry.filter = filters.evaluate(&entry.message);
            if entry.filter.visible {
                visible_count += 1;
            }
        }
        self.visible_count = visible_count;
    }

    pub fn visible_entries_cloned(&self) -> Vec<StoredChatEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.filter.visible)
            .cloned()
            .collect()
    }

    pub fn all_entries_cloned(&self) -> Vec<StoredChatEntry> {
        self.entries.iter().cloned().collect()
    }

    pub fn stats(&self) -> ChatStoreStats {
        ChatStoreStats {
            total_messages: self.entries.len(),
            visible_messages: self.visible_count,
            capacity: self.capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        filter::FilterEngine,
        model::{ChatMessage, MessageKind},
        store::ChatStore,
    };

    #[test]
    fn ring_buffer_evicts_old_messages() {
        let engine = FilterEngine::new(Default::default());
        let mut store = ChatStore::new(2);
        store.push(ChatMessage::new_text("c", "a", "A", "1", MessageKind::Chat), &engine);
        store.push(ChatMessage::new_text("c", "b", "B", "2", MessageKind::Chat), &engine);
        store.push(ChatMessage::new_text("c", "c", "C", "3", MessageKind::Chat), &engine);

        let visible = store.visible_entries_cloned();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].message.raw_text, "2");
        assert_eq!(visible[1].message.raw_text, "3");
    }
}
