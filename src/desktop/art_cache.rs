//! Byte-bounded keep-alive storage. Cloned live resources belong to their caller.

use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    sync::atomic::{AtomicU64, Ordering},
};

struct Entry<V> {
    value: V,
    bytes: usize,
    used: AtomicU64,
}

pub struct ByteLru<K, V> {
    entries: HashMap<K, Entry<V>>,
    clock: AtomicU64,
    bytes: usize,
    budget: usize,
    limit: usize,
}

impl<K: Eq + Hash + Clone, V> ByteLru<K, V> {
    pub fn new(budget: usize, limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: AtomicU64::new(0),
            bytes: 0,
            budget,
            limit,
        }
    }

    pub fn get<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let entry = self.entries.get(key)?;
        entry.used.store(
            self.clock.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        Some(&entry.value)
    }

    pub fn contains_key<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.entries.contains_key(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(key, entry)| (key, &entry.value))
    }

    pub fn remove<Q: Hash + Eq + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let entry = self.entries.remove(key)?;
        self.bytes -= entry.bytes;
        Some(entry.value)
    }

    pub fn insert(&mut self, key: K, value: V, bytes: usize) {
        self.remove(&key);
        if bytes > self.budget || self.limit == 0 {
            return;
        }
        while self.bytes + bytes > self.budget || self.entries.len() >= self.limit {
            let Some(old) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove(&old);
        }
        self.entries.insert(
            key,
            Entry {
                value,
                bytes,
                used: AtomicU64::new(self.clock.fetch_add(1, Ordering::Relaxed)),
            },
        );
        self.bytes += bytes;
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn evicts_by_bytes_and_last_access_not_insertion_order() {
        let mut cache = ByteLru::new(10, 10);
        cache.insert("old", 1, 4);
        cache.insert("recent", 2, 4);
        assert_eq!(cache.get(&"old"), Some(&1));
        cache.insert("large", 3, 6);
        assert!(cache.get(&"recent").is_none());
        assert_eq!(cache.get(&"old"), Some(&1));
        assert_eq!(cache.bytes(), 10);
        cache.insert("old", 4, 2);
        assert_eq!(cache.bytes(), 8);
        cache.insert("oversized", 5, 11);
        assert!(!cache.contains_key(&"oversized"));
        assert_eq!(cache.bytes(), 8);
    }

    #[test]
    fn eviction_releases_cache_owner_but_preserves_live_resource() {
        let mut cache = ByteLru::new(4, 2);
        let image = Arc::new(vec![0u8; 4]);
        let weak = Arc::downgrade(&image);
        cache.insert(1, image.clone(), 4);
        cache.insert(2, Arc::new(vec![1u8; 4]), 4);
        assert_eq!(Arc::strong_count(&image), 1);
        drop(image);
        assert!(weak.upgrade().is_none());
        assert_eq!(cache.bytes(), 4);
    }

    #[test]
    fn zero_byte_statuses_are_also_bounded() {
        let mut cache = ByteLru::new(10, 3);
        for i in 0..1000 {
            cache.insert(i, (), 0);
        }
        assert_eq!(cache.iter().count(), 3);
        assert!(cache.contains_key(&999));
    }
}
