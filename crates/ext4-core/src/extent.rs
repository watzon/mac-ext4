//! Extent trees (`EXT4_EXTENTS_FL`): parsing, lookup, and rebuilding.
//!
//! Appending after the last extent edits only the rightmost path of the
//! tree ([`Volume::extent_append`]).  Other changes work on the flattened,
//! sorted extent list of a file: the caller edits the list and
//! [`Volume::store_extents`] writes a fresh tree, reusing the file's
//! existing node blocks where it can.

use std::sync::Arc;

use crate::bytes::*;
use crate::crc::crc32c;
use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::volume::Volume;

pub const EXTENT_MAGIC: u16 = 0xF30A;
/// Longest initialized extent.
pub const MAX_INIT_LEN: u32 = 32768;
/// Longest unwritten (preallocated) extent.
pub const MAX_UNWRITTEN_LEN: u32 = 32767;
const MAX_DEPTH: u16 = 5;

/// Nodes of one root-to-leaf path: the block of each node (`None` for the
/// root in the inode) and its content.
type TreePath = Vec<(Option<u64>, Arc<Vec<u8>>)>;

/// One mapping of `len` logical blocks starting at `logical` to physical
/// blocks starting at `physical`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub logical: u32,
    pub physical: u64,
    pub len: u32,
    /// Preallocated but never written: reads return zeros.
    pub unwritten: bool,
}

impl Extent {
    pub fn end(&self) -> u64 {
        self.logical as u64 + self.len as u64
    }
}

#[derive(Clone, Copy, Debug)]
struct Header {
    entries: u16,
    max: u16,
    depth: u16,
}

fn parse_header(b: &[u8]) -> Result<Header> {
    if b.len() < 12 || le16(b, 0) != EXTENT_MAGIC {
        return Err(Error::corrupt("bad extent header magic"));
    }
    let h = Header {
        entries: le16(b, 2),
        max: le16(b, 4),
        depth: le16(b, 6),
    };
    if h.entries > h.max || 12 + h.max as usize * 12 > b.len() || h.depth > MAX_DEPTH {
        return Err(Error::corrupt("bad extent header"));
    }
    Ok(h)
}

fn parse_extent(b: &[u8], off: usize) -> Extent {
    let raw_len = le16(b, off + 4) as u32;
    let (len, unwritten) = if raw_len > MAX_INIT_LEN {
        (raw_len - MAX_INIT_LEN, true)
    } else {
        (raw_len, false)
    };
    Extent {
        logical: le32(b, off),
        physical: lohi(le32(b, off + 8), le16(b, off + 6) as u32),
        len,
        unwritten,
    }
}

fn parse_index(b: &[u8], off: usize) -> (u32, u64) {
    (
        le32(b, off),
        lohi(le32(b, off + 4), le16(b, off + 8) as u32),
    )
}

/// Checksum of a non-root tree node.
fn node_checksum(seed: u32, b: &[u8], max: u16) -> u32 {
    crc32c(seed, &b[..12 + max as usize * 12])
}

impl Volume {
    fn read_extent_node(
        &self,
        inode: &Inode,
        ino: u32,
        blk: u64,
        depth: u16,
    ) -> Result<Arc<Vec<u8>>> {
        let b = self.read_block(blk)?;
        let h = parse_header(&b)?;
        if h.depth != depth {
            return Err(Error::corrupt(format!(
                "extent node {blk} of inode {ino} has the wrong depth"
            )));
        }
        if self.sb.has_metadata_csum() && !self.ignore_checksums {
            let tail = 12 + h.max as usize * 12;
            if tail + 4 <= b.len() {
                let seed = Inode::csum_seed(&self.sb, ino, inode.generation());
                if node_checksum(seed, &b, h.max) != le32(&b, tail) {
                    return Err(Error::corrupt(format!(
                        "extent block {blk} of inode {ino} checksum mismatch"
                    )));
                }
            }
        }
        Ok(b)
    }

    /// Find the extent containing logical block `lblk`, or the next extent
    /// after it. Returns `(containing, next_start)`; `next_start` is the
    /// first logical block of the following extent if `lblk` is in a hole.
    pub fn extent_lookup(
        &self,
        inode: &Inode,
        ino: u32,
        lblk: u32,
    ) -> Result<(Option<Extent>, Option<u32>)> {
        let root = inode.i_block();
        let mut h = parse_header(root)?;
        let mut node: Arc<Vec<u8>> = Arc::new(root.to_vec());
        // Smallest start of an extent/subtree to the right seen on the way down.
        let mut right_bound: Option<u32> = None;
        let mut guard = 0;
        while h.depth > 0 {
            guard += 1;
            if guard > MAX_DEPTH as usize + 1 || h.entries == 0 {
                return if h.entries == 0 {
                    Ok((None, right_bound))
                } else {
                    Err(Error::corrupt("extent tree loop"))
                };
            }
            // Last index whose start <= lblk (or the first one).
            let mut pick = 0usize;
            for i in 0..h.entries as usize {
                let (start, _) = parse_index(&node, 12 + i * 12);
                if start <= lblk {
                    pick = i;
                } else {
                    right_bound = Some(right_bound.map_or(start, |r| r.min(start)));
                    break;
                }
            }
            let (_, child) = parse_index(&node, 12 + pick * 12);
            let depth = h.depth - 1;
            node = self.read_extent_node(inode, ino, child, depth)?;
            h = parse_header(&node)?;
        }
        for i in 0..h.entries as usize {
            let e = parse_extent(&node, 12 + i * 12);
            if lblk < e.logical {
                return Ok((None, Some(e.logical)));
            }
            if (lblk as u64) < e.end() {
                return Ok((Some(e), None));
            }
        }
        Ok((None, right_bound))
    }

    /// All extents of a file in logical order, plus the physical blocks of
    /// the tree's non-root nodes.
    pub fn read_extents(&self, inode: &Inode, ino: u32) -> Result<(Vec<Extent>, Vec<u64>)> {
        let mut extents = Vec::new();
        let mut nodes = Vec::new();
        let root = inode.i_block().to_vec();
        self.walk_extents(inode, ino, &root, None, &mut extents, &mut nodes, 0)?;
        // Validate ordering; overlapping extents mean corruption.
        for w in extents.windows(2) {
            if w[0].end() > w[1].logical as u64 {
                return Err(Error::corrupt(format!(
                    "inode {ino} has overlapping extents"
                )));
            }
        }
        Ok((extents, nodes))
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_extents(
        &self,
        inode: &Inode,
        ino: u32,
        node: &[u8],
        expect_depth: Option<u16>,
        out: &mut Vec<Extent>,
        nodes: &mut Vec<u64>,
        level: usize,
    ) -> Result<()> {
        let h = parse_header(node)?;
        if expect_depth.is_some_and(|d| d != h.depth) || level > MAX_DEPTH as usize {
            return Err(Error::corrupt(format!(
                "inode {ino} extent tree depth mismatch"
            )));
        }
        if h.depth == 0 {
            for i in 0..h.entries as usize {
                let e = parse_extent(node, 12 + i * 12);
                if e.len == 0 {
                    continue;
                }
                out.push(e);
            }
            return Ok(());
        }
        for i in 0..h.entries as usize {
            let (_, child) = parse_index(node, 12 + i * 12);
            let b = self.read_extent_node(inode, ino, child, h.depth - 1)?;
            nodes.push(child);
            self.walk_extents(inode, ino, &b, Some(h.depth - 1), out, nodes, level + 1)?;
        }
        Ok(())
    }

    /// Write `extents` (sorted, non-overlapping) as the inode's extent tree.
    ///
    /// `old_nodes` are the tree blocks currently owned by the file; they are
    /// reused first. Surplus old nodes are freed; missing nodes are allocated
    /// near `goal`. Updates `i_blocks` for node blocks added or removed.
    pub(crate) fn store_extents(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        extents: &[Extent],
        old_nodes: &[u64],
        goal: u64,
    ) -> Result<()> {
        let bs = self.block_size;
        let per_node = (bs - 12) / 12;
        let root_max = 4usize;

        // Decide the tree shape bottom-up: level 0 = leaves.
        let mut level_counts: Vec<usize> = Vec::new(); // nodes per level (non-root)
        let mut n = extents.len();
        while n > root_max {
            let nodes = n.div_ceil(per_node);
            level_counts.push(nodes);
            n = nodes;
        }
        let depth = level_counts.len();
        if depth > MAX_DEPTH as usize {
            return Err(Error::FileTooLarge);
        }
        let needed: usize = level_counts.iter().sum();

        // Acquire node blocks.
        let mut blocks: Vec<u64> = old_nodes.iter().copied().take(needed).collect();
        if blocks.len() < needed {
            let extra = needed - blocks.len();
            let mut got = Vec::new();
            for _ in 0..extra {
                let g = got.last().map_or(goal, |&b: &u64| b + 1);
                got.push(self.alloc_block(g)?);
            }
            blocks.extend(got);
            self.add_inode_blocks(inode, extra as u64)?;
        }
        for &b in old_nodes.iter().skip(needed) {
            self.free_blocks(b, 1)?;
            self.forget_block(b);
            self.sub_inode_blocks(inode, 1);
        }

        let seed = Inode::csum_seed(&self.sb, ino, inode.generation());
        let generation = 0u32;

        // Build leaves, then index levels.
        let mut cursor = 0usize;
        let mut items: Vec<Item> = extents.iter().map(|e| Item::Extent(*e)).collect();
        for (lvl, &count) in level_counts.iter().enumerate() {
            let mut next: Vec<Item> = Vec::with_capacity(count);
            let chunks = pack_full(items.len(), per_node);
            debug_assert_eq!(chunks.len(), count);
            let mut start = 0usize;
            for len in chunks {
                let blk = blocks[cursor];
                cursor += 1;
                let slice = &items[start..start + len];
                let first = slice.first().map_or(0, |it| it.first());
                let buf = self.new_block_mut(blk)?;
                write_node(buf, slice, per_node as u16, lvl as u16, generation);
                seal_node(seed, buf);
                next.push(Item::Index(first, blk));
                start += len;
            }
            items = next;
        }
        let root = inode.i_block_mut();
        root.fill(0);
        write_node(root, &items, root_max as u16, depth as u16, generation);
        Ok(())
    }

    /// The nodes from the root down to the rightmost leaf.  The root comes
    /// first with no block number; it lives in the inode.
    fn rightmost_path(&self, inode: &Inode, ino: u32) -> Result<TreePath> {
        let mut path = vec![(None, Arc::new(inode.i_block().to_vec()))];
        loop {
            let node = &path[path.len() - 1].1;
            let h = parse_header(node)?;
            if h.depth == 0 {
                return Ok(path);
            }
            if h.entries == 0 || path.len() > MAX_DEPTH as usize {
                return Err(Error::corrupt(format!(
                    "inode {ino} has a malformed extent index"
                )));
            }
            let (_, child) = parse_index(node, 12 + (h.entries as usize - 1) * 12);
            let next = self.read_extent_node(inode, ino, child, h.depth - 1)?;
            path.push((Some(child), next));
        }
    }

    /// The last extent of the file, found by walking only the rightmost
    /// path of the tree.
    pub(crate) fn last_extent(&self, inode: &Inode, ino: u32) -> Result<Option<Extent>> {
        let path = self.rightmost_path(inode, ino)?;
        let leaf = &path[path.len() - 1].1;
        let h = parse_header(leaf)?;
        Ok((h.entries > 0).then(|| parse_extent(leaf, 12 + (h.entries as usize - 1) * 12)))
    }

    /// Map `e` after the last extent of the file, editing only the
    /// rightmost path of the tree.  The last extent grows in place when `e`
    /// continues it; otherwise `e` is added to the last leaf, or to a new
    /// leaf when the last one is full and its parent has room.
    ///
    /// Returns false, having changed nothing, when `e` does not start at or
    /// after the end of the file's mapping or the tree needs a full rebuild
    /// with [`Volume::store_extents`].  New tree blocks are allocated near
    /// `goal`.
    pub(crate) fn extent_append(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        e: Extent,
        goal: u64,
    ) -> Result<bool> {
        let path = self.rightmost_path(inode, ino)?;
        let (leaf_blk, leaf) = path[path.len() - 1].clone();
        let h = parse_header(&leaf)?;
        let seed = Inode::csum_seed(&self.sb, ino, inode.generation());
        if h.entries > 0 {
            let at = 12 + (h.entries as usize - 1) * 12;
            let last = parse_extent(&leaf, at);
            if last.len == 0 || (e.logical as u64) < last.end() {
                return Ok(false);
            }
            if !last.unwritten
                && !e.unwritten
                && last.end() == e.logical as u64
                && last.physical + last.len as u64 == e.physical
                && last.len + e.len <= MAX_INIT_LEN
            {
                let grown = Extent {
                    len: last.len + e.len,
                    ..last
                };
                self.edit_node(inode, leaf_blk, seed, |b| put_extent(b, at, &grown))?;
                return Ok(true);
            }
        } else if path.len() > 1 {
            // An empty non-root leaf: the last extent is elsewhere.
            return Ok(false);
        }
        if h.entries < h.max {
            let at = 12 + h.entries as usize * 12;
            self.edit_node(inode, leaf_blk, seed, |b| {
                put_extent(b, at, &e);
                put16(b, 2, h.entries + 1);
            })?;
            return Ok(true);
        }
        // The last leaf is full: start a new leaf under the same parent.
        if path.len() < 2 {
            return Ok(false);
        }
        let (parent_blk, parent) = &path[path.len() - 2];
        let ph = parse_header(parent)?;
        if ph.entries >= ph.max {
            return Ok(false);
        }
        let per_node = ((self.block_size - 12) / 12) as u16;
        let nb = self.alloc_block(goal)?;
        self.add_inode_blocks(inode, 1)?;
        let buf = self.new_block_mut(nb)?;
        write_node(buf, &[Item::Extent(e)], per_node, 0, 0);
        seal_node(seed, buf);
        let at = 12 + ph.entries as usize * 12;
        self.edit_node(inode, *parent_blk, seed, |b| {
            put_index(b, at, e.logical, nb);
            put16(b, 2, ph.entries + 1);
        })?;
        Ok(true)
    }

    /// Apply `f` to a tree node: the inode root when `blk` is `None`,
    /// otherwise a staged copy of block `blk`, whose checksum is renewed.
    fn edit_node(
        &mut self,
        inode: &mut Inode,
        blk: Option<u64>,
        seed: u32,
        f: impl FnOnce(&mut [u8]),
    ) -> Result<()> {
        match blk {
            None => f(inode.i_block_mut()),
            Some(b) => {
                let buf = self.block_mut(b)?;
                f(buf);
                seal_node(seed, buf);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Item {
    Extent(Extent),
    Index(u32, u64),
}

impl Item {
    fn first(&self) -> u32 {
        match self {
            Item::Extent(e) => e.logical,
            Item::Index(l, _) => *l,
        }
    }
}

fn write_node(buf: &mut [u8], items: &[Item], max: u16, depth: u16, generation: u32) {
    put16(buf, 0, EXTENT_MAGIC);
    put16(buf, 2, items.len() as u16);
    put16(buf, 4, max);
    put16(buf, 6, depth);
    put32(buf, 8, generation);
    for (i, it) in items.iter().enumerate() {
        let off = 12 + i * 12;
        match it {
            Item::Extent(e) => put_extent(buf, off, e),
            Item::Index(l, b) => put_index(buf, off, *l, *b),
        }
    }
}

fn put_extent(buf: &mut [u8], off: usize, e: &Extent) {
    put32(buf, off, e.logical);
    let raw_len = if e.unwritten {
        e.len + MAX_INIT_LEN
    } else {
        e.len
    };
    put16(buf, off + 4, raw_len as u16);
    put16(buf, off + 6, (e.physical >> 32) as u16);
    put32(buf, off + 8, e.physical as u32);
}

fn put_index(buf: &mut [u8], off: usize, logical: u32, block: u64) {
    put32(buf, off, logical);
    put32(buf, off + 4, block as u32);
    put16(buf, off + 8, (block >> 32) as u16);
    put16(buf, off + 10, 0);
}

/// Store the checksum of a non-root node in the tail after its entries.
fn seal_node(seed: u32, buf: &mut [u8]) {
    let max = le16(buf, 4);
    let tail = 12 + max as usize * 12;
    if tail + 4 <= buf.len() {
        let c = node_checksum(seed, buf, max);
        put32(buf, tail, c);
    }
}

/// Split `n` items into nodes of `per_node` entries, filling each node
/// before starting the next, as Linux and e2fsck build trees.  Only the
/// last node has spare room, which is where appends go; e2fsck reports a
/// tree whose levels have a node's worth of spare room as one that could
/// be narrower.
fn pack_full(n: usize, per_node: usize) -> Vec<usize> {
    (0..n.div_ceil(per_node))
        .map(|i| (n - i * per_node).min(per_node))
        .collect()
}

/// Write an empty depth-0 extent header into an inode's `i_block`.
pub fn init_extent_root(inode: &mut Inode) {
    let root = inode.i_block_mut();
    root.fill(0);
    write_node(root, &[], 4, 0, 0);
}

/// Append `run` to a sorted extent list, merging with the last extent when
/// they are logically and physically contiguous.
pub fn push_merged(list: &mut Vec<Extent>, run: Extent) {
    if let Some(last) = list.last_mut() {
        let max = if last.unwritten {
            MAX_UNWRITTEN_LEN
        } else {
            MAX_INIT_LEN
        };
        if last.unwritten == run.unwritten
            && last.end() == run.logical as u64
            && last.physical + last.len as u64 == run.physical
            && last.len + run.len <= max
        {
            last.len += run.len;
            return;
        }
    }
    list.push(run);
}

/// Merge neighbors throughout a sorted list.
pub fn normalize(list: &mut Vec<Extent>) {
    let old = std::mem::take(list);
    for e in old {
        if e.len == 0 {
            continue;
        }
        let max = if e.unwritten {
            MAX_UNWRITTEN_LEN
        } else {
            MAX_INIT_LEN
        };
        // Split extents longer than the per-extent limit.
        let mut rest = e;
        while rest.len > max {
            push_merged(list, Extent { len: max, ..rest });
            rest.logical += max;
            rest.physical += max as u64;
            rest.len -= max;
        }
        push_merged(list, rest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_full_fills_nodes_in_order() {
        assert_eq!(pack_full(10, 4), vec![4, 4, 2]);
        assert_eq!(pack_full(340, 340), vec![340]);
        assert_eq!(pack_full(341, 340), vec![340, 1]);
    }

    #[test]
    fn normalize_merges_and_splits() {
        let mut v = vec![
            Extent {
                logical: 0,
                physical: 100,
                len: 10,
                unwritten: false,
            },
            Extent {
                logical: 10,
                physical: 110,
                len: 5,
                unwritten: false,
            },
            Extent {
                logical: 15,
                physical: 115,
                len: 5,
                unwritten: true,
            },
            Extent {
                logical: 20,
                physical: 1000,
                len: 40000,
                unwritten: false,
            },
        ];
        normalize(&mut v);
        assert_eq!(v.len(), 4);
        assert_eq!(
            v[0],
            Extent {
                logical: 0,
                physical: 100,
                len: 15,
                unwritten: false
            }
        );
        assert_eq!(v[2].len, 32768);
        assert_eq!(
            v[3],
            Extent {
                logical: 20 + 32768,
                physical: 1000 + 32768,
                len: 40000 - 32768,
                unwritten: false
            }
        );
    }
}
