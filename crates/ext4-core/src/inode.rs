//! On-disk inodes.

use crate::bytes::*;
use crate::crc::crc32c;
use crate::error::{Error, Result};
use crate::superblock::{ro_compat, Superblock};

pub const ROOT_INO: u32 = 2;
pub const BAD_BLOCKS_INO: u32 = 1;
pub const USR_QUOTA_INO: u32 = 3;
pub const GRP_QUOTA_INO: u32 = 4;
pub const BOOT_LOADER_INO: u32 = 5;
pub const UNDEL_DIR_INO: u32 = 6;
pub const RESIZE_INO: u32 = 7;
pub const JOURNAL_INO: u32 = 8;

pub const GOOD_OLD_INODE_SIZE: usize = 128;
pub const N_BLOCKS: usize = 15;
pub const I_BLOCK_OFF: usize = 0x28;
pub const I_BLOCK_LEN: usize = 60;

// i_mode file type bits
pub const S_IFMT: u16 = 0o170000;
pub const S_IFSOCK: u16 = 0o140000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFIFO: u16 = 0o010000;

// i_flags
pub const FL_SECRM: u32 = 0x1;
pub const FL_UNRM: u32 = 0x2;
pub const FL_COMPR: u32 = 0x4;
pub const FL_SYNC: u32 = 0x8;
pub const FL_IMMUTABLE: u32 = 0x10;
pub const FL_APPEND: u32 = 0x20;
pub const FL_NODUMP: u32 = 0x40;
pub const FL_NOATIME: u32 = 0x80;
pub const FL_INDEX: u32 = 0x1000;
pub const FL_JOURNAL_DATA: u32 = 0x4000;
pub const FL_DIRSYNC: u32 = 0x10000;
pub const FL_TOPDIR: u32 = 0x20000;
pub const FL_HUGE_FILE: u32 = 0x40000;
pub const FL_EXTENTS: u32 = 0x80000;
pub const FL_VERITY: u32 = 0x100000;
pub const FL_EA_INODE: u32 = 0x200000;
pub const FL_INLINE_DATA: u32 = 0x10000000;
pub const FL_PROJINHERIT: u32 = 0x20000000;
pub const FL_CASEFOLD: u32 = 0x40000000;
pub const FL_ENCRYPT: u32 = 0x800;

/// The kind of file an inode describes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
    Unknown,
}

impl FileType {
    pub fn from_mode(mode: u16) -> Self {
        match mode & S_IFMT {
            S_IFREG => FileType::Regular,
            S_IFDIR => FileType::Directory,
            S_IFLNK => FileType::Symlink,
            S_IFCHR => FileType::CharDevice,
            S_IFBLK => FileType::BlockDevice,
            S_IFIFO => FileType::Fifo,
            S_IFSOCK => FileType::Socket,
            _ => FileType::Unknown,
        }
    }

    /// ext4 directory entry `file_type` byte.
    pub fn dirent_type(self) -> u8 {
        match self {
            FileType::Unknown => 0,
            FileType::Regular => 1,
            FileType::Directory => 2,
            FileType::CharDevice => 3,
            FileType::BlockDevice => 4,
            FileType::Fifo => 5,
            FileType::Socket => 6,
            FileType::Symlink => 7,
        }
    }

    pub fn from_dirent_type(t: u8) -> Self {
        match t {
            1 => FileType::Regular,
            2 => FileType::Directory,
            3 => FileType::CharDevice,
            4 => FileType::BlockDevice,
            5 => FileType::Fifo,
            6 => FileType::Socket,
            7 => FileType::Symlink,
            _ => FileType::Unknown,
        }
    }

    pub fn mode_bits(self) -> u16 {
        match self {
            FileType::Regular => S_IFREG,
            FileType::Directory => S_IFDIR,
            FileType::Symlink => S_IFLNK,
            FileType::CharDevice => S_IFCHR,
            FileType::BlockDevice => S_IFBLK,
            FileType::Fifo => S_IFIFO,
            FileType::Socket => S_IFSOCK,
            FileType::Unknown => 0,
        }
    }
}

/// Seconds + nanoseconds since the Unix epoch.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Timestamp {
    pub secs: i64,
    pub nsecs: u32,
}

impl Timestamp {
    pub fn now() -> Self {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp {
            secs: d.as_secs() as i64,
            nsecs: d.subsec_nanos(),
        }
    }
}

macro_rules! ifield {
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
}

/// An inode held as its raw `s_inode_size` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inode {
    pub raw: Vec<u8>,
}

impl Inode {
    pub fn new(inode_size: usize) -> Self {
        Inode {
            raw: vec![0; inode_size],
        }
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        Inode { raw: b.to_vec() }
    }

    ifield!(mode, set_mode, u16, 0x0);
    ifield!(uid_lo, set_uid_lo, u16, 0x2);
    ifield!(size_lo, set_size_lo, u32, 0x4);
    ifield!(atime_raw, set_atime_raw, u32, 0x8);
    ifield!(ctime_raw, set_ctime_raw, u32, 0xC);
    ifield!(mtime_raw, set_mtime_raw, u32, 0x10);
    ifield!(dtime, set_dtime, u32, 0x14);
    ifield!(gid_lo, set_gid_lo, u16, 0x18);
    ifield!(links_count, set_links_count, u16, 0x1A);
    ifield!(blocks_lo, set_blocks_lo, u32, 0x1C);
    ifield!(flags, set_flags, u32, 0x20);
    ifield!(version_lo, set_version_lo, u32, 0x24);
    ifield!(generation, set_generation, u32, 0x64);
    ifield!(file_acl_lo, set_file_acl_lo, u32, 0x68);
    ifield!(size_hi, set_size_hi, u32, 0x6C);
    ifield!(blocks_hi, set_blocks_hi, u16, 0x74);
    ifield!(file_acl_hi, set_file_acl_hi, u16, 0x76);
    ifield!(uid_hi, set_uid_hi, u16, 0x78);
    ifield!(gid_hi, set_gid_hi, u16, 0x7A);
    ifield!(checksum_lo, set_checksum_lo, u16, 0x7C);

    pub fn file_type(&self) -> FileType {
        FileType::from_mode(self.mode())
    }
    pub fn is_dir(&self) -> bool {
        self.file_type() == FileType::Directory
    }
    pub fn has_flag(&self, f: u32) -> bool {
        self.flags() & f != 0
    }

    pub fn size(&self) -> u64 {
        lohi(self.size_lo(), self.size_hi())
    }
    pub fn set_size(&mut self, v: u64) {
        self.set_size_lo(v as u32);
        self.set_size_hi((v >> 32) as u32);
    }

    pub fn uid(&self) -> u32 {
        self.uid_lo() as u32 | ((self.uid_hi() as u32) << 16)
    }
    pub fn set_uid(&mut self, v: u32) {
        self.set_uid_lo(v as u16);
        self.set_uid_hi((v >> 16) as u16);
    }
    pub fn gid(&self) -> u32 {
        self.gid_lo() as u32 | ((self.gid_hi() as u32) << 16)
    }
    pub fn set_gid(&mut self, v: u32) {
        self.set_gid_lo(v as u16);
        self.set_gid_hi((v >> 16) as u16);
    }

    pub fn file_acl(&self) -> u64 {
        lohi(self.file_acl_lo(), self.file_acl_hi() as u32)
    }
    pub fn set_file_acl(&mut self, v: u64) {
        self.set_file_acl_lo(v as u32);
        self.set_file_acl_hi((v >> 32) as u16);
    }

    /// Raw `i_blocks` value.  `i_blocks_hi` counts only with `huge_file`;
    /// without it the field is not part of the count.
    fn raw_blocks(&self, sb: &Superblock) -> u64 {
        if sb.has_ro_compat(ro_compat::HUGE_FILE) {
            self.blocks_lo() as u64 | ((self.blocks_hi() as u64) << 32)
        } else {
            self.blocks_lo() as u64
        }
    }

    /// Allocated space in bytes, decoding `i_blocks` per `huge_file` rules.
    pub fn allocated_bytes(&self, sb: &Superblock) -> u64 {
        self.blocks_512(sb) * 512
    }

    /// `i_blocks` as a count of 512-byte sectors.  With `FL_HUGE_FILE` the
    /// stored value counts filesystem blocks and is scaled here.
    pub fn blocks_512(&self, sb: &Superblock) -> u64 {
        let count = self.raw_blocks(sb);
        if sb.has_ro_compat(ro_compat::HUGE_FILE) && self.has_flag(FL_HUGE_FILE) {
            count * (sb.block_size() as u64 / 512)
        } else {
            count
        }
    }

    /// Store `i_blocks` given a count of 512-byte sectors.
    ///
    /// Without `huge_file` only a 32-bit sector count fits; a larger count
    /// is an error, never silently truncated.
    pub fn set_blocks_512(&mut self, sb: &Superblock, sectors: u64) -> Result<()> {
        let huge = sb.has_ro_compat(ro_compat::HUGE_FILE);
        let mut flags = self.flags() & !FL_HUGE_FILE;
        let value = if sectors <= u32::MAX as u64 {
            sectors
        } else if !huge {
            return Err(Error::FileTooLarge);
        } else if sectors <= 0xFFFF_FFFF_FFFF {
            sectors
        } else {
            flags |= FL_HUGE_FILE;
            sectors / (sb.block_size() as u64 / 512)
        };
        if value > 0xFFFF_FFFF_FFFF {
            return Err(Error::FileTooLarge);
        }
        self.set_flags(flags);
        self.set_blocks_lo(value as u32);
        if huge {
            self.set_blocks_hi((value >> 32) as u16);
        }
        Ok(())
    }

    pub fn i_block(&self) -> &[u8] {
        &self.raw[I_BLOCK_OFF..I_BLOCK_OFF + I_BLOCK_LEN]
    }
    pub fn i_block_mut(&mut self) -> &mut [u8] {
        &mut self.raw[I_BLOCK_OFF..I_BLOCK_OFF + I_BLOCK_LEN]
    }

    pub fn extra_isize(&self) -> u16 {
        if self.raw.len() > GOOD_OLD_INODE_SIZE {
            le16(&self.raw, 0x80)
        } else {
            0
        }
    }
    pub fn set_extra_isize(&mut self, v: u16) {
        if self.raw.len() > GOOD_OLD_INODE_SIZE {
            put16(&mut self.raw, 0x80, v)
        }
    }

    /// True if an extra field ending at `end` (absolute offset) is present.
    fn has_extra(&self, end: usize) -> bool {
        self.raw.len() > GOOD_OLD_INODE_SIZE
            && end <= GOOD_OLD_INODE_SIZE + self.extra_isize() as usize
    }

    fn decode_time(&self, base: usize, extra: usize) -> Timestamp {
        let secs = le32(&self.raw, base) as i32 as i64;
        if self.has_extra(extra + 4) {
            let e = le32(&self.raw, extra);
            let epoch = (e & 3) as i64;
            Timestamp {
                secs: secs + (epoch << 32),
                nsecs: e >> 2,
            }
        } else {
            Timestamp { secs, nsecs: 0 }
        }
    }

    fn encode_time(&mut self, base: usize, extra: usize, t: Timestamp) {
        put32(&mut self.raw, base, t.secs as u32);
        if self.has_extra(extra + 4) {
            // epoch bits extend the signed 32-bit seconds value.
            let epoch = ((t.secs - (t.secs as i32 as i64)) >> 32) as u32 & 3;
            put32(
                &mut self.raw,
                extra,
                (t.nsecs.min(999_999_999) << 2) | epoch,
            );
        }
    }

    pub fn atime(&self) -> Timestamp {
        self.decode_time(0x8, 0x8C)
    }
    pub fn ctime(&self) -> Timestamp {
        self.decode_time(0xC, 0x84)
    }
    pub fn mtime(&self) -> Timestamp {
        self.decode_time(0x10, 0x88)
    }
    pub fn crtime(&self) -> Option<Timestamp> {
        if self.has_extra(0x94) {
            Some(self.decode_time(0x90, 0x94))
        } else {
            None
        }
    }
    pub fn set_atime(&mut self, t: Timestamp) {
        self.encode_time(0x8, 0x8C, t)
    }
    pub fn set_ctime(&mut self, t: Timestamp) {
        self.encode_time(0xC, 0x84, t)
    }
    pub fn set_mtime(&mut self, t: Timestamp) {
        self.encode_time(0x10, 0x88, t)
    }
    pub fn set_crtime(&mut self, t: Timestamp) {
        if self.has_extra(0x94) {
            self.encode_time(0x90, 0x94, t)
        }
    }

    pub fn projid(&self) -> u32 {
        if self.has_extra(0xA0) {
            le32(&self.raw, 0x9C)
        } else {
            0
        }
    }

    /// Device number for character and block special files, encoded in
    /// `i_block` (old 16-bit form in word 0, new 32-bit form in word 1).
    pub fn rdev(&self) -> (u32, u32) {
        let b = self.i_block();
        let old = le32(b, 0);
        if old != 0 {
            ((old >> 8) & 0xFF, old & 0xFF)
        } else {
            let new = le32(b, 4);
            let major = (new & 0xFFF00) >> 8;
            let minor = (new & 0xFF) | ((new >> 12) & 0xFFF00);
            (major, minor)
        }
    }

    pub fn set_rdev(&mut self, major: u32, minor: u32) {
        let b = self.i_block_mut();
        b.fill(0);
        if major < 256 && minor < 256 {
            put32(b, 0, (major << 8) | minor);
        } else {
            let new = (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12);
            put32(b, 4, new);
        }
    }

    /// Per-inode checksum seed (metadata_csum).
    pub fn csum_seed(sb: &Superblock, ino: u32, generation: u32) -> u32 {
        let c = crc32c(sb.csum_seed(), &ino.to_le_bytes());
        crc32c(c, &generation.to_le_bytes())
    }

    fn has_csum_hi(&self) -> bool {
        self.has_extra(0x84)
    }

    pub fn compute_checksum(&self, sb: &Superblock, ino: u32) -> u32 {
        let mut c = Self::csum_seed(sb, ino, self.generation());
        c = crc32c(c, &self.raw[..0x7C]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &self.raw[0x7E..GOOD_OLD_INODE_SIZE]);
        if self.raw.len() > GOOD_OLD_INODE_SIZE {
            if self.has_csum_hi() {
                c = crc32c(c, &self.raw[GOOD_OLD_INODE_SIZE..0x82]);
                c = crc32c(c, &[0, 0]);
                c = crc32c(c, &self.raw[0x84..]);
            } else {
                c = crc32c(c, &self.raw[GOOD_OLD_INODE_SIZE..]);
            }
        }
        c
    }

    pub fn stored_checksum(&self) -> u32 {
        let lo = self.checksum_lo() as u32;
        if self.has_csum_hi() {
            lo | ((le16(&self.raw, 0x82) as u32) << 16)
        } else {
            lo
        }
    }

    pub fn verify_checksum(&self, sb: &Superblock, ino: u32) -> bool {
        if !sb.has_metadata_csum() {
            return true;
        }
        let c = self.compute_checksum(sb, ino);
        if self.has_csum_hi() {
            c == self.stored_checksum()
        } else {
            c & 0xFFFF == self.stored_checksum()
        }
    }

    pub fn update_checksum(&mut self, sb: &Superblock, ino: u32) {
        if !sb.has_metadata_csum() {
            return;
        }
        let c = self.compute_checksum(sb, ino);
        self.set_checksum_lo(c as u16);
        if self.has_csum_hi() {
            put16(&mut self.raw, 0x82, (c >> 16) as u16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_with_extra_fields() {
        let mut ino = Inode::new(256);
        ino.set_extra_isize(32);
        let t = Timestamp {
            secs: 5_000_000_000,
            nsecs: 123_456_789,
        };
        ino.set_mtime(t);
        assert_eq!(ino.mtime(), t);
        let neg = Timestamp {
            secs: -100,
            nsecs: 5,
        };
        ino.set_atime(neg);
        assert_eq!(ino.atime(), neg);
    }

    #[test]
    fn rdev_round_trip() {
        let mut ino = Inode::new(256);
        ino.set_rdev(8, 1);
        assert_eq!(ino.rdev(), (8, 1));
        ino.set_rdev(300, 70000);
        assert_eq!(ino.rdev(), (300, 70000));
    }
}
