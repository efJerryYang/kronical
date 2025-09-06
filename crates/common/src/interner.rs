use crate::maps::HashMap;
use std::sync::{Arc, Weak};

// A small string interner that keeps weak references to deduplicate Arc<String>s
// across the process without global locks. Designed for titles/app names.
pub struct StringInterner {
    map: HashMap<String, Weak<String>>,
    max_entries: usize,
}

impl StringInterner {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            max_entries: 4096,
        }
    }

    pub fn intern(&mut self, s: &str) -> Arc<String> {
        self.cleanup_dead_references();

        if let Some(weak_ref) = self.map.get(s) {
            if let Some(arc_string) = weak_ref.upgrade() {
                return arc_string;
            }
        }

        if self.map.len() >= self.max_entries {
            self.cleanup_dead_references();
        }

        let arc_string = Arc::new(s.to_string());
        self.map.insert(s.to_string(), Arc::downgrade(&arc_string));
        arc_string
    }

    fn cleanup_dead_references(&mut self) {
        self.map.retain(|_, weak_ref| weak_ref.strong_count() > 0);

        if self.map.len() >= self.max_entries {
            self.map.clear();
            self.map.shrink_to_fit();
        }
    }
}
