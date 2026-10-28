//! Classic ext2/ext3 block maps: 12 direct pointers, then single, double,
//! and triple indirect blocks.

use crate::bytes::*;
use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::volume::Volume;

const DIRECT: u64 = 12;

/// Path through the indirect tree to one logical block.
#[derive(Debug, PartialEq, Eq)]
enum Path {
    Direct(usize),
    /// Root slot in `i_block` (12, 13, 14) and offsets per level.
    Indirect(usize, Vec<usize>),
}

fn path_for(lblk: u64, per: u64) -> Result<Path> {
    if lblk < DIRECT {
        return Ok(Path::Direct(lblk as usize));
    }
    let mut rem = lblk - DIRECT;
    if rem < per {
        return Ok(Path::Indirect(12, vec![rem as usize]));
    }
    rem -= per;
    if rem < per * per {
        return Ok(Path::Indirect(
            13,
            vec![(rem / per) as usize, (rem % per) as usize],
        ));
    }
    rem -= per * per;
    if rem < per * per * per {
        return Ok(Path::Indirect(
            14,
            vec![
                (rem / (per * per)) as usize,
                ((rem / per) % per) as usize,
                (rem % per) as usize,
            ],
        ));
    }
    Err(Error::FileTooLarge)
}

/// Largest logical block count addressable by a block map.
pub fn max_blocks(block_size: usize) -> u64 {
    let per = (block_size / 4) as u64;
    DIRECT + per + per * per + per * per * per
}

/// Block maps store 32-bit block numbers: every data and indirect block of a
/// block-mapped file must be below this.
pub(crate) const BLOCKMAP_LIMIT: u64 = 1 << 32;

/// Largest size in bytes of a block-mapped file, computed as Linux
/// `ext4_max_bitmap_size` does.  Without `huge_file`, `i_blocks` is a 32-bit
/// count of 512-byte sectors, and it must hold the data blocks plus every
/// indirect block needed to map them.
pub fn max_size(block_size: usize, huge_file: bool) -> u64 {
    let bits = block_size.trailing_zeros();
    let ppb = (block_size / 4) as u64;
    let mut upper = if huge_file {
        (1u64 << 48) - 1
    } else {
        u32::MAX as u64 >> (bits - 9)
    };
    let mut res = max_blocks(block_size);
    let full_meta = 1 + (1 + ppb) + (1 + ppb + ppb * ppb);
    if res + full_meta > upper {
        // Count the indirect blocks needed to map `upper` blocks densely.
        res = upper;
        upper -= DIRECT + ppb;
        let mut meta = 1;
        if upper < ppb * ppb {
            meta += 1 + upper.div_ceil(ppb);
        } else {
            meta += 1 + ppb;
            upper -= ppb * ppb;
            meta += 1 + upper.div_ceil(ppb) + upper.div_ceil(ppb * ppb);
        }
        res -= meta;
    }
    res.min(u32::MAX as u64) << bits
}

impl Volume {
    fn per_block(&self) -> u64 {
        (self.block_size / 4) as u64
    }

    /// Physical block for logical block `lblk`, or 0 for a hole.
    pub fn blockmap_lookup(&self, inode: &Inode, lblk: u64) -> Result<u64> {
        let root = inode.i_block();
        match path_for(lblk, self.per_block())? {
            Path::Direct(i) => Ok(le32(root, i * 4) as u64),
            Path::Indirect(slot, offs) => {
                let mut blk = le32(root, slot * 4) as u64;
                for off in offs {
                    if blk == 0 {
                        return Ok(0);
                    }
                    blk = self.with_block(blk, |b| le32(b, off * 4) as u64)?;
                }
                Ok(blk)
            }
        }
    }

    /// Walk all mapped blocks of a block-mapped file, calling `f(lblk, pblk)`
    /// for data blocks in logical order. Also returns the indirect blocks.
    pub fn blockmap_walk(
        &self,
        inode: &Inode,
        mut f: impl FnMut(u64, u64) -> Result<()>,
    ) -> Result<Vec<u64>> {
        let root = inode.i_block();
        let per = self.per_block();
        let mut meta = Vec::new();
        for i in 0..DIRECT as usize {
            let b = le32(root, i * 4) as u64;
            if b != 0 {
                f(i as u64, b)?;
            }
        }
        let mut base = DIRECT;
        for (slot, level) in [(12usize, 1u32), (13, 2), (14, 3)] {
            let b = le32(root, slot * 4) as u64;
            if b != 0 {
                self.walk_indirect(b, level, base, &mut f, &mut meta)?;
            }
            base += per.pow(level);
        }
        Ok(meta)
    }

    fn walk_indirect(
        &self,
        blk: u64,
        level: u32,
        base: u64,
        f: &mut impl FnMut(u64, u64) -> Result<()>,
        meta: &mut Vec<u64>,
    ) -> Result<()> {
        meta.push(blk);
        let data = self.read_block(blk)?;
        let per = self.per_block();
        let span = per.pow(level - 1);
        for i in 0..per as usize {
            let child = le32(&data, i * 4) as u64;
            if child == 0 {
                continue;
            }
            let lblk = base + i as u64 * span;
            if level == 1 {
                f(lblk, child)?;
            } else {
                self.walk_indirect(child, level - 1, lblk, f, meta)?;
            }
        }
        Ok(())
    }

    /// Map logical block `lblk` to `pblk`, allocating indirect blocks as
    /// needed. Returns the number of indirect blocks allocated.
    pub(crate) fn blockmap_set(&mut self, inode: &mut Inode, lblk: u64, pblk: u64) -> Result<u64> {
        if pblk >= BLOCKMAP_LIMIT {
            return Err(Error::invalid(format!(
                "block {pblk} cannot be stored in a 32-bit block map"
            )));
        }
        let per = self.per_block();
        let mut allocated = 0;
        match path_for(lblk, per)? {
            Path::Direct(i) => {
                put32(inode.i_block_mut(), i * 4, pblk as u32);
            }
            Path::Indirect(slot, offs) => {
                let mut parent: Option<u64> = None; // None = inode root
                let mut parent_off = slot;
                for (depth, &off) in offs.iter().enumerate() {
                    let cur = match parent {
                        None => le32(inode.i_block(), parent_off * 4) as u64,
                        Some(p) => self.with_block(p, |b| le32(b, parent_off * 4) as u64)?,
                    };
                    let cur = if cur == 0 {
                        let nb = self.alloc_blocks_below(pblk, 1, BLOCKMAP_LIMIT)?.0;
                        self.new_block_mut(nb)?;
                        allocated += 1;
                        match parent {
                            None => put32(inode.i_block_mut(), parent_off * 4, nb as u32),
                            Some(p) => put32(self.block_mut(p)?, parent_off * 4, nb as u32),
                        }
                        nb
                    } else {
                        cur
                    };
                    if depth + 1 == offs.len() {
                        put32(self.block_mut(cur)?, off * 4, pblk as u32);
                    }
                    parent = Some(cur);
                    parent_off = off;
                }
            }
        }
        Ok(allocated)
    }

    /// Free every data and indirect block at or after logical block `keep`.
    /// Returns the number of blocks freed.
    pub(crate) fn blockmap_truncate(&mut self, inode: &mut Inode, keep: u64) -> Result<u64> {
        let per = self.per_block();
        let mut freed = 0u64;
        // Direct blocks.
        for i in keep.min(DIRECT) as usize..DIRECT as usize {
            let b = le32(inode.i_block(), i * 4) as u64;
            if b != 0 {
                self.free_blocks(b, 1)?;
                freed += 1;
                put32(inode.i_block_mut(), i * 4, 0);
            }
        }
        let mut base = DIRECT;
        for (slot, level) in [(12usize, 1u32), (13, 2), (14, 3)] {
            let span = per.pow(level);
            let b = le32(inode.i_block(), slot * 4) as u64;
            if b != 0 && keep < base + span {
                let (n, empty) = self.truncate_indirect(b, level, base, keep)?;
                freed += n;
                if empty {
                    self.free_blocks(b, 1)?;
                    self.forget_block(b);
                    freed += 1;
                    put32(inode.i_block_mut(), slot * 4, 0);
                }
            }
            base += span;
        }
        Ok(freed)
    }

    /// Returns (blocks freed below `blk`, whether `blk` is now empty).
    fn truncate_indirect(
        &mut self,
        blk: u64,
        level: u32,
        base: u64,
        keep: u64,
    ) -> Result<(u64, bool)> {
        let per = self.per_block();
        let span = per.pow(level - 1);
        let data = self.read_block(blk)?;
        let mut freed = 0;
        let mut changed = Vec::new();
        let mut any_left = false;
        for i in 0..per as usize {
            let child = le32(&data, i * 4) as u64;
            if child == 0 {
                continue;
            }
            let start = base + i as u64 * span;
            if start + span <= keep {
                any_left = true;
                continue;
            }
            if level == 1 {
                self.free_blocks(child, 1)?;
                freed += 1;
                changed.push(i);
            } else {
                let (n, empty) = self.truncate_indirect(child, level - 1, start, keep)?;
                freed += n;
                if empty {
                    self.free_blocks(child, 1)?;
                    self.forget_block(child);
                    freed += 1;
                    changed.push(i);
                } else {
                    any_left = true;
                }
            }
        }
        if !any_left {
            return Ok((freed, true));
        }
        if !changed.is_empty() {
            let b = self.block_mut(blk)?;
            for i in changed {
                put32(b, i * 4, 0);
            }
        }
        Ok((freed, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(path_for(0, 256).unwrap(), Path::Direct(0));
        assert_eq!(path_for(12, 256).unwrap(), Path::Indirect(12, vec![0]));
        assert_eq!(
            path_for(12 + 256, 256).unwrap(),
            Path::Indirect(13, vec![0, 0])
        );
        assert_eq!(
            path_for(12 + 256 + 257, 256).unwrap(),
            Path::Indirect(13, vec![1, 1])
        );
        let t = 12 + 256 + 256 * 256;
        assert_eq!(path_for(t, 256).unwrap(), Path::Indirect(14, vec![0, 0, 0]));
        assert!(path_for(max_blocks(1024), 256).is_err());
    }
}
