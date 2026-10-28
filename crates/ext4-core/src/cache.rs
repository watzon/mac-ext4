//! A bounded cache of clean metadata blocks.
//!
//! Eviction uses the CLOCK algorithm: every entry has a "referenced" bit
//! that a hit sets. When the cache is full, a hand sweeps the entries,
//! clearing set bits, and evicts the first entry whose bit is already
//! clear. Each insertion evicts at most one entry, so the cost per
//! operation stays small and a burst of cold blocks cannot flush the whole
//! hot set at once.

use std::collections::HashMap;
use std::sync::Arc;

struct Slot {
    blk: u64,
    data: Arc<Vec<u8>>,
    referenced: bool,
}

pub struct BlockCache {
    /// Block number to index in `slots`.
    index: HashMap<u64, usize>,
    slots: Vec<Slot>,
    capacity: usize,
    hand: usize,
}

impl BlockCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(16);
        BlockCache {
            index: HashMap::with_capacity(capacity.min(4096)),
            slots: Vec::with_capacity(capacity.min(4096)),
            capacity,
            hand: 0,
        }
    }

    pub fn get(&mut self, blk: u64) -> Option<Arc<Vec<u8>>> {
        let &i = self.index.get(&blk)?;
        let slot = &mut self.slots[i];
        slot.referenced = true;
        Some(slot.data.clone())
    }

    pub fn insert(&mut self, blk: u64, data: Arc<Vec<u8>>) {
        if let Some(&i) = self.index.get(&blk) {
            let slot = &mut self.slots[i];
            slot.data = data;
            slot.referenced = true;
            return;
        }
        let slot = Slot {
            blk,
            data,
            referenced: false,
        };
        if self.slots.len() < self.capacity {
            self.index.insert(blk, self.slots.len());
            self.slots.push(slot);
            return;
        }
        let victim = self.find_victim();
        self.index.remove(&self.slots[victim].blk);
        self.index.insert(blk, victim);
        self.slots[victim] = slot;
        self.hand = (victim + 1) % self.slots.len();
    }

    pub fn remove(&mut self, blk: u64) {
        let Some(i) = self.index.remove(&blk) else {
            return;
        };
        self.slots.swap_remove(i);
        if let Some(moved) = self.slots.get(i) {
            self.index.insert(moved.blk, i);
        }
        if self.hand >= self.slots.len() {
            self.hand = 0;
        }
    }

    pub fn clear(&mut self) {
        self.index.clear();
        self.slots.clear();
        self.hand = 0;
    }

    /// Advance the hand to the next entry that was not referenced since
    /// the hand last passed it. Ends within two sweeps.
    fn find_victim(&mut self) -> usize {
        loop {
            let slot = &mut self.slots[self.hand];
            if !slot.referenced {
                return self.hand;
            }
            slot.referenced = false;
            self.hand = (self.hand + 1) % self.slots.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(v: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![v; 8])
    }

    #[test]
    fn insertion_at_capacity_evicts_one_entry() {
        let mut cache = BlockCache::new(16);
        for b in 0..16 {
            cache.insert(b, block(b as u8));
        }
        cache.insert(100, block(100));
        let present = (0..16).filter(|&b| cache.get(b).is_some()).count();
        assert_eq!(present, 15);
        assert!(cache.get(100).is_some());
    }

    #[test]
    fn referenced_entries_survive_cold_inserts() {
        let mut cache = BlockCache::new(16);
        for b in 0..16 {
            cache.insert(b, block(b as u8));
        }
        // Keep blocks 0..8 hot while 64 cold blocks stream through.
        for cold in 1000..1064 {
            for hot in 0..8 {
                assert!(cache.get(hot).is_some(), "hot block {hot} evicted");
            }
            cache.insert(cold, block(0));
        }
    }

    #[test]
    fn remove_keeps_index_consistent() {
        let mut cache = BlockCache::new(16);
        for b in 0..16 {
            cache.insert(b, block(b as u8));
        }
        cache.remove(0);
        cache.remove(15);
        cache.remove(7);
        for b in [0, 7, 15] {
            assert!(cache.get(b).is_none());
        }
        for b in (1..15).filter(|&b| b != 7) {
            assert_eq!(cache.get(b).unwrap()[0], b as u8);
        }
        cache.insert(7, block(70));
        assert_eq!(cache.get(7).unwrap()[0], 70);
    }
}
