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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_existing_arc_for_same_string() {
        let mut interner = StringInterner::new();
        let first = interner.intern("terminal");
        let second = interner.intern("terminal");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn cleans_dead_entries_and_resets_capacity_when_full() {
        let mut interner = StringInterner::new();
        interner.max_entries = 2;

        let first = interner.intern("one");
        let second = interner.intern("two");
        assert_eq!(interner.map.len(), 2);

        drop(first);
        drop(second);

        // This call should purge dead refs and then reuse capacity.
        let third = interner.intern("three");
        assert_eq!(interner.map.len(), 1);
        assert!(Arc::ptr_eq(&third, &interner.intern("three")));

        // Fill again to trigger force-clear path.
        let _ = interner.intern("alpha");
        let _ = interner.intern("beta");
        assert!(interner.map.len() <= interner.max_entries);
    }

    #[test]
    fn cleanup_clears_when_capacity_reached_with_live_entries() {
        let mut interner = StringInterner::new();
        interner.max_entries = 1;

        let one = interner.intern("one");
        let _two = interner.intern("two");

        assert_eq!(interner.map.len(), 1);
        assert!(interner.map.contains_key("two"));
        assert!(!interner.map.contains_key("one"));
        let _three = interner.intern("two");
        drop(one);
    }
}
