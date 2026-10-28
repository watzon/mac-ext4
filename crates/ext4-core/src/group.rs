//! Block group descriptors (32 or `s_desc_size` bytes each).

use crate::bytes::*;
use crate::crc::{crc16, crc32c};
use crate::superblock::Superblock;

pub const BG_INODE_UNINIT: u16 = 0x0001;
pub const BG_BLOCK_UNINIT: u16 = 0x0002;
pub const BG_INODE_ZEROED: u16 = 0x0004;

const CHECKSUM_OFF: usize = 0x1E;

/// One group descriptor, stored as its raw on-disk bytes.
#[derive(Clone, Debug)]
pub struct GroupDesc {
    pub raw: Vec<u8>,
}

macro_rules! split_field {
    ($get:ident, $set:ident, u32, $lo:expr, $hi:expr) => {
        pub fn $get(&self) -> u64 {
            let hi = if self.raw.len() >= 64 {
                le32(&self.raw, $hi)
            } else {
                0
            };
            lohi(le32(&self.raw, $lo), hi)
        }
        pub fn $set(&mut self, v: u64) {
            put32(&mut self.raw, $lo, v as u32);
            if self.raw.len() >= 64 {
                put32(&mut self.raw, $hi, (v >> 32) as u32);
            }
        }
    };
    ($get:ident, $set:ident, u16, $lo:expr, $hi:expr) => {
        pub fn $get(&self) -> u32 {
            let hi = if self.raw.len() >= 64 {
                le16(&self.raw, $hi)
            } else {
                0
            };
            (le16(&self.raw, $lo) as u32) | ((hi as u32) << 16)
        }
        pub fn $set(&mut self, v: u32) {
            put16(&mut self.raw, $lo, v as u16);
            if self.raw.len() >= 64 {
                put16(&mut self.raw, $hi, (v >> 16) as u16);
            }
        }
    };
}

impl GroupDesc {
    pub fn new(desc_size: usize) -> Self {
        GroupDesc {
            raw: vec![0; desc_size],
        }
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        GroupDesc { raw: b.to_vec() }
    }

    split_field!(block_bitmap, set_block_bitmap, u32, 0x0, 0x20);
    split_field!(inode_bitmap, set_inode_bitmap, u32, 0x4, 0x24);
    split_field!(inode_table, set_inode_table, u32, 0x8, 0x28);
    split_field!(exclude_bitmap, set_exclude_bitmap, u32, 0x14, 0x34);
    split_field!(free_blocks_count, set_free_blocks_count, u16, 0xC, 0x2C);
    split_field!(free_inodes_count, set_free_inodes_count, u16, 0xE, 0x2E);
    split_field!(used_dirs_count, set_used_dirs_count, u16, 0x10, 0x30);
    split_field!(itable_unused, set_itable_unused, u16, 0x1C, 0x32);
    split_field!(block_bitmap_csum, set_block_bitmap_csum, u16, 0x18, 0x38);
    split_field!(inode_bitmap_csum, set_inode_bitmap_csum, u16, 0x1A, 0x3A);

    pub fn flags(&self) -> u16 {
        le16(&self.raw, 0x12)
    }
    pub fn set_flags(&mut self, v: u16) {
        put16(&mut self.raw, 0x12, v)
    }
    pub fn has_flag(&self, f: u16) -> bool {
        self.flags() & f != 0
    }
    pub fn checksum(&self) -> u16 {
        le16(&self.raw, CHECKSUM_OFF)
    }

    /// Compute the descriptor checksum for group `group`, or `None` if the
    /// file system does not checksum group descriptors.
    pub fn compute_checksum(&self, sb: &Superblock, group: u32) -> Option<u16> {
        let size = sb.desc_size() as usize;
        let raw = &self.raw[..size.min(self.raw.len())];
        if sb.has_metadata_csum() {
            let mut c = crc32c(sb.csum_seed(), &group.to_le_bytes());
            c = crc32c(c, &raw[..CHECKSUM_OFF]);
            c = crc32c(c, &[0, 0]);
            if raw.len() > CHECKSUM_OFF + 2 {
                c = crc32c(c, &raw[CHECKSUM_OFF + 2..]);
            }
            Some((c & 0xFFFF) as u16)
        } else if sb.has_ro_compat(crate::superblock::ro_compat::GDT_CSUM) {
            let mut c = crc16(!0, &sb.uuid());
            c = crc16(c, &group.to_le_bytes());
            c = crc16(c, &raw[..CHECKSUM_OFF]);
            if sb.is_64bit() && raw.len() > CHECKSUM_OFF + 2 {
                c = crc16(c, &raw[CHECKSUM_OFF + 2..]);
            }
            Some(c)
        } else {
            None
        }
    }

    pub fn verify_checksum(&self, sb: &Superblock, group: u32) -> bool {
        match self.compute_checksum(sb, group) {
            Some(c) => c == self.checksum(),
            None => true,
        }
    }

    pub fn update_checksum(&mut self, sb: &Superblock, group: u32) {
        if let Some(c) = self.compute_checksum(sb, group) {
            put16(&mut self.raw, CHECKSUM_OFF, c);
        }
    }
}

/// Bitmap checksum as stored in the descriptor (`lo` 16 bits, plus `hi`
/// 16 bits when the descriptor is 64 bytes or larger).
pub fn bitmap_checksum(sb: &Superblock, bitmap: &[u8]) -> u32 {
    crc32c(sb.csum_seed(), bitmap)
}

/// Compare a computed bitmap checksum with the stored value, honoring the
/// descriptor size (small descriptors only store the low 16 bits).
pub fn bitmap_csum_matches(stored: u32, computed: u32, desc_size: usize) -> bool {
    if desc_size >= 64 {
        stored == computed
    } else {
        stored & 0xFFFF == computed & 0xFFFF
    }
}
