use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    ops::Deref,
    sync::{LazyLock, Mutex},
};

use dashmap::DashMap;

pub struct Registry<K, V: ?Sized + 'static> {
    slow: Mutex<Option<HashMap<K, &'static V>>>,
    fast: LazyLock<DashMap<K, &'static V>>,
}

impl<K: Hash + Eq + Clone, V: ?Sized + 'static> Registry<K, V> {
    pub const fn new() -> Self {
        Self {
            slow: Mutex::new(None),
            fast: LazyLock::new(DashMap::new),
        }
    }

    pub fn get(&self, key: K, value: impl FnOnce() -> Box<V>) -> &'static V {
        if let Some(value) = self.fast.view(&key, |_, value| *value) {
            return value;
        }
        let mut metadatas = self.slow.lock().unwrap();
        let metadatas = metadatas.get_or_insert_with(HashMap::new);
        metadatas.entry(key).or_insert_with_key(|key| {
            let value = &*Box::leak(value());
            self.fast.insert(key.clone(), value);
            value
        })
    }

    pub fn try_get(&self, key: &K) -> Option<&'static V> {
        if let Some(value) = self.fast.view(key, |_, value| *value) {
            return Some(value);
        }
        Some(self.slow.lock().ok()?.as_ref()?.get(key)?)
    }
}

impl<K: Hash + Eq + Clone, V: ?Sized + 'static> Default for Registry<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

struct RefPtr<'a, T: ?Sized + 'static>(&'a T);

impl<T: ?Sized + 'static> Clone for RefPtr<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized + 'static> Copy for RefPtr<'_, T> {}

impl<T: ?Sized + 'static> PartialEq for RefPtr<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(self.0, other.0)
    }
}

impl<T: ?Sized + 'static> Eq for RefPtr<'_, T> {}

impl<T: ?Sized + 'static> Hash for RefPtr<'_, T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::from_ref(self.0).hash(state);
    }
}

impl<T: ?Sized + 'static> Deref for RefPtr<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

struct SrPtr<T: ?Sized + 'static>(RefPtr<'static, T>);

impl<T: ?Sized + 'static> Clone for SrPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized + 'static> Copy for SrPtr<T> {}

impl<T: ?Sized + 'static> PartialEq for SrPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: ?Sized + 'static> Eq for SrPtr<T> {}

impl<T: ?Sized + 'static> Hash for SrPtr<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<'a, T: ?Sized + 'static> Borrow<RefPtr<'a, T>> for SrPtr<T> {
    fn borrow(&self) -> &RefPtr<'a, T> {
        &self.0
    }
}

impl<T: ?Sized + 'static> Deref for SrPtr<T> {
    type Target = &'static T;

    fn deref(&self) -> &Self::Target {
        &self.0 .0
    }
}

type Slow<A, B> = (HashMap<SrPtr<A>, SrPtr<B>>, HashMap<SrPtr<B>, SrPtr<A>>);

type Fast<A, B> = (DashMap<SrPtr<A>, SrPtr<B>>, DashMap<SrPtr<B>, SrPtr<A>>);

pub struct SymmetricRegistry<A: ?Sized + 'static, B: ?Sized + 'static> {
    slow: Mutex<Option<Slow<A, B>>>,
    fast: LazyLock<Fast<A, B>>,
}

impl<A: ?Sized + 'static, B: ?Sized + 'static> SymmetricRegistry<A, B> {
    pub const fn new() -> Self {
        Self {
            slow: Mutex::new(None),
            fast: LazyLock::new(|| (DashMap::new(), DashMap::new())),
        }
    }

    pub fn get_ab(&self, key: &'static A, value: impl FnOnce() -> Box<B>) -> &'static B {
        let key = RefPtr(key);
        if let Some(value) = self.fast.0.view(&key, |_, value| *value) {
            return &value;
        }
        let mut metadatas = self.slow.lock().unwrap();
        let metadatas = metadatas.get_or_insert_with(|| (HashMap::new(), HashMap::new()));
        metadatas.0.entry(SrPtr(key)).or_insert_with_key(|key| {
            let value = SrPtr(RefPtr(Box::leak(value())));
            self.fast.0.insert(*key, value);
            self.fast.1.insert(value, *key);
            metadatas.1.insert(value, *key);
            value
        })
    }

    pub fn get_ba(&self, key: &'static B, value: impl FnOnce() -> Box<A>) -> &'static A {
        let key = RefPtr(key);
        if let Some(value) = self.fast.1.view(&key, |_, value| *value) {
            return &value;
        }
        let mut metadatas = self.slow.lock().unwrap();
        let metadatas = metadatas.get_or_insert_with(|| (HashMap::new(), HashMap::new()));
        metadatas.1.entry(SrPtr(key)).or_insert_with_key(|key| {
            let value = SrPtr(RefPtr(Box::leak(value())));
            self.fast.1.insert(*key, value);
            self.fast.0.insert(value, *key);
            metadatas.0.insert(value, *key);
            value
        })
    }

    pub fn try_get_ab(&self, key: &A) -> Option<&'static B> {
        let key = RefPtr(key);
        if let Some(value) = self.fast.0.view(&key, |_, value| *value) {
            return Some(&value);
        }
        Some(self.slow.lock().ok()?.as_ref()?.0.get(&key)?)
    }

    pub fn try_get_ba(&self, key: &B) -> Option<&'static A> {
        let key = RefPtr(key);
        if let Some(value) = self.fast.1.view(&key, |_, value| *value) {
            return Some(&value);
        }
        Some(self.slow.lock().ok()?.as_ref()?.1.get(&key)?)
    }
}

impl<K: Hash + Eq + Clone, V: ?Sized + 'static> Default for SymmetricRegistry<K, V> {
    fn default() -> Self {
        Self::new()
    }
}
