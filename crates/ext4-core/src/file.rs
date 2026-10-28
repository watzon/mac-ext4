//! File contents: reading, writing, and truncation.

use crate::bmap::Mapping;
use crate::error::{Error, Result};
use crate::extent::{normalize, Extent};
use crate::inode::{FileType, Inode, FL_EXTENTS, FL_INLINE_DATA};
use crate::superblock::ro_compat;
use crate::volume::Volume;

/// True when `i_block` holds a block map or extent tree (as opposed to a
/// device number, a fast symlink target, or inline data).
pub fn has_block_map(vol: &Volume, inode: &Inode) -> bool {
    if inode.has_flag(FL_INLINE_DATA) {
        return false;
    }
    match inode.file_type() {
        FileType::Regular | FileType::Directory => true,
        FileType::Symlink => !is_fast_symlink(vol, inode),
        _ => false,
    }
}

pub fn is_fast_symlink(vol: &Volume, inode: &Inode) -> bool {
    if inode.file_type() != FileType::Symlink || inode.has_flag(FL_INLINE_DATA) {
        return false;
    }
    if inode.has_flag(crate::inode::FL_EA_INODE) {
        return inode.size() > 0 && inode.size() < 60;
    }
    let ea_sectors = if inode.file_acl() != 0 {
        vol.block_size() as u64 / 512
    } else {
        0
    };
    inode.blocks_512(&vol.sb) == ea_sectors
}

impl Volume {
    /// Largest file size this volume supports for `inode`'s mapping kind.
    ///
    /// This follows Linux `ext4_max_size` and `ext4_max_bitmap_size`: without
    /// `huge_file`, `i_blocks` is a 32-bit count of 512-byte sectors, so the
    /// size is capped at what that count can describe.
    pub fn max_file_size(&self, inode: &Inode) -> u64 {
        let huge = self.sb.has_ro_compat(ro_compat::HUGE_FILE);
        if !inode.has_flag(FL_EXTENTS) {
            return crate::blockmap::max_size(self.block_size, huge);
        }
        let bits = self.block_size.trailing_zeros();
        // The last logical block is left out so an extent can always cover
        // the final block of a maximum-size file.
        let mapped = (u32::MAX as u64) << bits;
        if huge {
            mapped
        } else {
            mapped.min((u32::MAX as u64 >> (bits - 9)) << bits)
        }
    }

    /// Read file bytes at `offset`. Returns the number of bytes read (short
    /// at end of file).
    pub fn read_file(&self, ino: u32, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let size = inode.size();
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        if inode.has_flag(FL_INLINE_DATA) {
            return self.read_inline_file(ino, inode, offset, buf);
        }
        if inode.file_type() == FileType::Symlink && is_fast_symlink(self, inode) {
            let n = ((size - offset) as usize).min(buf.len());
            let start = offset as usize;
            buf[..n].copy_from_slice(&inode.i_block()[start..start + n]);
            return Ok(n);
        }
        let total = ((size - offset) as usize).min(buf.len());
        let bs = self.block_size as u64;
        let mut done = 0usize;
        let mut scratch: Vec<u8> = Vec::new();
        while done < total {
            let pos = offset + done as u64;
            let lblk = pos / bs;
            let in_block = (pos % bs) as usize;
            let blocks_wanted = ((in_block + total - done) as u64).div_ceil(bs);
            let m = self.map_blocks(ino, inode, lblk, blocks_wanted)?;
            let run_bytes = (m.len() * bs) as usize - in_block;
            let n = run_bytes.min(total - done);
            let out = &mut buf[done..done + n];
            match m {
                Mapping::Hole(_) | Mapping::Unwritten { .. } => out.fill(0),
                Mapping::Data { physical, .. } => {
                    if in_block == 0 && n % bs as usize == 0 {
                        self.read_data(physical, out)?;
                    } else {
                        let nblocks = (in_block + n).div_ceil(bs as usize);
                        scratch.resize(nblocks * bs as usize, 0);
                        self.read_data(physical, &mut scratch)?;
                        out.copy_from_slice(&scratch[in_block..in_block + n]);
                    }
                }
            }
            done += n;
        }
        Ok(total)
    }

    /// Write `data` at `offset`, allocating blocks as needed and growing the
    /// file size. Updates the inode in memory; the caller writes it.
    pub(crate) fn write_file(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        self.ensure_writable()?;
        if data.is_empty() {
            return Ok(());
        }
        if inode.has_flag(FL_INLINE_DATA) {
            self.uninline(ino, inode)?;
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(Error::FileTooLarge)?;
        if end > self.max_file_size(inode) {
            return Err(Error::FileTooLarge);
        }
        // A write that starts past EOF grows the file over a gap; the gap
        // inside the old last block must read as zeros.
        let old_size = inode.size();
        if offset > old_size {
            self.zero_block_tail(ino, inode, old_size)?;
        }
        if inode.has_flag(FL_EXTENTS) {
            self.write_extent_file(ino, inode, offset, data)?;
        } else {
            self.write_blockmap_file(inode, ino, offset, data)?;
        }
        if end > inode.size() {
            inode.set_size(end);
            if end > 0x7FFF_FFFF && !self.sb.has_ro_compat(ro_compat::LARGE_FILE) {
                self.sb
                    .set_feature_ro_compat(self.sb.feature_ro_compat() | ro_compat::LARGE_FILE);
                self.mark_sb_dirty();
            }
        }
        Ok(())
    }

    /// Write one block-aligned span of `data` into physical blocks starting
    /// at `physical`, merging partial edge blocks with existing content
    /// (`fresh` = the blocks are new or unwritten, so the rest is zeros).
    fn put_blocks(
        &mut self,
        physical: u64,
        block_off: usize,
        data: &[u8],
        fresh: bool,
    ) -> Result<()> {
        let bs = self.block_size;
        if block_off == 0 && data.len() % bs == 0 {
            return self.write_data(physical, data);
        }
        let nblocks = (block_off + data.len()).div_ceil(bs);
        let mut buf = vec![0u8; nblocks * bs];
        if !fresh {
            // Only the first and last blocks need their old content.
            self.read_data(physical, &mut buf[..bs])?;
            if nblocks > 1 {
                let last = (nblocks - 1) * bs;
                self.read_data(physical + nblocks as u64 - 1, &mut buf[last..])?;
            }
        }
        buf[block_off..block_off + data.len()].copy_from_slice(data);
        self.write_data(physical, &buf)
    }

    fn write_extent_file(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        let bs = self.block_size as u64;
        let mut done = 0usize;
        // Fast paths: overwrite initialized blocks found by a single tree
        // lookup, and append after the last extent by editing only the
        // rightmost tree path.  Anything else goes to the full rebuild.
        while done < data.len() {
            let pos = offset + done as u64;
            let lblk = pos / bs;
            let in_block = (pos % bs) as usize;
            let remaining_blocks = ((in_block + data.len() - done) as u64).div_ceil(bs);
            match self.extent_lookup(inode, ino, lblk as u32)? {
                (Some(e), _) if !e.unwritten => {
                    let skip = lblk - e.logical as u64;
                    let run = (e.len as u64 - skip).min(remaining_blocks);
                    let n = ((run * bs) as usize - in_block).min(data.len() - done);
                    self.put_blocks(e.physical + skip, in_block, &data[done..done + n], false)?;
                    done += n;
                }
                (None, None) => {
                    let goal = match self.last_extent(inode, ino)? {
                        Some(p) => p.physical + lblk.saturating_sub(p.logical as u64),
                        None => self.inode_goal_block(ino),
                    };
                    let want = remaining_blocks.min(crate::extent::MAX_INIT_LEN as u64);
                    let (phys, got) = self.alloc_blocks(goal, want)?;
                    self.add_inode_blocks(inode, got)?;
                    let n = ((got * bs) as usize - in_block).min(data.len() - done);
                    self.put_blocks(phys, in_block, &data[done..done + n], true)?;
                    let run = Extent {
                        logical: lblk as u32,
                        physical: phys,
                        len: got as u32,
                        unwritten: false,
                    };
                    if !self.extent_append(ino, inode, run, phys + got)? {
                        let (mut extents, nodes) = self.read_extents(inode, ino)?;
                        let at = extents.partition_point(|x| x.end() <= lblk);
                        extents.insert(at, run);
                        normalize(&mut extents);
                        self.store_extents(ino, inode, &extents, &nodes, phys + got)?;
                    }
                    done += n;
                }
                _ => break,
            }
        }
        if done < data.len() {
            self.write_extent_rebuild(ino, inode, offset + done as u64, &data[done..])?;
        }
        Ok(())
    }

    /// General extent write: works on the whole extent list and rebuilds
    /// the tree.  Used for unwritten extents and holes inside the file.
    fn write_extent_rebuild(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        let bs = self.block_size as u64;
        let (mut extents, nodes) = self.read_extents(inode, ino)?;
        let mut changed = false;
        let mut done = 0usize;
        while done < data.len() {
            let pos = offset + done as u64;
            let lblk = pos / bs;
            let in_block = (pos % bs) as usize;
            let remaining_blocks = ((in_block + data.len() - done) as u64).div_ceil(bs);
            // Locate lblk in the list.
            let idx = extents.partition_point(|e| e.end() <= lblk);
            let hit = extents
                .get(idx)
                .filter(|e| (e.logical as u64) <= lblk)
                .copied();
            match hit {
                Some(e) if !e.unwritten => {
                    let skip = lblk - e.logical as u64;
                    let run = (e.len as u64 - skip).min(remaining_blocks);
                    let n = ((run * bs) as usize - in_block).min(data.len() - done);
                    self.put_blocks(e.physical + skip, in_block, &data[done..done + n], false)?;
                    done += n;
                }
                Some(e) => {
                    // Unwritten: write, then mark this piece initialized.
                    let skip = lblk - e.logical as u64;
                    let run = (e.len as u64 - skip).min(remaining_blocks);
                    let n = ((run * bs) as usize - in_block).min(data.len() - done);
                    self.put_blocks(e.physical + skip, in_block, &data[done..done + n], true)?;
                    let mut pieces = Vec::with_capacity(3);
                    if skip > 0 {
                        pieces.push(Extent {
                            len: skip as u32,
                            ..e
                        });
                    }
                    pieces.push(Extent {
                        logical: lblk as u32,
                        physical: e.physical + skip,
                        len: run as u32,
                        unwritten: false,
                    });
                    let tail = e.len as u64 - skip - run;
                    if tail > 0 {
                        pieces.push(Extent {
                            logical: (lblk + run) as u32,
                            physical: e.physical + skip + run,
                            len: tail as u32,
                            unwritten: true,
                        });
                    }
                    extents.splice(idx..idx + 1, pieces);
                    changed = true;
                    done += n;
                }
                None => {
                    // Hole up to the next extent.
                    let next_start = extents.get(idx).map_or(u64::MAX, |e| e.logical as u64);
                    let hole = (next_start - lblk).min(remaining_blocks);
                    let goal = if idx > 0 {
                        let p = extents[idx - 1];
                        p.physical + (lblk - p.logical as u64)
                    } else {
                        extents.get(idx).map_or_else(
                            || self.inode_goal_block(ino),
                            |n| n.physical.saturating_sub(n.logical as u64 - lblk),
                        )
                    };
                    let (phys, got) =
                        self.alloc_blocks(goal, hole.min(crate::extent::MAX_INIT_LEN as u64))?;
                    self.add_inode_blocks(inode, got)?;
                    let n = ((got * bs) as usize - in_block).min(data.len() - done);
                    self.put_blocks(phys, in_block, &data[done..done + n], true)?;
                    extents.insert(
                        idx,
                        Extent {
                            logical: lblk as u32,
                            physical: phys,
                            len: got as u32,
                            unwritten: false,
                        },
                    );
                    changed = true;
                    done += n;
                }
            }
        }
        if changed {
            normalize(&mut extents);
            let goal = extents
                .last()
                .map_or_else(|| self.inode_goal_block(ino), |e| e.physical + e.len as u64);
            self.store_extents(ino, inode, &extents, &nodes, goal)?;
        }
        Ok(())
    }

    fn write_blockmap_file(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        let bs = self.block_size as u64;
        let mut done = 0usize;
        while done < data.len() {
            let pos = offset + done as u64;
            let lblk = pos / bs;
            let in_block = (pos % bs) as usize;
            let n = (bs as usize - in_block).min(data.len() - done);
            let mut phys = self.blockmap_lookup(inode, lblk)?;
            let fresh = phys == 0;
            if fresh {
                let goal = if lblk > 0 {
                    match self.blockmap_lookup(inode, lblk - 1)? {
                        0 => self.inode_goal_block(ino),
                        p => p + 1,
                    }
                } else {
                    self.inode_goal_block(ino)
                };
                let (blk, _) = self.alloc_blocks_below(goal, 1, crate::blockmap::BLOCKMAP_LIMIT)?;
                phys = blk;
                let meta = self.blockmap_set(inode, lblk, phys)?;
                self.add_inode_blocks(inode, 1 + meta)?;
            }
            self.put_blocks(phys, in_block, &data[done..done + n], fresh)?;
            done += n;
        }
        Ok(())
    }

    /// Set the file size, freeing blocks past the new end.
    pub(crate) fn truncate(&mut self, ino: u32, inode: &mut Inode, new_size: u64) -> Result<()> {
        self.ensure_writable()?;
        let old = inode.size();
        if new_size > self.max_file_size(inode) {
            return Err(Error::FileTooLarge);
        }
        if inode.has_flag(FL_INLINE_DATA) {
            if new_size > old {
                // Growing an inline file: move it to blocks (sparse tail).
                self.uninline(ino, inode)?;
            } else {
                inode.set_size(new_size);
                return Ok(());
            }
        }
        if !has_block_map(self, inode) {
            inode.set_size(new_size);
            return Ok(());
        }
        let bs = self.block_size as u64;
        if new_size < old {
            let keep = new_size.div_ceil(bs);
            if inode.has_flag(FL_EXTENTS) {
                let (mut extents, nodes) = self.read_extents(inode, ino)?;
                let mut freed = 0u64;
                let mut kept = Vec::with_capacity(extents.len());
                for e in extents.drain(..) {
                    if e.end() <= keep {
                        kept.push(e);
                    } else if (e.logical as u64) < keep {
                        let keep_len = keep - e.logical as u64;
                        self.free_blocks(e.physical + keep_len, e.len as u64 - keep_len)?;
                        freed += e.len as u64 - keep_len;
                        kept.push(Extent {
                            len: keep_len as u32,
                            ..e
                        });
                    } else {
                        self.free_blocks(e.physical, e.len as u64)?;
                        freed += e.len as u64;
                    }
                }
                self.sub_inode_blocks(inode, freed);
                let goal = self.inode_goal_block(ino);
                self.store_extents(ino, inode, &kept, &nodes, goal)?;
            } else {
                let freed = self.blockmap_truncate(inode, keep)?;
                self.sub_inode_blocks(inode, freed);
            }
            // Zero the tail of the new last block so a later extension
            // reads zeros there.
            self.zero_block_tail(ino, inode, new_size)?;
        } else if new_size > old {
            // Bytes past the old EOF may hold data from a write that failed
            // after touching the block; the extension must read as zeros.
            self.zero_block_tail(ino, inode, old)?;
        }
        inode.set_size(new_size);
        if new_size > 0x7FFF_FFFF && !self.sb.has_ro_compat(ro_compat::LARGE_FILE) {
            self.sb
                .set_feature_ro_compat(self.sb.feature_ro_compat() | ro_compat::LARGE_FILE);
            self.mark_sb_dirty();
        }
        Ok(())
    }

    /// Zero the bytes of the block holding offset `size` from `size` to the
    /// end of that block, if that block is mapped to written data.
    fn zero_block_tail(&mut self, ino: u32, inode: &Inode, size: u64) -> Result<()> {
        let bs = self.block_size as u64;
        let tail = (size % bs) as usize;
        if tail == 0 {
            return Ok(());
        }
        if let Mapping::Data { physical, .. } = self.map_blocks(ino, inode, size / bs, 1)? {
            let mut buf = vec![0u8; bs as usize];
            self.read_data(physical, &mut buf)?;
            if buf[tail..].iter().any(|&b| b != 0) {
                buf[tail..].fill(0);
                self.write_data(physical, &buf)?;
            }
        }
        Ok(())
    }
}
