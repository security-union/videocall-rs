use std::borrow::Borrow;
use std::cmp::Ord;
use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug)]
pub struct HashMapWithOrderedKeys<K: Ord, V> {
    map: HashMap<K, V>,
    keys: Vec<K>,
}

//
// Only implementing the methods that are needed in peer_decode_manager
//
impl<K: Ord + Hash + Clone, V> HashMapWithOrderedKeys<K, V> {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            keys: vec![],
        }
    }

    //
    // Delegated methods
    //

    pub fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get(k)
    }

    pub fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get_mut(k)
    }

    pub fn contains_key<Q>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(k)
    }

    //
    // Delegated methods with extra handling to maintain ordered keys
    //

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.map.insert(k.clone(), v).or_else(|| {
            self.keys.push(k);
            self.keys.sort();
            None
        })
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        if let Ok(index) = self.keys.binary_search(k) {
            self.keys.remove(index);
        }
        self.map.remove(k)
    }

    //
    // New methods
    //

    pub fn ordered_keys(&self) -> &Vec<K> {
        &self.keys
    }

    pub fn remove_if<F>(&mut self, predicate: F)
    where
        F: Fn(&mut V) -> bool,
    {
        let mut keys_to_remove = Vec::new();

        for key in &self.keys {
            if let Some(value) = self.map.get_mut(key) {
                if !predicate(value) {
                    keys_to_remove.push(key.clone());
                }
            }
        }

        for key in &keys_to_remove {
            self.map.remove(key);
            self.keys.retain(|k| k != key);
        }
    }
}
