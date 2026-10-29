//! Extent mappings for kernel-offloaded I/O (FSKit KOIO).
//!
//! With KOIO the kernel moves file data itself; the module only tells it
//! where the data lives. A read asks for a mapping and changes nothing. A
//! write asks for a mapping that it may write through, then reports when the
//! data is on disk.
//!
//! Blocks that a write needs are allocated as *unwritten* extents and turn
//! into written extents only when the kernel reports the write complete, so
//! the data is on disk before any committed metadata points at it as file
//! content (ext4 ordered mode). A crash in between leaves blocks that read
//! as zeros, never stale disk content, and unwritten extents past the end
//! of the file are valid ext4 (e2fsck accepts them).
//!
//! Every regular file takes part on a writable volume: macOS's FSKit hangs
//! on large writes to files that opt out on a volume that supports this
//! mode, so opting out is not an option for writable files.
//! - Inline-data files move their data into blocks on their first mapping.
//! - Block-mapped (ext2/ext3) files cannot mark blocks unwritten. Their new
//!   blocks are zeroed before they are mapped when the volume has a journal,
//!   so a crash cannot expose old disk content through them either.
//!
//! On a read-only volume, only inline-data files stay out (their data is
//! not in blocks); they are read through the module.

use crate::blockmap::BLOCKMAP_LIMIT;
use crate::bmap::Mapping;
use crate::error::{Error, Result};
use crate::extent::{normalize, Extent, MAX_UNWRITTEN_LEN};
use crate::inode::{
    FileType, Inode, Timestamp, FL_APPEND, FL_EXTENTS, FL_IMMUTABLE, FL_INLINE_DATA,
};
use crate::superblock::ro_compat;
use crate::volume::Volume;

/// What a range of a file is backed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoKind {
    /// Initialized data at `physical`.
    Data,
    /// Not allocated: reads as zeros. `physical` is 0.
    Hole,
    /// Allocated but not written yet: reads as zeros; a write may go to
    /// `physical`, and `complete_kernel_write` then marks it written.
    Unwritten,
}

/// One mapped range, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoRun {
    /// Offset in the file.
    pub logical: u64,
    /// Offset on the device (0 for holes).
    pub physical: u64,
    pub len: u64,
    pub kind: IoKind,
}

/// True if the kernel may do this inode's data I/O on a volume that is
/// read-only or not.
pub fn kernel_io_eligible(inode: &Inode, read_only: bool) -> bool {
    inode.file_type() == FileType::Regular && !(read_only && inode.has_flag(FL_INLINE_DATA))
}

impl Volume {
    fn kernel_io_inode(&self, ino: u32) -> Result<Inode> {
        let inode = self.read_inode(ino)?;
        if inode.mode() == 0 {
            return Err(Error::NotFound);
        }
        if inode.file_type() != FileType::Regular {
            return Err(Error::invalid("kernel-offloaded I/O needs a regular file"));
        }
        Ok(inode)
    }

    /// The block range [first, end) covering bytes [offset, offset + len),
    /// and the first run's offset inside its block.
    fn block_span(&self, offset: u64, len: u64) -> Result<(u64, u64, u64)> {
        let bs = self.block_size as u64;
        let end = offset.checked_add(len).ok_or(Error::FileTooLarge)?;
        Ok((offset / bs, end.div_ceil(bs), offset % bs))
    }

    /// Map bytes [offset, offset + len) of `ino` for reading, in at most
    /// `max_runs` runs (the caller asks again for the rest). Changes nothing.
    /// Inline-data files have no blocks: see `move_inline_data_out`.
    pub fn map_for_read(
        &self,
        ino: u32,
        offset: u64,
        len: u64,
        max_runs: usize,
    ) -> Result<Vec<IoRun>> {
        let inode = self.kernel_io_inode(ino)?;
        if inode.has_flag(FL_INLINE_DATA) {
            return Err(Error::invalid("inline-data file has no blocks to map"));
        }
        self.map_range(ino, &inode, offset, len, max_runs)
    }

    /// True if `ino` keeps its data in the inode (inline data).
    pub fn has_inline_data(&self, ino: u32) -> Result<bool> {
        Ok(self.read_inode(ino)?.has_flag(FL_INLINE_DATA))
    }

    /// Move an inline-data file's data into blocks so it can be mapped.
    pub fn move_inline_data_out(&mut self, ino: u32) -> Result<()> {
        self.ensure_writable()?;
        let mut inode = self.kernel_io_inode(ino)?;
        if inode.has_flag(FL_INLINE_DATA) {
            self.uninline(ino, &mut inode)?;
            self.write_inode(ino, &inode)?;
        }
        Ok(())
    }

    fn map_range(
        &self,
        ino: u32,
        inode: &Inode,
        offset: u64,
        len: u64,
        max_runs: usize,
    ) -> Result<Vec<IoRun>> {
        let bs = self.block_size as u64;
        let (mut lblk, end_blk, mut skip) = self.block_span(offset, len)?;
        let mut runs = Vec::new();
        while lblk < end_blk && runs.len() < max_runs {
            let m = self.map_blocks(ino, inode, lblk, (end_blk - lblk).min(u32::MAX as u64))?;
            let (kind, physical) = match m {
                Mapping::Data { physical, .. } => (IoKind::Data, physical * bs + skip),
                Mapping::Unwritten { physical, .. } => (IoKind::Unwritten, physical * bs + skip),
                Mapping::Hole(_) => (IoKind::Hole, 0),
            };
            runs.push(IoRun {
                logical: lblk * bs + skip,
                physical,
                len: m.len() * bs - skip,
                kind,
            });
            lblk += m.len();
            skip = 0;
        }
        Ok(runs)
    }

    /// Map bytes [offset, offset + len) of `ino` for writing, in at most
    /// `max_runs` runs. Holes are allocated as unwritten extents; the size
    /// does not change until `complete_kernel_write`. If the volume fills
    /// up after some runs are mapped, those runs are returned (the kernel
    /// writes less); if nothing could be mapped, the error is returned.
    pub fn map_for_write(
        &mut self,
        ino: u32,
        offset: u64,
        len: u64,
        max_runs: usize,
    ) -> Result<Vec<IoRun>> {
        self.ensure_writable()?;
        let mut inode = self.kernel_io_inode(ino)?;
        if inode.has_flag(FL_IMMUTABLE) || (inode.has_flag(FL_APPEND) && offset < inode.size()) {
            return Err(Error::PermissionDenied);
        }
        let end = offset.checked_add(len).ok_or(Error::FileTooLarge)?;
        if end > self.max_file_size(&inode) {
            return Err(Error::FileTooLarge);
        }
        let bs = self.block_size as u64;
        let (first, end_blk, _) = self.block_span(offset, len)?;
        let mut lblk = first;
        let mut allocated = false;
        if inode.has_flag(FL_INLINE_DATA) {
            self.uninline(ino, &mut inode)?;
            allocated = true;
        }
        let extents = inode.has_flag(FL_EXTENTS);
        let mut failure = None;
        // Allocate the holes: unwritten extents, or zeroed blocks in a
        // block map.
        while lblk < end_blk {
            let m = self.map_blocks(ino, &inode, lblk, end_blk - lblk)?;
            if let Mapping::Hole(hole) = m {
                let got = if extents {
                    self.alloc_unwritten(ino, &mut inode, lblk, hole)
                } else {
                    self.alloc_blockmap_zeroed(ino, &mut inode, lblk, hole)
                };
                match got {
                    Ok(got) => {
                        allocated = true;
                        lblk += got;
                        continue;
                    }
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            lblk += m.len();
        }
        if allocated {
            self.write_inode(ino, &inode)?;
        }
        let mapped_end = lblk.min(end_blk) * bs;
        if mapped_end <= offset {
            return Err(failure.unwrap_or(Error::NoSpace));
        }
        let mut runs = self.map_range(ino, &inode, offset, mapped_end - offset, max_runs)?;
        runs.retain(|r| r.kind != IoKind::Hole);
        Ok(runs)
    }

    /// Allocate up to `want` blocks at `lblk` in a block-mapped file, below
    /// 2^32 (block maps hold 32-bit block numbers). On a journaled volume
    /// the blocks are zeroed first: unlike extents, block maps cannot mark
    /// blocks unwritten, and the new mapping can be committed before the
    /// kernel's data reaches the disk. Returns the number of blocks.
    fn alloc_blockmap_zeroed(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        lblk: u64,
        want: u64,
    ) -> Result<u64> {
        let prev = match lblk.checked_sub(1) {
            Some(p) => self.blockmap_lookup(inode, p)?,
            None => 0,
        };
        let goal = if prev != 0 {
            prev + 1
        } else {
            self.inode_goal_block(ino)
        };
        let (phys, got) = self.alloc_blocks_below(goal, want.min(1024), BLOCKMAP_LIMIT)?;
        if self.journal.is_some() {
            let zeros = vec![0u8; got as usize * self.block_size];
            self.write_data(phys, &zeros)?;
        }
        let mut meta = 0;
        for i in 0..got {
            meta += self.blockmap_set(inode, lblk + i, phys + i)?;
        }
        self.add_inode_blocks(inode, got + meta)?;
        Ok(got)
    }

    /// Allocate up to `want` blocks at `lblk` as one unwritten extent.
    /// Returns the number of blocks allocated.
    fn alloc_unwritten(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        lblk: u64,
        want: u64,
    ) -> Result<u64> {
        let want = want.min(MAX_UNWRITTEN_LEN as u64);
        // Fast path: past the last extent, add the run to the rightmost leaf.
        let last = self.last_extent(inode, ino)?;
        if last.is_none_or(|l| lblk >= l.end()) {
            let goal = last.map_or_else(
                || self.inode_goal_block(ino),
                |p| p.physical + (lblk - p.logical as u64),
            );
            let (phys, got) = self.alloc_blocks(goal, want)?;
            self.add_inode_blocks(inode, got)?;
            let run = Extent {
                logical: lblk as u32,
                physical: phys,
                len: got as u32,
                unwritten: true,
            };
            if !self.extent_append(ino, inode, run, phys + got)? {
                let (mut extents, nodes) = self.read_extents(inode, ino)?;
                extents.push(run);
                normalize(&mut extents);
                self.store_extents(ino, inode, &extents, &nodes, phys + got)?;
            }
            return Ok(got);
        }
        let (mut extents, nodes) = self.read_extents(inode, ino)?;
        let at = extents.partition_point(|e| e.end() <= lblk);
        let goal = if at > 0 {
            let p = extents[at - 1];
            p.physical + (lblk - p.logical as u64)
        } else {
            self.inode_goal_block(ino)
        };
        let (phys, got) = self.alloc_blocks(goal, want)?;
        self.add_inode_blocks(inode, got)?;
        extents.insert(
            at,
            Extent {
                logical: lblk as u32,
                physical: phys,
                len: got as u32,
                unwritten: true,
            },
        );
        normalize(&mut extents);
        self.store_extents(ino, inode, &extents, &nodes, phys + got)?;
        Ok(got)
    }

    /// The kernel wrote bytes [offset, offset + len) of `ino`: mark the
    /// blocks they touch as written, grow the size to cover them, and
    /// update the modification time.
    pub fn complete_kernel_write(&mut self, ino: u32, offset: u64, len: u64) -> Result<()> {
        self.ensure_writable()?;
        let mut inode = self.kernel_io_inode(ino)?;
        let (first, end_blk, _) = self.block_span(offset, len)?;
        let end = offset + len;
        if end > self.max_file_size(&inode) {
            return Err(Error::FileTooLarge);
        }
        if self.range_has_unwritten(ino, &inode, first, end_blk)? {
            // Usually a sequential write: only the last extent is unwritten.
            let tail_only = match self.last_extent(&inode, ino)? {
                Some(l) if l.unwritten => {
                    !self.range_has_unwritten(ino, &inode, first, (l.logical as u64).max(first))?
                }
                _ => false,
            };
            if !(tail_only && self.extent_tail_written(ino, &mut inode, first, end_blk)?) {
                self.mark_written(ino, &mut inode, first, end_blk)?;
            }
        }
        if end > inode.size() {
            inode.set_size(end);
            if end > 0x7FFF_FFFF && !self.sb.has_ro_compat(ro_compat::LARGE_FILE) {
                self.sb
                    .set_feature_ro_compat(self.sb.feature_ro_compat() | ro_compat::LARGE_FILE);
                self.mark_sb_dirty();
            }
        }
        let now = Timestamp::now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.write_inode(ino, &inode)
    }

    fn range_has_unwritten(
        &self,
        ino: u32,
        inode: &Inode,
        mut lblk: u64,
        end_blk: u64,
    ) -> Result<bool> {
        while lblk < end_blk {
            let m = self.map_blocks(ino, inode, lblk, end_blk - lblk)?;
            if matches!(m, Mapping::Unwritten { .. }) {
                return Ok(true);
            }
            lblk += m.len();
        }
        Ok(false)
    }

    /// Turn the unwritten parts of blocks [first, end_blk) into written
    /// extents, splitting extents at the range edges.
    fn mark_written(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        first: u64,
        end_blk: u64,
    ) -> Result<()> {
        let (extents, nodes) = self.read_extents(inode, ino)?;
        let mut out = Vec::with_capacity(extents.len() + 2);
        for e in extents {
            let (start, stop) = (e.logical as u64, e.end());
            if !e.unwritten || stop <= first || start >= end_blk {
                out.push(e);
                continue;
            }
            let cut_lo = start.max(first);
            let cut_hi = stop.min(end_blk);
            let piece = |from: u64, to: u64, unwritten: bool| Extent {
                logical: from as u32,
                physical: e.physical + (from - start),
                len: (to - from) as u32,
                unwritten,
            };
            if cut_lo > start {
                out.push(piece(start, cut_lo, true));
            }
            out.push(piece(cut_lo, cut_hi, false));
            if stop > cut_hi {
                out.push(piece(cut_hi, stop, true));
            }
        }
        normalize(&mut out);
        let goal = out
            .last()
            .map_or_else(|| self.inode_goal_block(ino), |e| e.physical + e.len as u64);
        self.store_extents(ino, inode, &out, &nodes, goal)
    }
}
