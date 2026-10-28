//! ext4-core: a clean-room ext2/ext3/ext4 engine.
//!
//! Layers, bottom up:
//! - [`device`]: positioned block I/O.
//! - [`superblock`], [`group`], [`inode`]: on-disk structure views.
//! - [`volume`]: mounted state, metadata cache, transactions, journal.
//! - Format logic: block mapping, directories, xattrs, allocation.
//! - [`fs::Filesystem`]: the thread-safe API used by platform shims.
//! - [`mkfs`] and [`check`]: format and consistency tools.

pub mod alloc;
pub mod blockmap;
pub mod bmap;
pub mod bytes;
pub mod check;
pub mod crc;
pub mod device;
pub mod dir;
mod dirwrite;
pub mod error;
pub mod extent;
pub mod file;
pub mod fs;
pub mod group;
pub mod hash;
mod inline;
pub mod inode;
pub mod journal;
pub mod mkfs;
pub mod ops;
mod orphan;
pub mod superblock;
pub mod volume;
pub mod xattr;

mod cache;

pub use device::{BlockDevice, FileDevice, MemDevice};
pub use error::{Error, Result};
pub use fs::{Attr, Filesystem, StatFs};
pub use volume::{probe, MountOptions, ProbeInfo};
