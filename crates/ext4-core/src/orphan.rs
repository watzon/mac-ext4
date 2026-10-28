//! Orphan inodes: files whose last link is gone but which are still in use,
//! or which were mid-truncate at a crash. They are chained through `i_dtime`
//! from `s_last_orphan`, and (with `orphan_file`) listed in the orphan file.

use std::collections::HashSet;

use crate::bytes::*;
use crate::error::{Error, Result};
use crate::extent::init_extent_root;
use crate::file::has_block_map;
use crate::inode::{Inode, Timestamp, FL_EXTENTS};
use crate::superblock::{compat, ro_compat};
use crate::volume::Volume;

const ORPHAN_BLOCK_MAGIC: u32 = 0x0B10_CA04;
/// Size of the orphan file block tail (`ob_magic`, `ob_checksum`).
const ORPHAN_TAIL: usize = 8;
/// An unlinked orphan is truncated in steps of this many bytes, one
/// operation each, before it is freed. The same step bounds size changes in
/// `Filesystem::setattr`.
const ORPHAN_TRUNCATE_STEP: u64 = 1 << 34;

/// One used entry of the orphan file.
struct OrphanSlot {
    ino: u32,
    /// Physical block of the orphan file that holds the entry.
    physical: u64,
    /// Entry index inside that block.
    index: usize,
}

impl Volume {
    /// Put `ino` at the head of the orphan chain. The caller writes `inode`.
    pub(crate) fn add_orphan(&mut self, ino: u32, inode: &mut Inode) {
        if self.orphans.contains(&ino) {
            return;
        }
        inode.set_dtime(self.sb.last_orphan());
        self.sb.set_last_orphan(ino);
        self.mark_sb_dirty();
        self.orphans.insert(0, ino);
    }

    /// Unlink `ino` from the orphan chain.
    pub(crate) fn remove_orphan(&mut self, ino: u32) -> Result<()> {
        let Some(pos) = self.orphans.iter().position(|&o| o == ino) else {
            return Ok(());
        };
        let next = self.orphans.get(pos + 1).copied().unwrap_or(0);
        if pos == 0 {
            self.sb.set_last_orphan(next);
            self.mark_sb_dirty();
        } else {
            let prev = self.orphans[pos - 1];
            let mut p = self.read_inode(prev)?;
            p.set_dtime(next);
            self.write_inode(prev, &p)?;
        }
        self.orphans.remove(pos);
        Ok(())
    }

    /// Release all blocks of an unlinked inode and free it.
    pub(crate) fn free_inode_fully(&mut self, ino: u32, mut inode: Inode) -> Result<()> {
        let is_dir = inode.is_dir();
        if has_block_map(self, &inode) {
            self.truncate(ino, &mut inode, 0)?;
        }
        if inode.file_acl() != 0 {
            self.release_xattr_block(inode.file_acl())?;
            inode.set_file_acl(0);
        }
        inode.set_links_count(0);
        inode.set_size(0);
        inode.set_blocks_512(&self.sb, 0)?;
        let now = Timestamp::now();
        inode.set_dtime(now.secs as u32);
        if inode.has_flag(FL_EXTENTS) {
            init_extent_root(&mut inode);
        }
        self.write_inode(ino, &inode)?;
        self.free_inode(ino, is_dir)
    }

    /// Free `ino` if it is an orphan with no links (the host dropped it).
    pub(crate) fn reclaim(&mut self, ino: u32) -> Result<()> {
        if self.read_only || !self.orphans.contains(&ino) {
            return Ok(());
        }
        let inode = self.read_inode(ino)?;
        if inode.links_count() != 0 {
            return Ok(());
        }
        self.remove_orphan(ino)?;
        let mut inode = self.read_inode(ino)?;
        inode.set_dtime(0);
        self.free_inode_fully(ino, inode)
    }

    /// Free every in-memory orphan (at unmount).
    pub(crate) fn release_orphans(&mut self) -> Result<()> {
        while let Some(&ino) = self.orphans.first() {
            self.begin_op();
            let r = (|| {
                let inode = self.read_inode(ino)?;
                self.remove_orphan(ino)?;
                if inode.links_count() == 0 {
                    let mut inode = self.read_inode(ino)?;
                    inode.set_dtime(0);
                    self.free_inode_fully(ino, inode)?;
                }
                Ok(())
            })();
            self.end_op(r)?;
        }
        Ok(())
    }

    /// Mount-time cleanup of the orphan chain and orphan file.
    ///
    /// Each orphan is handled as its own operation that also removes it
    /// from the chain or the orphan file, so the normal commit threshold can
    /// commit between orphans. A crash part way through leaves the
    /// unprocessed orphans recorded for the next mount.
    pub(crate) fn process_orphans(&mut self) -> Result<()> {
        let mut seen = HashSet::new();
        let mut next = self.sb.last_orphan();
        let mut chain = Vec::new();
        while next != 0 {
            if !seen.insert(next) || next > self.sb.inodes_count() || next < self.sb.first_ino() {
                // Broken chain: stop rather than loop or touch reserved inodes.
                break;
            }
            let inode = self.read_inode(next)?;
            chain.push(next);
            next = inode.dtime();
        }
        let slots = self.orphan_file_slots()?;
        if chain.is_empty()
            && slots.is_empty()
            && self.sb.last_orphan() == 0
            && !self.sb.has_ro_compat(ro_compat::ORPHAN_PRESENT)
        {
            return Ok(());
        }

        let mut count = 0u32;
        for (i, &ino) in chain.iter().enumerate() {
            // Chain entries are processed head first, so unlinking one only
            // moves `s_last_orphan` to the next.
            let next = chain.get(i + 1).copied().unwrap_or(0);
            self.cleanup_orphan(ino, |v| {
                v.sb.set_last_orphan(next);
                v.mark_sb_dirty();
                Ok(())
            })?;
            count += 1;
        }
        for slot in slots {
            if seen.insert(slot.ino) {
                self.cleanup_orphan(slot.ino, |v| v.clear_orphan_slot(&slot))?;
                count += 1;
            } else {
                // Also listed in the chain and already handled.
                self.begin_op();
                let r = self.clear_orphan_slot(&slot);
                self.end_op(r)?;
            }
        }

        self.begin_op();
        self.sb.set_last_orphan(0);
        self.sb
            .set_feature_ro_compat(self.sb.feature_ro_compat() & !ro_compat::ORPHAN_PRESENT);
        self.mark_sb_dirty();
        self.end_op(Ok(()))?;
        self.recovery.orphans_processed = count;
        self.commit()
    }

    /// Finish orphan `ino`: free it when it has no links, otherwise drop the
    /// blocks past `i_size` that an interrupted truncate left. `detach`
    /// removes it from the on-disk orphan record in the same operation.
    fn cleanup_orphan(
        &mut self,
        ino: u32,
        detach: impl FnOnce(&mut Volume) -> Result<()>,
    ) -> Result<()> {
        let inode = self.read_inode(ino)?;
        if inode.links_count() == 0 && has_block_map(self, &inode) {
            // Free a large file in bounded steps so no single operation
            // outgrows the journal. The inode stays an orphan meanwhile.
            let mut size = inode.size();
            while size > ORPHAN_TRUNCATE_STEP {
                size -= ORPHAN_TRUNCATE_STEP;
                self.begin_op();
                let r = (|| {
                    let mut inode = self.read_inode(ino)?;
                    self.truncate(ino, &mut inode, size)?;
                    self.write_inode(ino, &inode)
                })();
                self.end_op(r)?;
            }
        }
        self.begin_op();
        let r = (|| {
            let mut inode = self.read_inode(ino)?;
            inode.set_dtime(0);
            if inode.links_count() == 0 {
                self.free_inode_fully(ino, inode)?;
            } else {
                // A truncate that did not finish: drop blocks past i_size.
                let size = inode.size();
                if has_block_map(self, &inode) {
                    inode.set_size(u64::MAX >> 1);
                    self.truncate(ino, &mut inode, size)?;
                }
                self.write_inode(ino, &inode)?;
            }
            detach(self)
        })();
        self.end_op(r)
    }

    fn orphan_file(&self) -> Result<Option<(u32, Inode)>> {
        if !self.sb.has_compat(compat::ORPHAN_FILE) {
            return Ok(None);
        }
        let ino = self.sb.orphan_file_inum();
        if ino == 0 {
            return Ok(None);
        }
        Ok(Some((ino, self.read_inode(ino)?)))
    }

    /// Checksum of orphan file block `physical`, as Linux
    /// `ext4_orphan_file_block_csum_verify` computes it: the orphan file's
    /// inode seed, then the physical block number as le64, then the entries.
    fn orphan_block_checksum(&self, seed: u32, physical: u64, block: &[u8]) -> u32 {
        let c = crate::crc::crc32c(seed, &physical.to_le_bytes());
        crate::crc::crc32c(c, &block[..self.block_size - ORPHAN_TAIL])
    }

    /// Used entries of the orphan file. Every block's magic and checksum is
    /// checked before any entry is trusted.
    fn orphan_file_slots(&self) -> Result<Vec<OrphanSlot>> {
        let Some((ino, inode)) = self.orphan_file()? else {
            return Ok(Vec::new());
        };
        let bs = self.block_size;
        let seed = Inode::csum_seed(&self.sb, ino, inode.generation());
        let mut out = Vec::new();
        for lblk in 0..inode.size().div_ceil(bs as u64) {
            let crate::bmap::Mapping::Data { physical, .. } =
                self.map_blocks(ino, &inode, lblk, 1)?
            else {
                continue;
            };
            let b = self.read_block(physical)?;
            if le32(&b, bs - ORPHAN_TAIL) != ORPHAN_BLOCK_MAGIC {
                return Err(Error::corrupt(format!(
                    "orphan file block {lblk} has a bad magic"
                )));
            }
            if self.sb.has_metadata_csum()
                && !self.ignore_checksums
                && le32(&b, bs - 4) != self.orphan_block_checksum(seed, physical, &b)
            {
                return Err(Error::corrupt(format!(
                    "orphan file block {lblk} checksum mismatch"
                )));
            }
            for index in 0..(bs - ORPHAN_TAIL) / 4 {
                let ino = le32(&b, index * 4);
                if ino != 0 {
                    out.push(OrphanSlot {
                        ino,
                        physical,
                        index,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Zero one orphan file entry and refresh its block checksum.
    fn clear_orphan_slot(&mut self, slot: &OrphanSlot) -> Result<()> {
        let Some((ino, inode)) = self.orphan_file()? else {
            return Ok(());
        };
        let bs = self.block_size;
        let seed = Inode::csum_seed(&self.sb, ino, inode.generation());
        let csum = self.sb.has_metadata_csum();
        let buf = self.block_mut(slot.physical)?;
        put32(buf, slot.index * 4, 0);
        if csum {
            let c = crate::crc::crc32c(seed, &slot.physical.to_le_bytes());
            let c = crate::crc::crc32c(c, &buf[..bs - ORPHAN_TAIL]);
            put32(buf, bs - 4, c);
        }
        Ok(())
    }
}
