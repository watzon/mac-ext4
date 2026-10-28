//! Block and inode allocation through the group bitmaps.
//!
//! Writing is refused on `bigalloc` volumes (see `superblock::WRITE_RO_COMPAT`),
//! so one bitmap bit is one block here.

use crate::bytes::*;
use crate::error::{Error, Result};
use crate::group::{
    bitmap_checksum, bitmap_csum_matches, BG_BLOCK_UNINIT, BG_INODE_UNINIT, BG_INODE_ZEROED,
};
use crate::inode::Inode;
use crate::superblock::{incompat, ro_compat};
use crate::volume::Volume;

impl Volume {
    fn block_bitmap_bytes(&self) -> usize {
        (self.sb.clusters_per_group() / 8) as usize
    }

    fn inode_bitmap_bytes(&self) -> usize {
        (self.sb.inodes_per_group() / 8) as usize
    }

    /// Blocks of group `g` used by its superblock backup, GDT, and reserved
    /// GDT blocks (relative to the group start).
    fn group_sb_overhead(&self, g: u32) -> u64 {
        let has_super = self.sb.group_has_super(g);
        let mut n = if has_super { 1 } else { 0 };
        if self.sb.has_incompat(incompat::META_BG) {
            let dpb = self.descs_per_block();
            let meta_group = g / dpb;
            let first_meta = self.sb.first_meta_bg();
            if meta_group < first_meta {
                if has_super {
                    n += first_meta as u64 + self.sb.reserved_gdt_blocks() as u64;
                }
            } else {
                let idx = g % dpb;
                if idx == 0 || idx == 1 || idx == dpb - 1 {
                    n += 1;
                }
            }
        } else if has_super {
            n += self.gdt_blocks() as u64 + self.sb.reserved_gdt_blocks() as u64;
        }
        n
    }

    /// Compute the initial block bitmap of a `BLOCK_UNINIT` group.
    fn init_block_bitmap(&self, g: u32) -> Vec<u8> {
        let mut bm = vec![0u8; self.block_size];
        let start = self.group_first_block(g);
        let in_group = self.blocks_in_group(g);
        let overhead = self.group_sb_overhead(g);
        for i in 0..overhead.min(in_group) {
            set_bit(&mut bm, i as usize);
        }
        // Any group's bitmaps and inode table that live inside this group.
        let itable_blocks = (self.sb.inodes_per_group() as u64 * self.sb.inode_size() as u64)
            .div_ceil(self.block_size as u64);
        let end = start + in_group;
        let mark = |blk: u64, len: u64, bm: &mut Vec<u8>| {
            for b in blk..blk + len {
                if b >= start && b < end {
                    set_bit(bm, (b - start) as usize);
                }
            }
        };
        for gd in &self.groups {
            mark(gd.block_bitmap(), 1, &mut bm);
            mark(gd.inode_bitmap(), 1, &mut bm);
            mark(gd.inode_table(), itable_blocks, &mut bm);
        }
        // Bits past the end of the group are set.
        for i in in_group as usize..self.block_size * 8 {
            set_bit(&mut bm, i);
        }
        bm
    }

    /// The block bitmap of group `g` (computed for uninitialized groups).
    pub(crate) fn read_block_bitmap(&self, g: u32) -> Result<Vec<u8>> {
        let gd = &self.groups[g as usize];
        if self.sb.has_group_csum() && gd.has_flag(BG_BLOCK_UNINIT) {
            return Ok(self.init_block_bitmap(g));
        }
        let b = self.read_block(gd.block_bitmap())?;
        if self.sb.has_metadata_csum() && !self.ignore_checksums {
            let c = bitmap_checksum(&self.sb, &b[..self.block_bitmap_bytes()]);
            if !bitmap_csum_matches(gd.block_bitmap_csum(), c, self.sb.desc_size() as usize) {
                return Err(Error::corrupt(format!(
                    "block bitmap of group {g} checksum mismatch"
                )));
            }
        }
        Ok((*b).clone())
    }

    pub(crate) fn read_inode_bitmap(&self, g: u32) -> Result<Vec<u8>> {
        let gd = &self.groups[g as usize];
        if self.sb.has_group_csum() && gd.has_flag(BG_INODE_UNINIT) {
            let mut bm = vec![0u8; self.block_size];
            for i in self.sb.inodes_per_group() as usize..self.block_size * 8 {
                set_bit(&mut bm, i);
            }
            return Ok(bm);
        }
        let b = self.read_block(gd.inode_bitmap())?;
        if self.sb.has_metadata_csum() && !self.ignore_checksums {
            let c = bitmap_checksum(&self.sb, &b[..self.inode_bitmap_bytes()]);
            if !bitmap_csum_matches(gd.inode_bitmap_csum(), c, self.sb.desc_size() as usize) {
                return Err(Error::corrupt(format!(
                    "inode bitmap of group {g} checksum mismatch"
                )));
            }
        }
        Ok((*b).clone())
    }

    fn write_block_bitmap(&mut self, g: u32, bm: &[u8]) -> Result<()> {
        let csum = self
            .sb
            .has_metadata_csum()
            .then(|| bitmap_checksum(&self.sb, &bm[..self.block_bitmap_bytes()]));
        let loc = self.groups[g as usize].block_bitmap();
        let gd = self.group_mut(g);
        gd.set_flags(gd.flags() & !BG_BLOCK_UNINIT);
        if let Some(c) = csum {
            gd.set_block_bitmap_csum(c);
        }
        self.block_mut(loc)?.copy_from_slice(bm);
        Ok(())
    }

    fn write_inode_bitmap(&mut self, g: u32, bm: &[u8]) -> Result<()> {
        let csum = self
            .sb
            .has_metadata_csum()
            .then(|| bitmap_checksum(&self.sb, &bm[..self.inode_bitmap_bytes()]));
        let loc = self.groups[g as usize].inode_bitmap();
        let gd = self.group_mut(g);
        gd.set_flags(gd.flags() & !BG_INODE_UNINIT);
        if let Some(c) = csum {
            gd.set_inode_bitmap_csum(c);
        }
        self.block_mut(loc)?.copy_from_slice(bm);
        Ok(())
    }

    fn freed_recently(&self, blk: u64) -> bool {
        self.freed_in_txn
            .iter()
            .any(|&(s, n)| blk >= s && blk < s + n)
    }

    /// Allocate one block near `goal`.
    pub(crate) fn alloc_block(&mut self, goal: u64) -> Result<u64> {
        Ok(self.alloc_blocks(goal, 1)?.0)
    }

    /// Allocate up to `want` contiguous blocks, starting the search at
    /// `goal`. Returns `(first, count)` with `1 <= count <= want`.
    pub(crate) fn alloc_blocks(&mut self, goal: u64, want: u64) -> Result<(u64, u64)> {
        self.alloc_blocks_below(goal, want, u64::MAX)
    }

    /// Like [`Volume::alloc_blocks`], but every returned block is below
    /// `limit`.  Block-mapped files store 32-bit block numbers, so their
    /// data and indirect blocks come from below 2^32.
    pub(crate) fn alloc_blocks_below(
        &mut self,
        goal: u64,
        want: u64,
        limit: u64,
    ) -> Result<(u64, u64)> {
        self.ensure_writable()?;
        if self.sb.has_ro_compat(ro_compat::BIGALLOC) {
            return Err(Error::ReadOnly);
        }
        let want = want.max(1);
        let first_data = self.sb.first_data_block() as u64;
        let total = self.sb.blocks_count().min(limit);
        if total <= first_data {
            return Err(Error::NoSpace);
        }
        let goal = if goal < first_data || goal >= total {
            first_data
        } else {
            goal
        };
        // Only groups starting below the limit hold eligible blocks.
        let groups = self.group_of_block(total - 1) + 1;
        let start_group = self.group_of_block(goal);
        for step in 0..=groups {
            let g = (start_group + step) % groups;
            if self.groups[g as usize].free_blocks_count() == 0 {
                continue;
            }
            let bm = self.read_block_bitmap(g)?;
            let base = self.group_first_block(g);
            let limit = self.blocks_in_group(g).min(total - base) as usize;
            let mut from = if step == 0 { (goal - base) as usize } else { 0 };
            // On the wrap-around pass, search the goal group's lower part.
            if step == groups {
                from = 0;
            }
            while let Some(bit) = find_zero_bit(&bm, from, limit) {
                let blk = base + bit as u64;
                if self.freed_recently(blk) {
                    from = bit + 1;
                    continue;
                }
                let mut len = 1u64;
                while len < want
                    && bit + (len as usize) < limit
                    && !test_bit(&bm, bit + len as usize)
                    && !self.freed_recently(blk + len)
                {
                    len += 1;
                }
                self.mark_blocks(g, bm, bit, len as usize, true)?;
                return Ok((blk, len));
            }
        }
        Err(Error::NoSpace)
    }

    fn mark_blocks(
        &mut self,
        g: u32,
        mut bm: Vec<u8>,
        bit: usize,
        len: usize,
        used: bool,
    ) -> Result<()> {
        for i in bit..bit + len {
            if test_bit(&bm, i) == used {
                return Err(Error::corrupt(format!(
                    "block {} is already {} in the bitmap",
                    self.group_first_block(g) + i as u64,
                    if used { "allocated" } else { "free" }
                )));
            }
            if used {
                set_bit(&mut bm, i);
            } else {
                clear_bit(&mut bm, i);
            }
        }
        self.write_block_bitmap(g, &bm)?;
        let gd = self.group_mut(g);
        let free = gd.free_blocks_count();
        gd.set_free_blocks_count(if used {
            free - len as u32
        } else {
            free + len as u32
        });
        let sbfree = self.sb.free_blocks_count();
        self.sb.set_free_blocks_count(if used {
            sbfree.saturating_sub(len as u64)
        } else {
            sbfree + len as u64
        });
        self.mark_sb_dirty();
        Ok(())
    }

    /// Free `count` blocks starting at `start` (may span groups).
    pub(crate) fn free_blocks(&mut self, start: u64, count: u64) -> Result<()> {
        self.ensure_writable()?;
        let first_data = self.sb.first_data_block() as u64;
        if start < first_data || start + count > self.sb.blocks_count() {
            return Err(Error::corrupt(format!(
                "freeing blocks {start}+{count} outside the file system"
            )));
        }
        let mut blk = start;
        let end = start + count;
        while blk < end {
            let g = self.group_of_block(blk);
            let base = self.group_first_block(g);
            let group_end = base + self.blocks_in_group(g);
            let n = end.min(group_end) - blk;
            let bm = self.read_block_bitmap(g)?;
            self.mark_blocks(g, bm, (blk - base) as usize, n as usize, false)?;
            blk += n;
        }
        self.freed_in_txn.push((start, count));
        Ok(())
    }

    /// Add `blocks` filesystem blocks to `i_blocks`.  Fails with
    /// [`Error::FileTooLarge`] when the count no longer fits.
    pub(crate) fn add_inode_blocks(&self, inode: &mut Inode, blocks: u64) -> Result<()> {
        let sectors = inode
            .blocks_512(&self.sb)
            .checked_add(blocks * (self.block_size as u64 / 512))
            .ok_or(Error::FileTooLarge)?;
        inode.set_blocks_512(&self.sb, sectors)
    }

    pub(crate) fn sub_inode_blocks(&self, inode: &mut Inode, blocks: u64) {
        let sectors = inode
            .blocks_512(&self.sb)
            .saturating_sub(blocks * (self.block_size as u64 / 512));
        // The decoded count always fits its own encoding, and a smaller
        // count fits too, so storing it cannot fail.
        let stored = inode.set_blocks_512(&self.sb, sectors);
        debug_assert!(stored.is_ok());
    }

    /// A reasonable first block to allocate near for data of inode `ino`.
    pub(crate) fn inode_goal_block(&self, ino: u32) -> u64 {
        let g = (ino - 1) / self.sb.inodes_per_group();
        self.group_first_block(g)
    }

    /// Allocate an inode. Directories spread across groups; other files
    /// stay near their parent.
    pub(crate) fn alloc_inode(&mut self, parent: u32, is_dir: bool) -> Result<u32> {
        self.ensure_writable()?;
        let groups = self.group_count();
        let ipg = self.sb.inodes_per_group();
        let parent_group = (parent.max(1) - 1) / ipg;
        let order: Vec<u32> = if is_dir {
            // Prefer the group with the most free inodes and above-average free blocks.
            let avg_blocks = self.sb.free_blocks_count() / groups as u64;
            let mut best: Option<(u32, u32)> = None;
            for g in 0..groups {
                let gd = &self.groups[g as usize];
                let fi = gd.free_inodes_count();
                if fi == 0 || (gd.free_blocks_count() as u64) < avg_blocks {
                    continue;
                }
                if best.is_none_or(|(_, b)| fi > b) {
                    best = Some((g, fi));
                }
            }
            let first = best.map_or(parent_group, |(g, _)| g);
            (0..groups).map(|i| (first + i) % groups).collect()
        } else {
            (0..groups).map(|i| (parent_group + i) % groups).collect()
        };
        for g in order {
            if self.groups[g as usize].free_inodes_count() == 0 {
                continue;
            }
            let mut bm = self.read_inode_bitmap(g)?;
            let first_index = if g == 0 {
                self.sb.first_ino() as usize - 1
            } else {
                0
            };
            let Some(bit) = find_zero_bit(&bm, first_index, ipg as usize) else {
                continue;
            };
            let ino = g * ipg + bit as u32 + 1;
            // Inode table slots past the "unused" boundary may hold garbage
            // on lazily initialized groups. Zero the slots we expose.
            let unused = if self.sb.has_group_csum() {
                self.groups[g as usize].itable_unused()
            } else {
                0
            };
            let boundary = ipg - unused;
            if self.sb.has_group_csum() && bit as u32 >= boundary {
                if !self.groups[g as usize].has_flag(BG_INODE_ZEROED) {
                    for idx in boundary..=bit as u32 {
                        let slot = g * ipg + idx + 1;
                        let (blk, off) = self.inode_location(slot)?;
                        let isz = self.sb.inode_size() as usize;
                        self.block_mut(blk)?[off..off + isz].fill(0);
                    }
                }
                self.group_mut(g).set_itable_unused(ipg - bit as u32 - 1);
            }
            set_bit(&mut bm, bit);
            self.write_inode_bitmap(g, &bm)?;
            let gd = self.group_mut(g);
            gd.set_free_inodes_count(gd.free_inodes_count() - 1);
            if is_dir {
                gd.set_used_dirs_count(gd.used_dirs_count() + 1);
            }
            let fi = self.sb.free_inodes_count();
            self.sb.set_free_inodes_count(fi.saturating_sub(1));
            self.mark_sb_dirty();
            return Ok(ino);
        }
        Err(Error::NoSpace)
    }

    pub(crate) fn free_inode(&mut self, ino: u32, is_dir: bool) -> Result<()> {
        let ipg = self.sb.inodes_per_group();
        let g = (ino - 1) / ipg;
        let bit = ((ino - 1) % ipg) as usize;
        let mut bm = self.read_inode_bitmap(g)?;
        if !test_bit(&bm, bit) {
            return Err(Error::corrupt(format!("inode {ino} is already free")));
        }
        clear_bit(&mut bm, bit);
        self.write_inode_bitmap(g, &bm)?;
        let gd = self.group_mut(g);
        gd.set_free_inodes_count(gd.free_inodes_count() + 1);
        if is_dir {
            gd.set_used_dirs_count(gd.used_dirs_count().saturating_sub(1));
        }
        self.sb
            .set_free_inodes_count(self.sb.free_inodes_count() + 1);
        self.mark_sb_dirty();
        Ok(())
    }
}
