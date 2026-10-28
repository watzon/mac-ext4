//! Extended attributes stored in the inode body and in one external block.

use crate::bytes::*;
use crate::crc::crc32c;
use crate::error::{Error, Result};
use crate::inode::{Inode, GOOD_OLD_INODE_SIZE};
use crate::superblock::compat;
use crate::volume::Volume;

pub const XATTR_MAGIC: u32 = 0xEA02_0000;
const BLOCK_HEADER: usize = 32;
const ENTRY_HEADER: usize = 16;

pub const INDEX_USER: u8 = 1;
pub const INDEX_POSIX_ACL_ACCESS: u8 = 2;
pub const INDEX_POSIX_ACL_DEFAULT: u8 = 3;
pub const INDEX_TRUSTED: u8 = 4;
pub const INDEX_SECURITY: u8 = 6;
pub const INDEX_SYSTEM: u8 = 7;
pub const INDEX_RICHACL: u8 = 8;

/// One attribute: namespace index, name without the namespace prefix, value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Xattr {
    pub index: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl Xattr {
    /// Full Linux name, such as `user.comment`.
    pub fn full_name(&self) -> Vec<u8> {
        let mut v = prefix(self.index).as_bytes().to_vec();
        v.extend_from_slice(&self.name);
        v
    }
}

pub fn prefix(index: u8) -> &'static str {
    match index {
        INDEX_USER => "user.",
        INDEX_POSIX_ACL_ACCESS => "system.posix_acl_access",
        INDEX_POSIX_ACL_DEFAULT => "system.posix_acl_default",
        INDEX_TRUSTED => "trusted.",
        INDEX_SECURITY => "security.",
        INDEX_SYSTEM => "system.",
        INDEX_RICHACL => "system.richacl",
        _ => "unknown.",
    }
}

/// Split a full Linux attribute name into (index, suffix).
pub fn split_name(full: &[u8]) -> Option<(u8, &[u8])> {
    // Exact names first: they share the "system." prefix.
    for idx in [
        INDEX_POSIX_ACL_ACCESS,
        INDEX_POSIX_ACL_DEFAULT,
        INDEX_RICHACL,
    ] {
        if full == prefix(idx).as_bytes() {
            return Some((idx, &[]));
        }
    }
    for idx in [INDEX_USER, INDEX_TRUSTED, INDEX_SECURITY, INDEX_SYSTEM] {
        let p = prefix(idx).as_bytes();
        if full.len() > p.len() && full.starts_with(p) {
            return Some((idx, &full[p.len()..]));
        }
    }
    None
}

fn entry_size(name_len: usize) -> usize {
    (ENTRY_HEADER + name_len + 3) & !3
}

fn value_size(len: usize) -> usize {
    (len + 3) & !3
}

/// Hash of one entry (name bytes treated as unsigned).
pub fn entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 5) ^ (h >> 27) ^ c as u32;
    }
    let mut padded = value.to_vec();
    padded.resize(value_size(value.len()), 0);
    for w in padded.chunks_exact(4) {
        h = (h << 16) ^ (h >> 16) ^ le32(w, 0);
    }
    h
}

/// Parse entries starting at `start` in `area`. `value_base` is the offset
/// that `e_value_offs` is relative to. `limit` bounds values.
fn parse_area(area: &[u8], start: usize, value_base: usize, out: &mut Vec<RawXattr>) -> Result<()> {
    let mut off = start;
    loop {
        if off + 4 > area.len() {
            return Err(Error::corrupt("xattr entries run past their area"));
        }
        if le32(area, off) == 0 {
            return Ok(());
        }
        if off + ENTRY_HEADER > area.len() {
            return Err(Error::corrupt("xattr entry header truncated"));
        }
        let name_len = area[off] as usize;
        let index = area[off + 1];
        let value_offs = le16(area, off + 2) as usize;
        let value_inum = le32(area, off + 4);
        let value_size = le32(area, off + 8) as usize;
        let hash = le32(area, off + 12);
        if off + ENTRY_HEADER + name_len > area.len() {
            return Err(Error::corrupt("xattr name truncated"));
        }
        let name = area[off + ENTRY_HEADER..off + ENTRY_HEADER + name_len].to_vec();
        let value = if value_inum != 0 {
            ValueRef::Inode(value_inum, value_size)
        } else {
            let vs = value_base + value_offs;
            if vs + value_size > area.len() {
                return Err(Error::corrupt("xattr value out of bounds"));
            }
            ValueRef::Inline(area[vs..vs + value_size].to_vec())
        };
        out.push(RawXattr {
            index,
            name,
            value,
            hash,
        });
        off += entry_size(name_len);
    }
}

#[derive(Clone, Debug)]
enum ValueRef {
    Inline(Vec<u8>),
    /// Value stored in an EA inode (`ea_inode` feature), with its size.
    Inode(u32, usize),
}

#[derive(Clone, Debug)]
struct RawXattr {
    index: u8,
    name: Vec<u8>,
    value: ValueRef,
    #[allow(dead_code)]
    hash: u32,
}

impl Volume {
    fn ibody_area(&self, inode: &Inode) -> Option<(usize, usize)> {
        let isz = self.sb.inode_size() as usize;
        if isz <= GOOD_OLD_INODE_SIZE {
            return None;
        }
        let start = GOOD_OLD_INODE_SIZE + inode.extra_isize() as usize;
        if start + 4 > isz {
            return None;
        }
        Some((start, isz))
    }

    fn xattr_block_checksum(&self, blk: u64, b: &[u8]) -> u32 {
        let c = crc32c(self.sb.csum_seed(), &blk.to_le_bytes());
        let c = crc32c(c, &b[..0x10]);
        let c = crc32c(c, &[0; 4]);
        crc32c(c, &b[0x14..])
    }

    fn read_raw_xattrs(&self, ino: u32, inode: &Inode) -> Result<Vec<RawXattr>> {
        let mut out = Vec::new();
        if let Some((start, end)) = self.ibody_area(inode) {
            let area = &inode.raw[..end];
            if le32(area, start) == XATTR_MAGIC {
                parse_area(area, start + 4, start + 4, &mut out)?;
            }
        }
        let blk = inode.file_acl();
        if blk != 0 {
            let b = self.read_block(blk)?;
            if le32(&b, 0) != XATTR_MAGIC || le32(&b, 8) != 1 {
                return Err(Error::corrupt(format!(
                    "inode {ino} xattr block {blk} has a bad header"
                )));
            }
            if self.sb.has_metadata_csum()
                && !self.ignore_checksums
                && self.xattr_block_checksum(blk, &b) != le32(&b, 0x10)
            {
                return Err(Error::corrupt(format!(
                    "xattr block {blk} checksum mismatch"
                )));
            }
            parse_area(&b, BLOCK_HEADER, 0, &mut out)?;
        }
        Ok(out)
    }

    /// All extended attributes of an inode, values included.
    pub fn read_xattrs(&self, ino: u32, inode: &Inode) -> Result<Vec<Xattr>> {
        let raw = self.read_raw_xattrs(ino, inode)?;
        let mut out = Vec::with_capacity(raw.len());
        for r in raw {
            let value = match r.value {
                ValueRef::Inline(v) => v,
                ValueRef::Inode(vino, size) => {
                    let vinode = self.read_inode(vino)?;
                    let mut v = vec![0u8; size];
                    let n = self.read_file(vino, &vinode, 0, &mut v)?;
                    v.truncate(n);
                    v
                }
            };
            out.push(Xattr {
                index: r.index,
                name: r.name,
                value,
            });
        }
        Ok(out)
    }

    pub fn get_xattr(
        &self,
        ino: u32,
        inode: &Inode,
        index: u8,
        name: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .read_xattrs(ino, inode)?
            .into_iter()
            .find(|x| x.index == index && x.name == name)
            .map(|x| x.value))
    }

    /// Replace all attributes of the inode with `attrs`. The caller writes
    /// the inode afterwards. Attributes with index `INDEX_SYSTEM` named
    /// `data` (inline file data) always stay in the inode body.
    pub(crate) fn store_xattrs(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        attrs: &[Xattr],
    ) -> Result<()> {
        self.ensure_writable()?;
        if !attrs.is_empty() && !self.sb.has_compat(compat::EXT_ATTR) {
            self.sb
                .set_feature_compat(self.sb.feature_compat() | compat::EXT_ATTR);
            self.mark_sb_dirty();
        }
        for a in attrs {
            if a.name.len() > 255 {
                return Err(Error::NameTooLong);
            }
        }
        // Place attributes: inline data first, then small ones, in the body.
        let mut order: Vec<&Xattr> = attrs.iter().collect();
        order.sort_by_key(|a| {
            (
                !(a.index == INDEX_SYSTEM && a.name == b"data"),
                entry_size(a.name.len()) + value_size(a.value.len()),
            )
        });
        let mut body: Vec<&Xattr> = Vec::new();
        let mut block: Vec<&Xattr> = Vec::new();
        let body_space = self.ibody_area(inode).map_or(0, |(s, e)| e - s - 4 - 4);
        let mut used = 0;
        for a in order {
            let need = entry_size(a.name.len()) + value_size(a.value.len());
            if used + need <= body_space && block.is_empty() {
                body.push(a);
                used += need;
            } else if a.index == INDEX_SYSTEM && a.name == b"data" {
                return Err(Error::NoSpace);
            } else {
                block.push(a);
            }
        }

        // In-inode area.
        if let Some((start, end)) = self.ibody_area(inode) {
            let area = &mut inode.raw[start..end];
            area.fill(0);
            if !body.is_empty() {
                put32(area, 0, XATTR_MAGIC);
                let base = 4;
                let mut eoff = base;
                let mut vend = area.len();
                for a in &body {
                    let vlen = value_size(a.value.len());
                    vend -= vlen;
                    area[vend..vend + a.value.len()].copy_from_slice(&a.value);
                    write_entry(
                        area,
                        eoff,
                        a,
                        (vend - base) as u16,
                        entry_hash(&a.name, &a.value),
                    );
                    eoff += entry_size(a.name.len());
                }
            }
        }

        // External block.
        let old = inode.file_acl();
        if block.is_empty() {
            if old != 0 {
                self.release_xattr_block(old)?;
                inode.set_file_acl(0);
                self.sub_inode_blocks(inode, 1);
            }
            return Ok(());
        }
        let bs = self.block_size;
        block.sort_by(|a, b| {
            (a.index, a.name.len(), &a.name).cmp(&(b.index, b.name.len(), &b.name))
        });
        let total: usize = BLOCK_HEADER
            + 4
            + block
                .iter()
                .map(|a| entry_size(a.name.len()) + value_size(a.value.len()))
                .sum::<usize>();
        if total > bs {
            return Err(Error::NoSpace);
        }
        let blk = if old != 0 && self.with_block(old, |b| le32(b, 4))? == 1 {
            old
        } else {
            if old != 0 {
                self.release_xattr_block(old)?;
                inode.set_file_acl(0);
                self.sub_inode_blocks(inode, 1);
            }
            let goal = self.inode_goal_block(ino);
            let nb = self.alloc_block(goal)?;
            inode.set_file_acl(nb);
            self.add_inode_blocks(inode, 1)?;
            nb
        };
        let mut buf = vec![0u8; bs];
        put32(&mut buf, 0, XATTR_MAGIC);
        put32(&mut buf, 4, 1);
        put32(&mut buf, 8, 1);
        let mut eoff = BLOCK_HEADER;
        let mut vend = bs;
        let mut block_hash: u32 = 0;
        for a in &block {
            let vlen = value_size(a.value.len());
            vend -= vlen;
            buf[vend..vend + a.value.len()].copy_from_slice(&a.value);
            let h = entry_hash(&a.name, &a.value);
            write_entry(&mut buf, eoff, a, vend as u16, h);
            eoff += entry_size(a.name.len());
            block_hash = (block_hash << 16) ^ (block_hash >> 16) ^ h;
        }
        put32(&mut buf, 0xC, block_hash);
        if self.sb.has_metadata_csum() {
            let c = self.xattr_block_checksum(blk, &buf);
            put32(&mut buf, 0x10, c);
        }
        if blk == old {
            self.block_mut(blk)?.copy_from_slice(&buf);
        } else {
            self.new_block_mut(blk)?.copy_from_slice(&buf);
        }
        Ok(())
    }

    /// Drop one reference to an xattr block, freeing it at zero.
    pub(crate) fn release_xattr_block(&mut self, blk: u64) -> Result<()> {
        let refs = self.with_block(blk, |b| le32(b, 4))?;
        if refs <= 1 {
            self.free_blocks(blk, 1)?;
            self.forget_block(blk);
        } else {
            let csum = self.sb.has_metadata_csum();
            let mut b = self.block_mut(blk)?.clone();
            put32(&mut b, 4, refs - 1);
            if csum {
                let c = self.xattr_block_checksum(blk, &b);
                put32(&mut b, 0x10, c);
            }
            self.block_mut(blk)?.copy_from_slice(&b);
        }
        Ok(())
    }
}

fn write_entry(area: &mut [u8], off: usize, a: &Xattr, value_offs: u16, hash: u32) {
    area[off] = a.name.len() as u8;
    area[off + 1] = a.index;
    put16(area, off + 2, value_offs);
    put32(area, off + 4, 0);
    put32(area, off + 8, a.value.len() as u32);
    put32(area, off + 12, hash);
    area[off + ENTRY_HEADER..off + ENTRY_HEADER + a.name.len()].copy_from_slice(&a.name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_names() {
        assert_eq!(split_name(b"user.foo"), Some((INDEX_USER, &b"foo"[..])));
        assert_eq!(
            split_name(b"system.posix_acl_access"),
            Some((INDEX_POSIX_ACL_ACCESS, &b""[..]))
        );
        assert_eq!(
            split_name(b"system.data"),
            Some((INDEX_SYSTEM, &b"data"[..]))
        );
        assert_eq!(split_name(b"user."), None);
        assert_eq!(split_name(b"other.x"), None);
    }
}
