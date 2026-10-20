// SPDX-License-Identifier: MPL-2.0

use core::hash::Hash;

use hashbrown::HashMap;

use crate::prelude::*;

pub(super) struct PriorityQueue<K, P, V> {
    entries: Vec<Entry<K, P, V>>,
    indices: HashMap<K, usize>,
}

struct Entry<K, P, V> {
    key: K,
    priority: P,
    value: V,
}

impl<K: Copy + Eq + Hash, P: Copy + Ord, V> PriorityQueue<K, P, V> {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
            indices: HashMap::new(),
        }
    }

    pub(super) fn reserve(&mut self, additional: usize) -> Result<()> {
        self.entries
            .try_reserve(additional)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        self.indices
            .try_reserve(additional)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        Ok(())
    }

    pub(super) fn first(&self) -> Option<(&K, &P, &V)> {
        self.entries
            .first()
            .map(|entry| (&entry.key, &entry.priority, &entry.value))
    }

    pub(super) fn get(&self, key: &K) -> Option<&V> {
        self.indices
            .get(key)
            .map(|index| &self.entries[*index].value)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn set(&mut self, key: K, priority: P, value: V) -> Result<()> {
        if let Some(&index) = self.indices.get(&key) {
            self.entries[index].priority = priority;
            self.entries[index].value = value;
            self.repair(index);
        } else {
            self.reserve(1)?;
            let index = self.entries.len();
            self.entries.push(Entry {
                key,
                priority,
                value,
            });
            self.indices.insert(key, index);
            self.sift_up(index);
        }
        Ok(())
    }

    pub(super) fn remove(&mut self, key: &K) -> Option<V> {
        let index = self.indices.remove(key)?;
        let removed = self.entries.swap_remove(index);
        if index < self.entries.len() {
            *self.indices.get_mut(&self.entries[index].key).unwrap() = index;
            self.repair(index);
        }
        Some(removed.value)
    }

    pub(super) fn pop(&mut self) -> Option<(K, P, V)> {
        let entry = self.entries.first()?;
        let (key, priority) = (entry.key, entry.priority);
        self.remove(&key).map(|value| (key, priority, value))
    }

    fn repair(&mut self, index: usize) {
        if index > 0 && self.entries[index].priority < self.entries[(index - 1) / 2].priority {
            self.sift_up(index);
        } else {
            self.sift_down(index);
        }
    }

    fn swap(&mut self, first: usize, second: usize) {
        self.entries.swap(first, second);
        *self.indices.get_mut(&self.entries[first].key).unwrap() = first;
        *self.indices.get_mut(&self.entries[second].key).unwrap() = second;
    }

    fn sift_up(&mut self, mut index: usize) {
        while index > 0 {
            let parent = (index - 1) / 2;
            if self.entries[parent].priority <= self.entries[index].priority {
                break;
            }
            self.swap(parent, index);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize) {
        while index < self.entries.len() / 2 {
            let left = index * 2 + 1;
            let right = left + 1;
            let first = if right < self.entries.len()
                && self.entries[right].priority < self.entries[left].priority
            {
                right
            } else {
                left
            };
            if self.entries[index].priority <= self.entries[first].priority {
                break;
            }
            self.swap(index, first);
            index = first;
        }
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::PriorityQueue;

    #[ktest]
    fn priority_changes_and_removal_preserve_fifo_ties() {
        let mut queue = PriorityQueue::new();
        for key in 0..32 {
            queue.set(key, (10, key), key * 2).unwrap();
        }
        queue.set(17, (1, 17), 34).unwrap();
        queue.set(3, (20, 3), 6).unwrap();
        assert_eq!(queue.first(), Some((&17, &(1, 17), &34)));
        assert_eq!(queue.remove(&17), Some(34));
        assert_eq!(queue.remove(&20), Some(40));
        for key in (0..32).filter(|key| ![3, 17, 20].contains(key)) {
            assert_eq!(queue.get(&key), Some(&(key * 2)));
            assert_eq!(queue.pop(), Some((key, (10, key), key * 2)));
        }
        assert_eq!(queue.pop(), Some((3, (20, 3), 6)));
        assert!(queue.is_empty());
        assert_eq!(queue.remove(&17), None);
        assert_eq!(queue.pop(), None);
    }
}
