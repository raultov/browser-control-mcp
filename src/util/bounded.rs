//! A map with a size cap, so state fed by the browser cannot grow forever.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// A `HashMap` that keeps at most `CAP` entries, evicting the oldest
/// insertion first. Reads go through `Deref`; writes only through the
/// bounded methods, so the cap cannot be bypassed.
#[derive(Debug, Clone)]
pub struct BoundedMap<K, V, const CAP: usize> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V, const CAP: usize> Default for BoundedMap<K, V, CAP> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<K: Eq + Hash + Clone, V, const CAP: usize> BoundedMap<K, V, CAP> {
    /// Inserts or replaces `key`; returns the key evicted to respect the cap.
    pub fn insert(&mut self, key: K, value: V) -> Option<K> {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        if self.map.len() <= CAP {
            return None;
        }
        let oldest = self.order.pop_front()?;
        self.map.remove(&oldest);
        Some(oldest)
    }

    /// Mutable access to an existing entry.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get_mut(key)
    }

    /// Removes `key`.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.order.retain(|k| k != key);
        self.map.remove(key)
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

impl<K, V, const CAP: usize> IntoIterator for BoundedMap<K, V, CAP> {
    type Item = (K, V);
    type IntoIter = std::collections::hash_map::IntoIter<K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.into_iter()
    }
}

impl<K: serde::Serialize, V: serde::Serialize, const CAP: usize> serde::Serialize
    for BoundedMap<K, V, CAP>
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.map.serialize(serializer)
    }
}

impl<K, V, const CAP: usize> std::ops::Deref for BoundedMap<K, V, CAP> {
    type Target = HashMap<K, V>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_entry_is_evicted_and_replacing_keeps_the_slot() {
        let mut map = BoundedMap::<_, _, 2>::default();
        assert_eq!(map.insert("a", 1), None);
        assert_eq!(map.insert("b", 2), None);
        assert_eq!(map.insert("a", 3), None, "replacing does not grow the map");
        assert_eq!(map.insert("c", 4), Some("a"));
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("b") && map.contains_key("c"));
        assert_eq!(map.remove(&"b"), Some(2));
        assert_eq!(map.insert("d", 5), None);
    }
}
