//! Directory entries: block format, iteration, and lookup (linear and
//! hashed-index). Modification lives in `dirwrite.rs`.

use std::sync::Arc;

use crate::bytes::*;
use crate::crc::crc32c;
use crate::error::{Error, Result};
use crate::hash;
use crate::inode::{FileType, Inode, FL_INDEX, FL_INLINE_DATA};
use crate::superblock::{incompat, FLAG_UNSIGNED_HASH};
use crate::volume::Volume;

pub const NAME_MAX: usize = 255;
/// Size of the checksum tail (a fake directory entry) in leaf blocks.
pub const TAIL_SIZE: usize = 12;
const TAIL_FT: u8 = 0xDE;

/// A directory entry as returned to callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub inode: u32,
    pub name: Vec<u8>,
    pub file_type: FileType,
}

/// Minimum record length for a name of `name_len` bytes.
pub fn rec_len_for(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

/// Decode `rec_len`, which encodes 65536 specially on 64 KiB blocks.
pub fn decode_rec_len(raw: u16, block_size: usize) -> usize {
    if block_size >= 65536 && (raw == 65535 || raw == 0) {
        65536
    } else {
        raw as usize
    }
}

pub fn encode_rec_len(len: usize, block_size: usize) -> u16 {
    if block_size >= 65536 && len >= 65536 {
        65535
    } else {
        len as u16
    }
}

/// A raw entry inside a block buffer.
#[derive(Clone, Copy, Debug)]
pub struct RawEntry {
    pub offset: usize,
    pub inode: u32,
    pub rec_len: usize,
    pub name_len: usize,
    pub file_type: u8,
}

impl RawEntry {
    pub fn name<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.offset + 8..self.offset + 8 + self.name_len]
    }
    pub fn is_tail(&self) -> bool {
        self.inode == 0
            && self.rec_len == TAIL_SIZE
            && self.name_len == 0
            && self.file_type == TAIL_FT
    }
}

/// Parse the entries in `buf[start..end]`. Fails on malformed records.
pub fn parse_entries(
    buf: &[u8],
    start: usize,
    end: usize,
    filetype: bool,
    block_size: usize,
) -> Result<Vec<RawEntry>> {
    let mut out = Vec::new();
    let mut off = start;
    while off < end {
        if off + 8 > end {
            return Err(Error::corrupt(
                "directory entry header crosses the block end",
            ));
        }
        let inode = le32(buf, off);
        let rec_len = decode_rec_len(le16(buf, off + 4), block_size);
        let (name_len, file_type) = if filetype {
            (buf[off + 6] as usize, buf[off + 7])
        } else {
            (le16(buf, off + 6) as usize & 0xFF, 0)
        };
        if rec_len < 8
            || rec_len % 4 != 0
            || off + rec_len > end
            || (inode != 0 && 8 + name_len > rec_len)
        {
            return Err(Error::corrupt(format!(
                "bad directory entry at offset {off}"
            )));
        }
        out.push(RawEntry {
            offset: off,
            inode,
            rec_len,
            name_len: name_len.min(rec_len - 8),
            file_type,
        });
        off += rec_len;
    }
    Ok(out)
}

/// Write an entry header and name at `off`.
pub fn put_entry(
    buf: &mut [u8],
    off: usize,
    inode: u32,
    rec_len: usize,
    name: &[u8],
    ft: u8,
    block_size: usize,
) {
    put32(buf, off, inode);
    put16(buf, off + 4, encode_rec_len(rec_len, block_size));
    buf[off + 6] = name.len() as u8;
    buf[off + 7] = ft;
    buf[off + 8..off + 8 + name.len()].copy_from_slice(name);
    // Zero padding after the name for tidiness.
    let pad_end = (off + rec_len_for(name.len())).min(off + rec_len);
    for b in &mut buf[off + 8 + name.len()..pad_end] {
        *b = 0;
    }
}

/// Write the 12-byte checksum tail at the end of a leaf block.
pub fn put_tail(buf: &mut [u8]) {
    let off = buf.len() - TAIL_SIZE;
    put32(buf, off, 0);
    put16(buf, off + 4, TAIL_SIZE as u16);
    buf[off + 6] = 0;
    buf[off + 7] = TAIL_FT;
    put32(buf, off + 8, 0);
}

pub fn has_tail(buf: &[u8]) -> bool {
    let off = buf.len() - TAIL_SIZE;
    le32(buf, off) == 0
        && le16(buf, off + 4) as usize == TAIL_SIZE
        && buf[off + 6] == 0
        && buf[off + 7] == TAIL_FT
}

pub fn leaf_checksum(seed: u32, buf: &[u8]) -> u32 {
    crc32c(seed, &buf[..buf.len() - TAIL_SIZE])
}

/// The block's dx count/limit header offset if it is an htree interior
/// node or root, given whether it is block 0.
pub(crate) fn dx_count_offset(buf: &[u8], is_root: bool, block_size: usize) -> Option<usize> {
    if is_root {
        // '.' (12 bytes) then '..' whose rec_len covers the rest of the block.
        let dotdot_len = decode_rec_len(le16(buf, 16), block_size);
        if le16(buf, 4) != 12 || dotdot_len != block_size - 12 {
            return None;
        }
        let info_len = buf[24 + 5] as usize;
        Some(24 + info_len)
    } else {
        let rec = decode_rec_len(le16(buf, 4), block_size);
        if le32(buf, 0) == 0 && rec == block_size && buf[6] == 0 {
            Some(8)
        } else {
            None
        }
    }
}

/// Checksum of an htree node: the valid index records followed by the
/// checksum tail with its checksum field zeroed.
pub(crate) fn dx_checksum(
    seed: u32,
    buf: &[u8],
    count_off: usize,
    count: usize,
    tail_off: usize,
) -> u32 {
    let c = crc32c(seed, &buf[..count_off + count * 8]);
    let c = crc32c(c, &buf[tail_off..tail_off + 4]);
    crc32c(c, &[0; 4])
}

/// Hash parameters of an indexed directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DxInfo {
    pub hash_version: u8,
    pub levels: u8,
}

impl Volume {
    pub(crate) fn has_filetype(&self) -> bool {
        self.sb.has_incompat(incompat::FILETYPE)
    }

    /// Read logical block `lblk` of directory `ino`; verify its checksum.
    pub(crate) fn dir_block(&self, ino: u32, inode: &Inode, lblk: u64) -> Result<Arc<Vec<u8>>> {
        let phys = match self.map_blocks(ino, inode, lblk, 1)? {
            crate::bmap::Mapping::Data { physical, .. } => physical,
            _ => {
                return Err(Error::corrupt(format!(
                    "directory {ino} has a hole at block {lblk}"
                )))
            }
        };
        let b = self.read_block(phys)?;
        if self.sb.has_metadata_csum() && !self.ignore_checksums {
            self.verify_dir_block(ino, inode, lblk, &b)?;
        }
        Ok(b)
    }

    fn verify_dir_block(&self, ino: u32, inode: &Inode, lblk: u64, b: &[u8]) -> Result<()> {
        let seed = Inode::csum_seed(&self.sb, ino, inode.generation());
        if has_tail(b) {
            if leaf_checksum(seed, b) != le32(b, b.len() - 4) {
                return Err(Error::corrupt(format!(
                    "directory {ino} block {lblk} checksum mismatch"
                )));
            }
            return Ok(());
        }
        if inode.has_flag(FL_INDEX) {
            if let Some(co) = dx_count_offset(b, lblk == 0, self.block_size) {
                let limit = le16(b, co) as usize;
                let count = le16(b, co + 2) as usize;
                let tail = co + limit * 8;
                if count <= limit && tail + 8 <= b.len() {
                    if dx_checksum(seed, b, co, count, tail) != le32(b, tail + 4) {
                        return Err(Error::corrupt(format!(
                            "directory {ino} index block {lblk} checksum mismatch"
                        )));
                    }
                    return Ok(());
                }
            }
        }
        // Block without room for a checksum; Linux reports these, but they
        // are readable, so accept them.
        Ok(())
    }

    pub(crate) fn dir_block_count(&self, inode: &Inode) -> u64 {
        inode.size().div_ceil(self.block_size as u64)
    }

    /// Enumerate entries from position `cookie`, calling
    /// `f(entry, next_cookie)` until it returns false.
    ///
    /// Cookies are byte positions in the directory (block-mapped) or in the
    /// inline data area offset by 2 (inline; 0 and 1 are "." and "..").
    pub fn read_dir(
        &self,
        ino: u32,
        cookie: u64,
        mut f: impl FnMut(DirEntry, u64) -> bool,
    ) -> Result<()> {
        let inode = self.read_inode(ino)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if inode.has_flag(FL_INLINE_DATA) {
            return self.read_inline_dir(ino, &inode, cookie, f);
        }
        let bs = self.block_size as u64;
        let nblocks = self.dir_block_count(&inode);
        let mut lblk = cookie / bs;
        let mut skip_to = (cookie % bs) as usize;
        while lblk < nblocks {
            let b = self.dir_block(ino, &inode, lblk)?;
            let end = if has_tail(&b) {
                b.len() - TAIL_SIZE
            } else {
                b.len()
            };
            let entries = parse_entries(&b, 0, end, self.has_filetype(), self.block_size)?;
            for e in entries {
                if e.offset < skip_to {
                    continue;
                }
                if e.inode == 0 {
                    continue;
                }
                let next = lblk * bs + (e.offset + e.rec_len) as u64;
                let entry = DirEntry {
                    inode: e.inode,
                    name: e.name(&b).to_vec(),
                    file_type: self.entry_type(&e)?,
                };
                if !f(entry, next) {
                    return Ok(());
                }
            }
            lblk += 1;
            skip_to = 0;
        }
        Ok(())
    }

    fn entry_type(&self, e: &RawEntry) -> Result<FileType> {
        if self.has_filetype() {
            Ok(FileType::from_dirent_type(e.file_type))
        } else {
            Ok(self.read_inode(e.inode)?.file_type())
        }
    }

    /// Find `name` in directory `ino`.
    pub fn lookup(&self, ino: u32, name: &[u8]) -> Result<Option<DirEntry>> {
        let inode = self.read_inode(ino)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if name.is_empty() || name.len() > NAME_MAX {
            return if name.len() > NAME_MAX {
                Err(Error::NameTooLong)
            } else {
                Ok(None)
            };
        }
        Ok(self.find_entry(ino, &inode, name)?.map(|(e, _)| e))
    }

    /// Locate `name`; returns the entry and its (logical block, offset).
    pub(crate) fn find_entry(
        &self,
        ino: u32,
        inode: &Inode,
        name: &[u8],
    ) -> Result<Option<(DirEntry, (u64, usize))>> {
        if inode.has_flag(FL_INLINE_DATA) {
            return self.find_inline_entry(ino, inode, name);
        }
        // "." and ".." are the first two records of block 0, which in an
        // htree is the root, outside every hashed leaf. The linear search
        // below finds them in the first block it reads, as Linux does.
        let dot = name == b"." || name == b"..";
        if !dot
            && inode.has_flag(FL_INDEX)
            && self.sb.has_compat(crate::superblock::compat::DIR_INDEX)
        {
            match self.dx_find(ino, inode, name) {
                Ok(r) => return Ok(r),
                Err(Error::Corrupt(_)) | Err(Error::Unsupported(_)) => {
                    // A damaged or unknown index: the leaves are still a
                    // valid linear directory.
                }
                Err(e) => return Err(e),
            }
        }
        for lblk in 0..self.dir_block_count(inode) {
            let b = self.dir_block(ino, inode, lblk)?;
            if let Some(e) = self.search_block(&b, name)? {
                return Ok(Some((e.0, (lblk, e.1))));
            }
        }
        Ok(None)
    }

    fn search_block(&self, b: &[u8], name: &[u8]) -> Result<Option<(DirEntry, usize)>> {
        let end = if has_tail(b) {
            b.len() - TAIL_SIZE
        } else {
            b.len()
        };
        for e in parse_entries(b, 0, end, self.has_filetype(), self.block_size)? {
            if e.inode != 0 && e.name(b) == name {
                let entry = DirEntry {
                    inode: e.inode,
                    name: name.to_vec(),
                    file_type: self.entry_type(&e)?,
                };
                return Ok(Some((entry, e.offset)));
            }
        }
        Ok(None)
    }

    pub(crate) fn dx_info(&self, root: &[u8]) -> Result<DxInfo> {
        if dx_count_offset(root, true, self.block_size).is_none() {
            return Err(Error::corrupt("bad htree root"));
        }
        let info_len = root[29];
        let mut version = root[28];
        let levels = root[30];
        if info_len != 8 || le32(root, 24) != 0 {
            return Err(Error::corrupt("bad htree root info"));
        }
        let max_levels = if self.sb.has_incompat(incompat::LARGEDIR) {
            3
        } else {
            2
        };
        if levels >= max_levels {
            return Err(Error::corrupt("htree is too deep"));
        }
        if version > hash::TEA {
            if version == hash::SIPHASH {
                return Err(Error::unsupported("siphash directory index"));
            }
            return Err(Error::corrupt("unknown htree hash version"));
        }
        if self.sb.flags() & FLAG_UNSIGNED_HASH != 0 {
            version += 3;
        }
        Ok(DxInfo {
            hash_version: version,
            levels,
        })
    }

    /// Walk the htree path for `hash`. Returns, for each level, the block
    /// buffer, its logical block number, and the chosen entry index; the
    /// last element's entry points at the leaf.
    pub(crate) fn dx_path(
        &self,
        ino: u32,
        inode: &Inode,
        hash: u32,
        info: DxInfo,
    ) -> Result<Vec<DxLevel>> {
        let nblocks = self.dir_block_count(inode);
        let mut path = Vec::new();
        let mut lblk = 0u64;
        for depth in 0..=info.levels {
            let b = self.dir_block(ino, inode, lblk)?;
            let co = dx_count_offset(&b, depth == 0, self.block_size)
                .ok_or_else(|| Error::corrupt("bad htree node"))?;
            let limit = le16(&b, co) as usize;
            let count = le16(&b, co + 2) as usize;
            if count == 0 || count > limit || co + limit * 8 > b.len() {
                return Err(Error::corrupt("bad htree node counts"));
            }
            // Binary search: last entry with key <= hash (entry 0 has key 0).
            let (mut lo, mut hi) = (1usize, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if le32(&b, co + mid * 8) <= hash {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let idx = lo - 1;
            let child = le32(&b, co + idx * 8 + 4) as u64 & 0x0FFF_FFFF;
            if child >= nblocks {
                return Err(Error::corrupt("htree points past the directory end"));
            }
            path.push(DxLevel {
                lblk,
                buf: b,
                count_off: co,
                count,
                idx,
            });
            lblk = child;
        }
        Ok(path)
    }

    fn dx_find(
        &self,
        ino: u32,
        inode: &Inode,
        name: &[u8],
    ) -> Result<Option<(DirEntry, (u64, usize))>> {
        let root = self.dir_block(ino, inode, 0)?;
        let info = self.dx_info(&root)?;
        let (hash, _) = hash::dirhash(info.hash_version, name, self.sb.hash_seed())
            .ok_or_else(|| Error::unsupported("hash version"))?;
        let mut path = self.dx_path(ino, inode, hash, info)?;
        loop {
            let leaf_lblk = path
                .last()
                .map(|l| l.child())
                .ok_or_else(|| Error::corrupt("empty htree path"))?;
            let b = self.dir_block(ino, inode, leaf_lblk)?;
            if let Some((e, off)) = self.search_block(&b, name)? {
                return Ok(Some((e, (leaf_lblk, off))));
            }
            // Hash collisions may continue in the next leaf, flagged by the
            // low bit of the next index key.
            if !self.dx_next_leaf(ino, inode, &mut path, hash)? {
                return Ok(None);
            }
        }
    }

    /// Advance `path` to the next leaf if its starting key continues `hash`.
    fn dx_next_leaf(
        &self,
        ino: u32,
        inode: &Inode,
        path: &mut Vec<DxLevel>,
        hash: u32,
    ) -> Result<bool> {
        // Find the deepest level that has a next entry.
        let mut level = path.len();
        loop {
            if level == 0 {
                return Ok(false);
            }
            level -= 1;
            if path[level].idx + 1 < path[level].count {
                break;
            }
        }
        path[level].idx += 1;
        let key = path[level].key(path[level].idx);
        if key & 1 == 0 || (key & !1) != hash {
            return Ok(false);
        }
        // Re-descend along the leftmost entries below `level`.
        path.truncate(level + 1);
        let levels = self.dx_info(&self.dir_block(ino, inode, 0)?)?.levels as usize;
        while path.len() <= levels {
            let lblk = path.last().expect("non-empty").child();
            let b = self.dir_block(ino, inode, lblk)?;
            let co = dx_count_offset(&b, false, self.block_size)
                .ok_or_else(|| Error::corrupt("bad htree node"))?;
            let limit = le16(&b, co) as usize;
            let count = le16(&b, co + 2) as usize;
            if count == 0 || count > limit {
                return Err(Error::corrupt("bad htree node counts"));
            }
            path.push(DxLevel {
                lblk,
                buf: b,
                count_off: co,
                count,
                idx: 0,
            });
        }
        Ok(true)
    }
}

/// One level of an htree lookup path.
pub(crate) struct DxLevel {
    pub lblk: u64,
    pub buf: Arc<Vec<u8>>,
    pub count_off: usize,
    pub count: usize,
    pub idx: usize,
}

impl DxLevel {
    pub fn key(&self, i: usize) -> u32 {
        if i == 0 {
            0
        } else {
            le32(&self.buf, self.count_off + i * 8)
        }
    }
    pub fn child(&self) -> u64 {
        le32(&self.buf, self.count_off + self.idx * 8 + 4) as u64 & 0x0FFF_FFFF
    }
}
