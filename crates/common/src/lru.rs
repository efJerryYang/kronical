use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

pub struct LruCache<K, V> {
    map: HashMap<K, (V, Instant)>,
    capacity: usize,
    ttl: Option<Duration>,
}

impl<K: Eq + Hash + Clone, V: Clone> LruCache<K, V> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            capacity: capacity.max(1),
            ttl: None,
        }
    }

    pub fn with_capacity_and_ttl(capacity: usize, ttl: Duration) -> Self {
        Self {
            map: HashMap::new(),
            capacity: capacity.max(1),
            ttl: Some(ttl),
        }
    }

    pub fn len(&self) -> usize {
        #[allow(dead_code)]
        self.map.len()
    }

    pub fn get_cloned(&mut self, key: &K) -> Option<V> {
        let now = Instant::now();
        if let Some((v, ts)) = self.map.get_mut(key) {
            if let Some(ttl) = self.ttl {
                if now.duration_since(*ts) > ttl {
                    self.map.remove(key);
                    return None;
                }
            }
            *ts = now;
            return Some(v.clone());
        }
        None
    }

    pub fn put(&mut self, key: K, value: V) {
        let now = Instant::now();
        self.map.insert(key.clone(), (value, now));
        self.evict_if_needed();
    }

    fn evict_if_needed(&mut self) {
        if self.map.len() <= self.capacity {
            return;
        }
        if let Some((oldest_key, _)) = self
            .map
            .iter()
            .min_by_key(|(_k, (_v, ts))| *ts)
            .map(|(k, v)| (k.clone(), v.clone()))
        {
            self.map.remove(&oldest_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn evicts_least_recently_used_entry() {
        let mut cache = LruCache::with_capacity(2);
        let key_a = "a".to_string();
        let key_b = "b".to_string();
        let key_c = "c".to_string();

        cache.put(key_a.clone(), 1);
        cache.put(key_b.clone(), 2);

        // Refresh key_a so key_b becomes the oldest entry.
        assert_eq!(cache.get_cloned(&key_a), Some(1));

        cache.put(key_c.clone(), 3);

        assert_eq!(cache.get_cloned(&key_b), None);
        assert_eq!(cache.get_cloned(&key_a), Some(1));
        assert_eq!(cache.get_cloned(&key_c), Some(3));
    }

    #[test]
    fn ttl_expiration_removes_stale_entries() {
        let mut cache = LruCache::with_capacity_and_ttl(4, Duration::from_millis(20));
        let key = "ttl-key".to_string();

        cache.put(key.clone(), 5);
        assert_eq!(cache.get_cloned(&key), Some(5));

        thread::sleep(Duration::from_millis(30));

        assert_eq!(cache.get_cloned(&key), None);
        assert_eq!(cache.len(), 0);
    }
}
