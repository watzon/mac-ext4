//! The ext2/3/4 superblock (1024 bytes at byte offset 1024).

use crate::bytes::*;
use crate::crc::crc32c;
use crate::error::{Error, Result};

pub const SUPERBLOCK_OFFSET: u64 = 1024;
pub const SUPERBLOCK_SIZE: usize = 1024;
pub const EXT4_MAGIC: u16 = 0xEF53;

pub mod compat {
    pub const DIR_PREALLOC: u32 = 0x0001;
    pub const IMAGIC_INODES: u32 = 0x0002;
    pub const HAS_JOURNAL: u32 = 0x0004;
    pub const EXT_ATTR: u32 = 0x0008;
    pub const RESIZE_INODE: u32 = 0x0010;
    pub const DIR_INDEX: u32 = 0x0020;
    pub const LAZY_BG: u32 = 0x0040;
    pub const EXCLUDE_BITMAP: u32 = 0x0100;
    pub const SPARSE_SUPER2: u32 = 0x0200;
    pub const FAST_COMMIT: u32 = 0x0400;
    pub const STABLE_INODES: u32 = 0x0800;
    pub const ORPHAN_FILE: u32 = 0x1000;
}

pub mod incompat {
    pub const COMPRESSION: u32 = 0x0001;
    pub const FILETYPE: u32 = 0x0002;
    pub const RECOVER: u32 = 0x0004;
    pub const JOURNAL_DEV: u32 = 0x0008;
    pub const META_BG: u32 = 0x0010;
    pub const EXTENTS: u32 = 0x0040;
    pub const BIT64: u32 = 0x0080;
    pub const MMP: u32 = 0x0100;
    pub const FLEX_BG: u32 = 0x0200;
    pub const EA_INODE: u32 = 0x0400;
    pub const DIRDATA: u32 = 0x1000;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x10000;
    pub const CASEFOLD: u32 = 0x20000;
}

pub mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x0001;
    pub const LARGE_FILE: u32 = 0x0002;
    pub const BTREE_DIR: u32 = 0x0004;
    pub const HUGE_FILE: u32 = 0x0008;
    pub const GDT_CSUM: u32 = 0x0010;
    pub const DIR_NLINK: u32 = 0x0020;
    pub const EXTRA_ISIZE: u32 = 0x0040;
    pub const HAS_SNAPSHOT: u32 = 0x0080;
    pub const QUOTA: u32 = 0x0100;
    pub const BIGALLOC: u32 = 0x0200;
    pub const METADATA_CSUM: u32 = 0x0400;
    pub const REPLICA: u32 = 0x0800;
    pub const READONLY: u32 = 0x1000;
    pub const PROJECT: u32 = 0x2000;
    pub const SHARED_BLOCKS: u32 = 0x4000;
    pub const VERITY: u32 = 0x8000;
    pub const ORPHAN_PRESENT: u32 = 0x10000;
}

/// `s_state` bits.
pub const STATE_VALID: u16 = 0x0001;
pub const STATE_ERROR: u16 = 0x0002;
pub const STATE_ORPHAN_FS: u16 = 0x0004;

/// `s_flags` bits.
pub const FLAG_SIGNED_HASH: u32 = 0x0001;
pub const FLAG_UNSIGNED_HASH: u32 = 0x0002;
pub const FLAG_TEST_FS: u32 = 0x0004;

/// Features this engine can read.
pub const READ_INCOMPAT: u32 = incompat::FILETYPE
    | incompat::RECOVER
    | incompat::META_BG
    | incompat::EXTENTS
    | incompat::BIT64
    | incompat::MMP
    | incompat::FLEX_BG
    | incompat::CSUM_SEED
    | incompat::LARGEDIR
    | incompat::INLINE_DATA
    | incompat::EA_INODE
    | incompat::ENCRYPT
    | incompat::CASEFOLD;

/// Features this engine can write. Volumes with other incompat or ro_compat
/// features mount read-only.
pub const WRITE_INCOMPAT: u32 = incompat::FILETYPE
    | incompat::RECOVER
    | incompat::META_BG
    | incompat::EXTENTS
    | incompat::BIT64
    | incompat::FLEX_BG
    | incompat::CSUM_SEED
    | incompat::LARGEDIR
    | incompat::INLINE_DATA;

pub const WRITE_RO_COMPAT: u32 = ro_compat::SPARSE_SUPER
    | ro_compat::LARGE_FILE
    | ro_compat::HUGE_FILE
    | ro_compat::GDT_CSUM
    | ro_compat::DIR_NLINK
    | ro_compat::EXTRA_ISIZE
    | ro_compat::METADATA_CSUM
    | ro_compat::ORPHAN_PRESENT;

macro_rules! field {
    ($get:ident, $set:ident, u8, $off:expr) => {
        pub fn $get(&self) -> u8 {
            self.raw[$off]
        }
        pub fn $set(&mut self, v: u8) {
            self.raw[$off] = v
        }
    };
    ($get:ident, $set:ident, u16, $off:expr) => {
        pub fn $get(&self) -> u16 {
            le16(&self.raw, $off)
        }
        pub fn $set(&mut self, v: u16) {
            put16(&mut self.raw, $off, v)
        }
    };
    ($get:ident, $set:ident, u32, $off:expr) => {
        pub fn $get(&self) -> u32 {
            le32(&self.raw, $off)
        }
        pub fn $set(&mut self, v: u32) {
            put32(&mut self.raw, $off, v)
        }
    };
    ($get:ident, $set:ident, u64, $off:expr) => {
        pub fn $get(&self) -> u64 {
            le64(&self.raw, $off)
        }
        pub fn $set(&mut self, v: u64) {
            put64(&mut self.raw, $off, v)
        }
    };
}

#[derive(Clone)]
pub struct Superblock {
    pub raw: [u8; SUPERBLOCK_SIZE],
}

impl std::fmt::Debug for Superblock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Superblock")
            .field("blocks", &self.blocks_count())
            .field("inodes", &self.inodes_count())
            .field("block_size", &self.block_size())
            .field("compat", &format_args!("{:#x}", self.feature_compat()))
            .field("incompat", &format_args!("{:#x}", self.feature_incompat()))
            .field(
                "ro_compat",
                &format_args!("{:#x}", self.feature_ro_compat()),
            )
            .finish()
    }
}

impl Superblock {
    field!(inodes_count, set_inodes_count, u32, 0x0);
    field!(blocks_count_lo, set_blocks_count_lo, u32, 0x4);
    field!(r_blocks_count_lo, set_r_blocks_count_lo, u32, 0x8);
    field!(free_blocks_count_lo, set_free_blocks_count_lo, u32, 0xC);
    field!(free_inodes_count, set_free_inodes_count, u32, 0x10);
    field!(first_data_block, set_first_data_block, u32, 0x14);
    field!(log_block_size, set_log_block_size, u32, 0x18);
    field!(log_cluster_size, set_log_cluster_size, u32, 0x1C);
    field!(blocks_per_group, set_blocks_per_group, u32, 0x20);
    field!(clusters_per_group, set_clusters_per_group, u32, 0x24);
    field!(inodes_per_group, set_inodes_per_group, u32, 0x28);
    field!(mtime, set_mtime, u32, 0x2C);
    field!(wtime, set_wtime, u32, 0x30);
    field!(mnt_count, set_mnt_count, u16, 0x34);
    field!(max_mnt_count, set_max_mnt_count, u16, 0x36);
    field!(magic, set_magic, u16, 0x38);
    field!(state, set_state, u16, 0x3A);
    field!(errors, set_errors, u16, 0x3C);
    field!(minor_rev_level, set_minor_rev_level, u16, 0x3E);
    field!(lastcheck, set_lastcheck, u32, 0x40);
    field!(checkinterval, set_checkinterval, u32, 0x44);
    field!(creator_os, set_creator_os, u32, 0x48);
    field!(rev_level, set_rev_level, u32, 0x4C);
    field!(def_resuid, set_def_resuid, u16, 0x50);
    field!(def_resgid, set_def_resgid, u16, 0x52);
    field!(first_ino_raw, set_first_ino, u32, 0x54);
    field!(inode_size_raw, set_inode_size, u16, 0x58);
    field!(block_group_nr, set_block_group_nr, u16, 0x5A);
    field!(feature_compat, set_feature_compat, u32, 0x5C);
    field!(feature_incompat, set_feature_incompat, u32, 0x60);
    field!(feature_ro_compat, set_feature_ro_compat, u32, 0x64);
    field!(
        algorithm_usage_bitmap,
        set_algorithm_usage_bitmap,
        u32,
        0xC8
    );
    field!(reserved_gdt_blocks, set_reserved_gdt_blocks, u16, 0xCE);
    field!(journal_inum, set_journal_inum, u32, 0xE0);
    field!(journal_dev, set_journal_dev, u32, 0xE4);
    field!(last_orphan, set_last_orphan, u32, 0xE8);
    field!(def_hash_version, set_def_hash_version, u8, 0xFC);
    field!(jnl_backup_type, set_jnl_backup_type, u8, 0xFD);
    field!(desc_size_raw, set_desc_size, u16, 0xFE);
    field!(default_mount_opts, set_default_mount_opts, u32, 0x100);
    field!(first_meta_bg, set_first_meta_bg, u32, 0x104);
    field!(mkfs_time, set_mkfs_time, u32, 0x108);
    field!(blocks_count_hi, set_blocks_count_hi, u32, 0x150);
    field!(r_blocks_count_hi, set_r_blocks_count_hi, u32, 0x154);
    field!(free_blocks_count_hi, set_free_blocks_count_hi, u32, 0x158);
    field!(min_extra_isize, set_min_extra_isize, u16, 0x15C);
    field!(want_extra_isize, set_want_extra_isize, u16, 0x15E);
    field!(flags, set_flags, u32, 0x160);
    field!(raid_stride, set_raid_stride, u16, 0x164);
    field!(mmp_interval, set_mmp_interval, u16, 0x166);
    field!(mmp_block, set_mmp_block, u64, 0x168);
    field!(raid_stripe_width, set_raid_stripe_width, u32, 0x170);
    field!(log_groups_per_flex, set_log_groups_per_flex, u8, 0x174);
    field!(checksum_type, set_checksum_type, u8, 0x175);
    field!(kbytes_written, set_kbytes_written, u64, 0x178);
    field!(error_count, set_error_count, u32, 0x194);
    field!(usr_quota_inum, set_usr_quota_inum, u32, 0x240);
    field!(grp_quota_inum, set_grp_quota_inum, u32, 0x244);
    field!(overhead_clusters, set_overhead_clusters, u32, 0x248);
    field!(backup_bg0, set_backup_bg0, u32, 0x24C);
    field!(backup_bg1, set_backup_bg1, u32, 0x250);
    field!(lpf_ino, set_lpf_ino, u32, 0x268);
    field!(prj_quota_inum, set_prj_quota_inum, u32, 0x26C);
    field!(checksum_seed_raw, set_checksum_seed, u32, 0x270);
    field!(wtime_hi, set_wtime_hi, u8, 0x274);
    field!(mtime_hi, set_mtime_hi, u8, 0x275);
    field!(mkfs_time_hi, set_mkfs_time_hi, u8, 0x276);
    field!(lastcheck_hi, set_lastcheck_hi, u8, 0x277);
    field!(encoding, set_encoding, u16, 0x27C);
    field!(encoding_flags, set_encoding_flags, u16, 0x27E);
    field!(orphan_file_inum, set_orphan_file_inum, u32, 0x280);
    field!(checksum, set_checksum, u32, 0x3FC);

    pub fn zeroed() -> Self {
        Superblock {
            raw: [0; SUPERBLOCK_SIZE],
        }
    }

    /// Parse and validate a superblock from its 1024 raw bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < SUPERBLOCK_SIZE {
            return Err(Error::corrupt("superblock is truncated"));
        }
        let mut raw = [0u8; SUPERBLOCK_SIZE];
        raw.copy_from_slice(&bytes[..SUPERBLOCK_SIZE]);
        let sb = Superblock { raw };
        sb.validate()?;
        Ok(sb)
    }

    fn validate(&self) -> Result<()> {
        if self.magic() != EXT4_MAGIC {
            return Err(Error::corrupt(
                "bad superblock magic (not an ext2/3/4 file system)",
            ));
        }
        if self.log_block_size() > 6 {
            return Err(Error::corrupt("block size is larger than 64 KiB"));
        }
        if self.has_ro_compat(ro_compat::BIGALLOC)
            && (self.log_cluster_size() < self.log_block_size() || self.log_cluster_size() > 29)
        {
            return Err(Error::corrupt("invalid cluster size"));
        }
        if self.blocks_per_group() == 0
            || self.inodes_per_group() == 0
            || self.clusters_per_group() == 0
        {
            return Err(Error::corrupt("zero blocks or inodes per group"));
        }
        if self.blocks_per_group() > 8 * self.block_size() * self.blocks_per_cluster() {
            return Err(Error::corrupt("blocks per group exceeds bitmap capacity"));
        }
        if self.inodes_per_group() > 8 * self.block_size() {
            return Err(Error::corrupt("inodes per group exceeds bitmap capacity"));
        }
        let isz = self.inode_size() as u32;
        if isz < 128 || !isz.is_power_of_two() || isz > self.block_size() {
            return Err(Error::corrupt("invalid inode size"));
        }
        if self.is_64bit() {
            let ds = self.desc_size() as u32;
            if !(64..=1024).contains(&ds) || !ds.is_power_of_two() {
                return Err(Error::corrupt("invalid group descriptor size"));
            }
        }
        if self.first_data_block() as u64 >= self.blocks_count() {
            return Err(Error::corrupt(
                "first data block is past the end of the file system",
            ));
        }
        let groups = self.group_count();
        if groups == 0
            || (groups as u64 * self.inodes_per_group() as u64) < self.inodes_count() as u64
        {
            return Err(Error::corrupt("inode count does not match group geometry"));
        }
        Ok(())
    }

    pub fn block_size(&self) -> u32 {
        1024 << self.log_block_size()
    }

    pub fn cluster_size(&self) -> u32 {
        if self.has_ro_compat(ro_compat::BIGALLOC) {
            1024 << self.log_cluster_size()
        } else {
            self.block_size()
        }
    }

    /// Blocks per allocation cluster (1 unless `bigalloc`).
    pub fn blocks_per_cluster(&self) -> u32 {
        self.cluster_size() / self.block_size()
    }

    pub fn blocks_count(&self) -> u64 {
        lohi(
            self.blocks_count_lo(),
            if self.is_64bit() {
                self.blocks_count_hi()
            } else {
                0
            },
        )
    }

    pub fn set_blocks_count(&mut self, v: u64) {
        self.set_blocks_count_lo(v as u32);
        if self.is_64bit() {
            self.set_blocks_count_hi((v >> 32) as u32);
        }
    }

    pub fn r_blocks_count(&self) -> u64 {
        lohi(
            self.r_blocks_count_lo(),
            if self.is_64bit() {
                self.r_blocks_count_hi()
            } else {
                0
            },
        )
    }

    pub fn set_r_blocks_count(&mut self, v: u64) {
        self.set_r_blocks_count_lo(v as u32);
        if self.is_64bit() {
            self.set_r_blocks_count_hi((v >> 32) as u32);
        }
    }

    pub fn free_blocks_count(&self) -> u64 {
        lohi(
            self.free_blocks_count_lo(),
            if self.is_64bit() {
                self.free_blocks_count_hi()
            } else {
                0
            },
        )
    }

    pub fn set_free_blocks_count(&mut self, v: u64) {
        self.set_free_blocks_count_lo(v as u32);
        if self.is_64bit() {
            self.set_free_blocks_count_hi((v >> 32) as u32);
        }
    }

    pub fn inode_size(&self) -> u16 {
        if self.rev_level() == 0 {
            128
        } else {
            self.inode_size_raw()
        }
    }

    pub fn first_ino(&self) -> u32 {
        if self.rev_level() == 0 {
            11
        } else {
            self.first_ino_raw()
        }
    }

    pub fn desc_size(&self) -> u16 {
        if self.is_64bit() {
            self.desc_size_raw()
        } else {
            32
        }
    }

    pub fn group_count(&self) -> u32 {
        let data = self.blocks_count() - self.first_data_block() as u64;
        data.div_ceil(self.blocks_per_group() as u64) as u32
    }

    pub fn uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[0x68..0x78]);
        u
    }

    pub fn set_uuid(&mut self, u: [u8; 16]) {
        self.raw[0x68..0x78].copy_from_slice(&u);
    }

    pub fn journal_uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[0xD0..0xE0]);
        u
    }

    /// Volume label, trimmed at the first NUL.
    pub fn volume_name(&self) -> &[u8] {
        let raw = &self.raw[0x78..0x88];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        &raw[..end]
    }

    pub fn set_volume_name(&mut self, name: &[u8]) {
        let n = name.len().min(16);
        self.raw[0x78..0x88].fill(0);
        self.raw[0x78..0x78 + n].copy_from_slice(&name[..n]);
    }

    pub fn set_last_mounted(&mut self, path: &[u8]) {
        let n = path.len().min(64);
        self.raw[0x88..0xC8].fill(0);
        self.raw[0x88..0x88 + n].copy_from_slice(&path[..n]);
    }

    pub fn hash_seed(&self) -> [u32; 4] {
        [
            le32(&self.raw, 0xEC),
            le32(&self.raw, 0xF0),
            le32(&self.raw, 0xF4),
            le32(&self.raw, 0xF8),
        ]
    }

    pub fn set_hash_seed(&mut self, seed: [u32; 4]) {
        for (i, w) in seed.iter().enumerate() {
            put32(&mut self.raw, 0xEC + 4 * i, *w);
        }
    }

    /// `s_jnl_blocks`: backup of the journal inode's `i_block` plus size.
    pub fn jnl_blocks(&self) -> [u32; 17] {
        let mut out = [0u32; 17];
        for (i, v) in out.iter_mut().enumerate() {
            *v = le32(&self.raw, 0x10C + 4 * i);
        }
        out
    }

    pub fn set_jnl_blocks(&mut self, v: &[u32; 17]) {
        for (i, w) in v.iter().enumerate() {
            put32(&mut self.raw, 0x10C + 4 * i, *w);
        }
    }

    pub fn has_compat(&self, f: u32) -> bool {
        self.feature_compat() & f != 0
    }
    pub fn has_incompat(&self, f: u32) -> bool {
        self.feature_incompat() & f != 0
    }
    pub fn has_ro_compat(&self, f: u32) -> bool {
        self.feature_ro_compat() & f != 0
    }
    pub fn is_64bit(&self) -> bool {
        self.has_incompat(incompat::BIT64)
    }
    pub fn has_metadata_csum(&self) -> bool {
        self.has_ro_compat(ro_compat::METADATA_CSUM)
    }
    /// Group descriptors carry a checksum (crc16 or crc32c).
    pub fn has_group_csum(&self) -> bool {
        self.has_metadata_csum() || self.has_ro_compat(ro_compat::GDT_CSUM)
    }

    /// Seed for all metadata_csum checksums.
    pub fn csum_seed(&self) -> u32 {
        if self.has_incompat(incompat::CSUM_SEED) {
            self.checksum_seed_raw()
        } else {
            crc32c(!0, &self.raw[0x68..0x78])
        }
    }

    pub fn compute_checksum(&self) -> u32 {
        crc32c(!0, &self.raw[..0x3FC])
    }

    pub fn verify_checksum(&self) -> bool {
        !self.has_metadata_csum()
            || self.checksum_type() != 1
            || self.compute_checksum() == self.checksum()
    }

    /// Recompute `s_checksum` if `metadata_csum` is enabled.
    pub fn update_checksum(&mut self) {
        if self.has_metadata_csum() {
            let c = self.compute_checksum();
            self.set_checksum(c);
        }
    }

    /// True when group `g` holds a superblock and descriptor backup.
    pub fn group_has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if self.has_compat(compat::SPARSE_SUPER2) {
            return g == self.backup_bg0() || g == self.backup_bg1();
        }
        if g <= 1 || !self.has_ro_compat(ro_compat::SPARSE_SUPER) {
            return true;
        }
        if g & 1 == 0 {
            return false;
        }
        is_power_of(g, 3) || is_power_of(g, 5) || is_power_of(g, 7)
    }

    /// Feature bits present that this engine cannot read.
    pub fn unknown_read_features(&self) -> u32 {
        self.feature_incompat() & !READ_INCOMPAT
    }

    /// Reason why the volume must be mounted read-only, if any.
    pub fn read_only_reason(&self) -> Option<String> {
        let inc = self.feature_incompat() & !WRITE_INCOMPAT;
        if inc != 0 {
            return Some(format!(
                "incompatible features {inc:#x} are not supported for writing"
            ));
        }
        let ro = self.feature_ro_compat() & !WRITE_RO_COMPAT;
        if ro != 0 {
            return Some(format!(
                "read-only-compatible features {ro:#x} are not supported for writing"
            ));
        }
        if self.has_compat(compat::FAST_COMMIT) {
            return Some("fast_commit journals are not supported for writing".into());
        }
        if self.has_ro_compat(ro_compat::METADATA_CSUM) && self.has_ro_compat(ro_compat::GDT_CSUM) {
            return Some("metadata_csum and gdt_csum are both set".into());
        }
        None
    }
}

fn is_power_of(mut n: u32, base: u32) -> bool {
    while n % base == 0 {
        n /= base;
    }
    n == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_super_backup_groups() {
        let mut sb = Superblock::zeroed();
        sb.set_feature_ro_compat(ro_compat::SPARSE_SUPER);
        let backups: Vec<u32> = (0..100).filter(|&g| sb.group_has_super(g)).collect();
        assert_eq!(backups, vec![0, 1, 3, 5, 7, 9, 25, 27, 49, 81]);
    }
}
