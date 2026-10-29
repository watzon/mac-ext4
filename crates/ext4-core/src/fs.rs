//! [`Filesystem`]: the thread-safe API that platform shims call.
//!
//! Reads share a read lock; each mutation takes the write lock, runs as one
//! undoable operation, and joins the running journal transaction, which is
//! committed when it grows large, every few seconds via [`Filesystem::tick`],
//! on [`Filesystem::sync`], and at unmount.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::device::BlockDevice;
use crate::dir::{DirEntry, NAME_MAX};
use crate::error::{Error, Result};
use crate::inode::*;
use crate::koio::IoRun;
use crate::ops::SetAttr;
use crate::superblock::STATE_ERROR;
use crate::volume::{MountOptions, RecoveryReport, Volume};
use crate::xattr::{self, Xattr};

/// File attributes in host-neutral form.
#[derive(Clone, Debug)]
pub struct Attr {
    pub ino: u32,
    /// Parent directory for directories (from ".."), else 0.
    pub parent: u32,
    pub mode: u16,
    pub file_type: FileType,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub alloc_bytes: u64,
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
    pub crtime: Option<Timestamp>,
    /// ext4 `i_flags`.
    pub flags: u32,
    pub rdev: (u32, u32),
    pub generation: u32,
    /// The kernel may do this file's data I/O (see `koio`).
    pub kernel_io: bool,
}

#[derive(Clone, Debug)]
pub struct StatFs {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: u64,
    pub avail_blocks: u64,
    pub inodes: u64,
    pub free_inodes: u64,
    pub name_max: u32,
    pub label: String,
    pub uuid: [u8; 16],
    pub flavor: &'static str,
    pub read_only: bool,
    /// Largest size of a new file on this volume, in bytes.
    pub max_file_size: u64,
}

/// A mounted ext2/3/4 file system.
pub struct Filesystem {
    vol: RwLock<Volume>,
}

/// How a macOS attribute name maps onto ext4.
enum XName<'a> {
    /// Writable `user.` attribute.
    User(&'a [u8]),
    /// Another ext4 namespace, exposed read-only as `linux.<full name>`.
    Other(u8, &'a [u8]),
}

const LINUX_PREFIX: &[u8] = b"linux.";

fn map_xattr_name(name: &[u8]) -> Result<XName<'_>> {
    if name.is_empty() || name.len() > 255 {
        return Err(if name.is_empty() {
            Error::invalid("empty attribute name")
        } else {
            Error::NameTooLong
        });
    }
    if let Some(full) = name.strip_prefix(LINUX_PREFIX) {
        let (idx, rest) = xattr::split_name(full).ok_or(Error::NoAttribute)?;
        if idx == xattr::INDEX_USER {
            return Ok(XName::User(rest));
        }
        return Ok(XName::Other(idx, rest));
    }
    Ok(XName::User(name))
}

impl Filesystem {
    /// Mount `dev`. Journal replay and orphan cleanup run here.
    pub fn open(dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Filesystem> {
        let vol = Volume::open(dev, opts)?;
        Ok(Filesystem {
            vol: RwLock::new(vol),
        })
    }

    fn r(&self) -> RwLockReadGuard<'_, Volume> {
        self.vol.read().unwrap_or_else(|e| e.into_inner())
    }

    fn w(&self) -> RwLockWriteGuard<'_, Volume> {
        self.vol.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Run one mutation as an undoable operation.
    fn mutate<T>(&self, f: impl FnOnce(&mut Volume) -> Result<T>) -> Result<T> {
        self.mutate_synced(&[], &[], f)
    }

    /// Run one mutation as an undoable operation. When it succeeds and one
    /// of `files` has `FL_SYNC`, or one of `dirs` has `FL_SYNC` or
    /// `FL_DIRSYNC` (Linux `IS_SYNC` and `IS_DIRSYNC`), the change is made
    /// durable before returning.
    fn mutate_synced<T>(
        &self,
        files: &[u32],
        dirs: &[u32],
        f: impl FnOnce(&mut Volume) -> Result<T>,
    ) -> Result<T> {
        let mut v = self.w();
        v.ensure_writable()?;
        v.begin_op();
        let r = f(&mut v);
        let out = v.end_op(r)?;
        if Self::wants_sync(&v, files, dirs) {
            Self::sync_locked(&mut v)?;
        }
        Ok(out)
    }

    fn wants_sync(v: &Volume, files: &[u32], dirs: &[u32]) -> bool {
        let has = |ino: u32, mask: u32| v.read_inode(ino).is_ok_and(|i| i.flags() & mask != 0);
        files.iter().any(|&ino| has(ino, FL_SYNC))
            || dirs.iter().any(|&ino| has(ino, FL_SYNC | FL_DIRSYNC))
    }

    /// Commit the running transaction, or flush when there is nothing to
    /// commit. A commit already ends with a device flush.
    fn sync_locked(v: &mut Volume) -> Result<()> {
        if v.has_pending() {
            v.commit()
        } else {
            v.device().flush()?;
            Ok(())
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.r().is_read_only()
    }

    pub fn read_only_reason(&self) -> Option<String> {
        self.r().read_only_reason().map(str::to_owned)
    }

    pub fn recovery_report(&self) -> RecoveryReport {
        self.r().recovery.clone()
    }

    /// Run `f` with shared access to the volume (for tools such as fsck).
    pub fn with_volume<T>(&self, f: impl FnOnce(&Volume) -> T) -> T {
        f(&self.r())
    }

    pub fn statfs(&self) -> StatFs {
        let v = self.r();
        let sb = &v.sb;
        let free = sb.free_blocks_count();
        StatFs {
            block_size: sb.block_size(),
            blocks: sb.blocks_count(),
            free_blocks: free,
            avail_blocks: free.saturating_sub(sb.r_blocks_count()),
            inodes: sb.inodes_count() as u64,
            free_inodes: sb.free_inodes_count() as u64,
            name_max: NAME_MAX as u32,
            label: String::from_utf8_lossy(sb.volume_name()).into_owned(),
            uuid: sb.uuid(),
            flavor: crate::volume::flavor(sb),
            read_only: v.is_read_only() || sb.state() & STATE_ERROR != 0,
            max_file_size: v.max_new_file_size(),
        }
    }

    fn attr_of(v: &Volume, ino: u32, inode: &Inode) -> Result<Attr> {
        let parent = if inode.is_dir() {
            if ino == ROOT_INO {
                ROOT_INO
            } else {
                v.find_entry(ino, inode, b"..")?.map_or(0, |(e, _)| e.inode)
            }
        } else {
            0
        };
        let file_type = inode.file_type();
        let rdev = match file_type {
            FileType::CharDevice | FileType::BlockDevice => inode.rdev(),
            _ => (0, 0),
        };
        Ok(Attr {
            ino,
            parent,
            mode: inode.mode(),
            file_type,
            uid: inode.uid(),
            gid: inode.gid(),
            nlink: inode.links_count() as u32,
            size: inode.size(),
            alloc_bytes: inode.allocated_bytes(&v.sb),
            atime: inode.atime(),
            mtime: inode.mtime(),
            ctime: inode.ctime(),
            crtime: inode.crtime(),
            flags: inode.flags(),
            rdev,
            generation: inode.generation(),
            kernel_io: crate::koio::kernel_io_eligible(inode, v.is_read_only()),
        })
    }

    fn live_inode(v: &Volume, ino: u32) -> Result<Inode> {
        let inode = v.read_inode(ino)?;
        // Freed inodes keep their mode; a zero link count means gone unless
        // the inode is an orphan the host still references.
        if inode.mode() == 0 || (inode.links_count() == 0 && !v.orphans.contains(&ino)) {
            return Err(Error::NotFound);
        }
        Ok(inode)
    }

    pub fn getattr(&self, ino: u32) -> Result<Attr> {
        let v = self.r();
        let inode = Self::live_inode(&v, ino)?;
        Self::attr_of(&v, ino, &inode)
    }

    pub fn lookup(&self, dir: u32, name: &[u8]) -> Result<Attr> {
        let v = self.r();
        let e = v.lookup(dir, name)?.ok_or(Error::NotFound)?;
        let inode = Self::live_inode(&v, e.inode)?;
        Self::attr_of(&v, e.inode, &inode)
    }

    /// Enumerate `dir` from `cookie`; `f(entry, next_cookie)` returns false
    /// to stop. Includes "." and "..".
    pub fn read_dir(
        &self,
        dir: u32,
        cookie: u64,
        f: impl FnMut(DirEntry, u64) -> bool,
    ) -> Result<()> {
        self.r().read_dir(dir, cookie, f)
    }

    /// Enumerate `dir` like [`Filesystem::read_dir`], holding one read lock
    /// for the whole call. `f(entry, attr, next_cookie)` returns false to
    /// stop. With `with_attrs`, `attr` is what [`Filesystem::getattr`] would
    /// return for the entry's inode, except for "." and "..", which get
    /// `None`. If reading an entry's attributes fails, enumeration stops
    /// and that error is returned. Without `with_attrs`, `attr` is always
    /// `None` and no inodes are read.
    pub fn read_dir_plus(
        &self,
        dir: u32,
        cookie: u64,
        with_attrs: bool,
        mut f: impl FnMut(&DirEntry, Option<&Attr>, u64) -> bool,
    ) -> Result<()> {
        let v = self.r();
        let mut failure = None;
        v.read_dir(dir, cookie, |e, next| {
            if !with_attrs || e.name == b"." || e.name == b".." {
                return f(&e, None, next);
            }
            match Self::live_inode(&v, e.inode).and_then(|i| Self::attr_of(&v, e.inode, &i)) {
                Ok(attr) => f(&e, Some(&attr), next),
                Err(err) => {
                    failure = Some(err);
                    false
                }
            }
        })?;
        failure.map_or(Ok(()), Err)
    }

    /// Collect all entries of a directory.
    pub fn list_dir(&self, dir: u32) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        self.read_dir(dir, 0, |e, _| {
            out.push(e);
            true
        })?;
        Ok(out)
    }

    /// A value that changes whenever the directory changes.
    pub fn dir_verifier(&self, dir: u32) -> Result<u64> {
        let v = self.r();
        let inode = v.read_inode(dir)?;
        let m = inode.mtime();
        let c = inode.ctime();
        let mut h = (m.secs as u64).wrapping_mul(1_000_000_007) ^ m.nsecs as u64;
        h = h.rotate_left(17) ^ (c.secs as u64).wrapping_mul(998_244_353) ^ c.nsecs as u64;
        h = h.rotate_left(17) ^ inode.size();
        Ok(h | 1)
    }

    pub fn read(&self, ino: u32, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let v = self.r();
        let inode = Self::live_inode(&v, ino)?;
        if inode.is_dir() {
            return Err(Error::IsDir);
        }
        v.read_file(ino, &inode, offset, buf)
    }

    pub fn readlink(&self, ino: u32) -> Result<Vec<u8>> {
        let v = self.r();
        let inode = Self::live_inode(&v, ino)?;
        if inode.file_type() != FileType::Symlink {
            return Err(Error::invalid("not a symbolic link"));
        }
        let size = inode.size().min(4096) as usize;
        let mut buf = vec![0u8; size];
        let n = v.read_file(ino, &inode, 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Write `data` at `offset`. Large writes run as several operations;
    /// if a later one fails after earlier ones succeeded, the bytes written
    /// so far are returned (a short write) and the error is dropped. The
    /// error is returned only when nothing was written.
    /// Map bytes of `ino` for kernel-offloaded reads (see `koio`).
    pub fn map_for_read(
        &self,
        ino: u32,
        offset: u64,
        len: u64,
        max_runs: usize,
    ) -> Result<Vec<IoRun>> {
        {
            let v = self.r();
            if !v.has_inline_data(ino)? || v.is_read_only() {
                return v.map_for_read(ino, offset, len, max_runs);
            }
        }
        // Inline data has no blocks to map: move it out first.
        self.mutate(|v| v.move_inline_data_out(ino))?;
        self.r().map_for_read(ino, offset, len, max_runs)
    }

    /// Map bytes of `ino` for kernel-offloaded writes, allocating unwritten
    /// blocks for holes (see `koio`).
    pub fn map_for_write(
        &self,
        ino: u32,
        offset: u64,
        len: u64,
        max_runs: usize,
    ) -> Result<Vec<IoRun>> {
        self.mutate(|v| v.map_for_write(ino, offset, len, max_runs))
    }

    /// The kernel finished writing bytes of `ino` (see `koio`).
    pub fn complete_kernel_write(&self, ino: u32, offset: u64, len: u64) -> Result<()> {
        self.mutate_synced(&[ino], &[], |v| v.complete_kernel_write(ino, offset, len))
    }

    pub fn write(&self, ino: u32, offset: u64, data: &[u8]) -> Result<usize> {
        // Split very large writes so each operation stays well inside one
        // journal transaction.
        const CHUNK: usize = 8 << 20;
        let mut done = 0;
        while done < data.len() {
            let n = CHUNK.min(data.len() - done);
            match self.mutate(|v| v.write(ino, offset + done as u64, &data[done..done + n])) {
                Ok(_) => done += n,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        if done > 0 {
            let mut v = self.w();
            if Self::wants_sync(&v, &[ino], &[]) {
                Self::sync_locked(&mut v)?;
            }
        }
        Ok(done)
    }

    pub fn create(
        &self,
        dir: u32,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        rdev: (u32, u32),
    ) -> Result<Attr> {
        self.mutate_synced(&[], &[dir], |v| {
            let ino = v.create(dir, name, mode, uid, gid, rdev)?;
            let inode = v.read_inode(ino)?;
            Self::attr_of(v, ino, &inode)
        })
    }

    pub fn mkdir(&self, dir: u32, name: &[u8], perm: u16, uid: u32, gid: u32) -> Result<Attr> {
        self.create(dir, name, S_IFDIR | (perm & 0o7777), uid, gid, (0, 0))
    }

    pub fn symlink(
        &self,
        dir: u32,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Attr> {
        self.mutate_synced(&[], &[dir], |v| {
            let ino = v.symlink(dir, name, target, uid, gid)?;
            let inode = v.read_inode(ino)?;
            Self::attr_of(v, ino, &inode)
        })
    }

    pub fn link(&self, ino: u32, dir: u32, name: &[u8]) -> Result<Attr> {
        self.mutate_synced(&[], &[dir], |v| {
            v.link(ino, dir, name)?;
            let inode = v.read_inode(ino)?;
            Self::attr_of(v, ino, &inode)
        })
    }

    /// Remove a name. Returns the inode it referred to. The inode stays
    /// allocated until [`Filesystem::reclaim`] if no links remain.
    pub fn remove(&self, dir: u32, name: &[u8]) -> Result<u32> {
        self.mutate_synced(&[], &[dir], |v| v.remove(dir, name))
    }

    pub fn rename(
        &self,
        sdir: u32,
        sname: &[u8],
        ddir: u32,
        dname: &[u8],
        noreplace: bool,
        exchange: bool,
    ) -> Result<()> {
        self.mutate_synced(&[], &[sdir, ddir], |v| {
            v.rename(sdir, sname, ddir, dname, noreplace, exchange)
        })
    }

    pub fn setattr(&self, ino: u32, attr: &SetAttr) -> Result<Attr> {
        // Size changes can free many blocks; run them in bounded steps so
        // one transaction never outgrows the journal.
        if let Some(target) = attr.size {
            let cur = self.getattr(ino)?.size;
            let step = 1u64 << 34;
            let mut size = cur;
            while size > target.saturating_add(step) {
                size -= step;
                let partial = SetAttr {
                    size: Some(size),
                    ..Default::default()
                };
                self.mutate(|v| v.setattr(ino, &partial))?;
            }
        }
        self.mutate_synced(&[ino], &[], |v| {
            v.setattr(ino, attr)?;
            let inode = v.read_inode(ino)?;
            Self::attr_of(v, ino, &inode)
        })
    }

    /// The host dropped its last reference to `ino`.
    pub fn reclaim(&self, ino: u32) -> Result<()> {
        let mut v = self.w();
        if v.is_read_only() {
            return Ok(());
        }
        v.begin_op();
        let r = v.reclaim(ino);
        v.end_op(r)
    }

    /// macOS-view attribute names of `ino`.
    pub fn list_xattrs(&self, ino: u32) -> Result<Vec<Vec<u8>>> {
        let v = self.r();
        let inode = Self::live_inode(&v, ino)?;
        let attrs = v.read_xattrs(ino, &inode)?;
        Ok(attrs
            .into_iter()
            .filter(|a| !(a.index == xattr::INDEX_SYSTEM && a.name == b"data"))
            .map(|a| {
                if a.index == xattr::INDEX_USER {
                    a.name
                } else {
                    let mut n = LINUX_PREFIX.to_vec();
                    n.extend_from_slice(&Xattr::full_name(&a));
                    n
                }
            })
            .collect())
    }

    pub fn get_xattr(&self, ino: u32, name: &[u8]) -> Result<Vec<u8>> {
        let v = self.r();
        let inode = Self::live_inode(&v, ino)?;
        let (idx, n) = match map_xattr_name(name)? {
            XName::User(n) => (xattr::INDEX_USER, n),
            XName::Other(i, n) => (i, n),
        };
        if idx == xattr::INDEX_SYSTEM && n == b"data" {
            return Err(Error::NoAttribute);
        }
        v.get_xattr(ino, &inode, idx, n)?.ok_or(Error::NoAttribute)
    }

    pub fn set_xattr(
        &self,
        ino: u32,
        name: &[u8],
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<()> {
        let XName::User(n) = map_xattr_name(name)? else {
            return Err(Error::PermissionDenied);
        };
        self.mutate_synced(&[ino], &[], |v| {
            v.set_xattr(ino, xattr::INDEX_USER, n, Some(value), create, replace)
        })
    }

    pub fn remove_xattr(&self, ino: u32, name: &[u8]) -> Result<()> {
        let XName::User(n) = map_xattr_name(name)? else {
            return Err(Error::PermissionDenied);
        };
        self.mutate_synced(&[ino], &[], |v| {
            v.set_xattr(ino, xattr::INDEX_USER, n, None, false, false)
        })
    }

    pub fn set_label(&self, label: &[u8]) -> Result<()> {
        self.mutate(|v| v.set_label(label))
    }

    /// Commit the running transaction if it is due (call about once a second).
    pub fn tick(&self) -> Result<()> {
        let mut v = self.w();
        if v.is_read_only() {
            return Ok(());
        }
        v.maybe_commit()
    }

    /// Make every completed operation durable.
    pub fn sync(&self) -> Result<()> {
        let mut v = self.w();
        if v.is_read_only() {
            return Ok(());
        }
        Self::sync_locked(&mut v)
    }

    /// Simulate a crash immediately after the running transaction's durable
    /// journal commit block. Interoperability tests only.
    #[doc(hidden)]
    pub fn crash_after_commit_block(&self) -> Result<()> {
        self.w().commit_without_checkpoint_for_test()
    }

    /// Commit, mark the file system clean, and release the device.
    pub fn unmount(self) -> Result<()> {
        // Drop then finds nothing left to do.
        self.w().unmount()
    }
}

impl Drop for Filesystem {
    fn drop(&mut self) {
        // Best effort for callers that forget to unmount (for example a CLI
        // command that exits early): leave the volume consistent.
        let v = self.vol.get_mut().unwrap_or_else(|e| e.into_inner());
        let _ = v.unmount();
    }
}
