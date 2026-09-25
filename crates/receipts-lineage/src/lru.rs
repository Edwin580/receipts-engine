use std::collections::HashMap;
use std::hash::Hash;

/// A small least-recently-used map. Eviction scans all entries, which is
/// fine at the few hundred entries the trace cache holds.
#[derive(Debug)]
pub(crate) struct Lru<K, V> {
    capacity: usize,
    clock: u64,
    entries: HashMap<K, (V, u64)>,
}

impl<K: Hash + Eq + Clone, V: Clone> Lru<K, V> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "an LRU needs room for one entry");
        Self {
            capacity,
            clock: 0,
            entries: HashMap::new(),
        }
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(key).map(|(v, used)| {
            *used = clock;
            v.clone()
        })
    }

    pub fn insert(&mut self, key: K, value: V) {
        self.clock += 1;
        if !self.entries.contains_key(&key)
            && self.entries.len() >= self.capacity
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, (value, self.clock));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let mut l = Lru::new(2);
        l.insert(1, "a");
        l.insert(2, "b");
        assert_eq!(l.get(&1), Some("a")); // 2 is now oldest
        l.insert(3, "c");
        assert_eq!(l.get(&2), None);
        assert_eq!(l.get(&1), Some("a"));
        assert_eq!(l.get(&3), Some("c"));
        l.insert(3, "c2"); // update in place, no eviction
        assert_eq!(l.len(), 2);
        assert_eq!(l.get(&3), Some("c2"));
    }
}
