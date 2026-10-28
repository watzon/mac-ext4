//! Namespace and attribute operations on a [`Volume`]. Each public method
//! here is one operation; [`crate::fs::Filesystem`] wraps it in an
//! undo scope and the volume lock.

use crate::dir::{DirEntry, NAME_MAX};
use crate::error::{Error, Result};
use crate::extent::init_extent_root;
use crate::inode::*;
use crate::superblock::{incompat, ro_compat};
use crate::volume::Volume;

/// Link count limit for directories without `dir_nlink`, and for files.
pub const LINK_MAX: u16 = 65000;

/// Attribute changes requested by `setattr`.
#[derive(Clone, Debug, Default)]
pub struct SetAttr {
    pub mode: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<Timestamp>,
    pub mtime: Option<Timestamp>,
    pub ctime: Option<Timestamp>,
    pub crtime: Option<Timestamp>,
    /// ext4 `i_flags` user-settable subset (immutable, append, nodump).
    pub flags: Option<u32>,
}

/// Inode flags that `setattr` replaces. These are the flags the host can
/// express (immutable, append-only, nodump); every other flag, such as a
/// Linux-set noatime, is kept as it is.
pub const SETTABLE_FLAGS: u32 = FL_IMMUTABLE | FL_APPEND | FL_NODUMP;

pub(crate) fn validate_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(Error::invalid("invalid file name"));
    }
    if name.len() > NAME_MAX {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

fn next_generation() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static GEN: AtomicU32 = AtomicU32::new(0);
    let seed = (Timestamp::now().nsecs ^ (Timestamp::now().secs as u32).rotate_left(13)) | 1;
    let _ = GEN.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed);
    GEN.fetch_add(0x9E37_79B9, Ordering::Relaxed)
}

impl Volume {
    fn load_dir(&self, ino: u32) -> Result<Inode> {
        let inode = self.read_inode(ino)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if inode.links_count() == 0 {
            return Err(Error::NotFound);
        }
        Ok(inode)
    }

    fn check_mutable(inode: &Inode) -> Result<()> {
        if inode.has_flag(FL_IMMUTABLE) || inode.has_flag(FL_APPEND) {
            Err(Error::PermissionDenied)
        } else {
            Ok(())
        }
    }

    fn touch_dir(dir: &mut Inode) {
        let now = Timestamp::now();
        dir.set_mtime(now);
        dir.set_ctime(now);
    }

    fn inc_dir_links(&mut self, dir: &mut Inode) -> Result<()> {
        let n = dir.links_count();
        if n >= LINK_MAX || n == 1 {
            if !self.sb.has_ro_compat(ro_compat::DIR_NLINK) {
                return Err(Error::TooManyLinks);
            }
            // With dir_nlink, a count of 1 means "too many to count".
            dir.set_links_count(1);
        } else {
            dir.set_links_count(n + 1);
        }
        Ok(())
    }

    fn dec_dir_links(dir: &mut Inode) {
        let n = dir.links_count();
        if n > 2 {
            dir.set_links_count(n - 1);
        }
    }

    fn new_inode(&self, mode: u16, uid: u32, gid: u32) -> Inode {
        let mut inode = Inode::new(self.sb.inode_size() as usize);
        if self.sb.inode_size() as usize > GOOD_OLD_INODE_SIZE {
            let extra = self
                .sb
                .want_extra_isize()
                .max(32)
                .min(self.sb.inode_size() - GOOD_OLD_INODE_SIZE as u16);
            inode.set_extra_isize(extra);
        }
        inode.set_mode(mode);
        inode.set_uid(uid);
        inode.set_gid(gid);
        let now = Timestamp::now();
        inode.set_atime(now);
        inode.set_ctime(now);
        inode.set_mtime(now);
        inode.set_crtime(now);
        inode.set_generation(next_generation());
        inode
    }

    pub(crate) fn uses_extents(&self) -> bool {
        self.sb.has_incompat(incompat::EXTENTS)
    }

    /// Create a file system object of any type except symlink.
    pub fn create(
        &mut self,
        dir: u32,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        rdev: (u32, u32),
    ) -> Result<u32> {
        self.ensure_writable()?;
        validate_name(name)?;
        let mut dinode = self.load_dir(dir)?;
        Self::check_mutable(&dinode)?;
        if self.find_entry(dir, &dinode, name)?.is_some() {
            return Err(Error::Exists);
        }
        let ft = FileType::from_mode(mode);
        if ft == FileType::Unknown || ft == FileType::Symlink {
            return Err(Error::invalid("unsupported file type"));
        }
        let is_dir = ft == FileType::Directory;
        if is_dir {
            self.inc_dir_links(&mut dinode)?;
        }
        let ino = self.alloc_inode(dir, is_dir)?;
        let mut inode = self.new_inode(mode, uid, gid);
        // Inherit project id and a few behavior flags from the parent.
        let inherit = dinode.flags() & (FL_NODUMP | FL_NOATIME | FL_SYNC | FL_DIRSYNC);
        inode.set_flags(inode.flags() | inherit);
        match ft {
            FileType::Regular | FileType::Directory => {
                if self.uses_extents() {
                    inode.set_flags(inode.flags() | FL_EXTENTS);
                    init_extent_root(&mut inode);
                }
            }
            FileType::CharDevice | FileType::BlockDevice => inode.set_rdev(rdev.0, rdev.1),
            _ => {}
        }
        if is_dir {
            inode.set_links_count(2);
            self.init_dir_block(ino, &mut inode, dir, &[])?;
        } else {
            inode.set_links_count(1);
        }
        self.write_inode(ino, &inode)?;
        self.add_entry(dir, &mut dinode, name, ino, ft)?;
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        Ok(ino)
    }

    pub fn symlink(
        &mut self,
        dir: u32,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<u32> {
        self.ensure_writable()?;
        validate_name(name)?;
        if target.is_empty() || target.len() >= self.block_size.min(4096) {
            return Err(if target.is_empty() {
                Error::invalid("empty symlink target")
            } else {
                Error::NameTooLong
            });
        }
        let mut dinode = self.load_dir(dir)?;
        Self::check_mutable(&dinode)?;
        if self.find_entry(dir, &dinode, name)?.is_some() {
            return Err(Error::Exists);
        }
        let ino = self.alloc_inode(dir, false)?;
        let mut inode = self.new_inode(S_IFLNK | 0o777, uid, gid);
        inode.set_links_count(1);
        if target.len() < I_BLOCK_LEN {
            inode.i_block_mut()[..target.len()].copy_from_slice(target);
            inode.set_size(target.len() as u64);
        } else {
            if self.uses_extents() {
                inode.set_flags(inode.flags() | FL_EXTENTS);
                init_extent_root(&mut inode);
            }
            self.write_file(ino, &mut inode, 0, target)?;
        }
        self.write_inode(ino, &inode)?;
        self.add_entry(dir, &mut dinode, name, ino, FileType::Symlink)?;
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        Ok(ino)
    }

    pub fn link(&mut self, ino: u32, dir: u32, name: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        validate_name(name)?;
        let mut inode = self.read_inode(ino)?;
        // A freed inode keeps its mode, so a zero link count is only a live
        // object while it is still in the retained orphan set.
        if inode.mode() == 0 || (inode.links_count() == 0 && !self.orphans.contains(&ino)) {
            return Err(Error::NotFound);
        }
        if inode.is_dir() {
            return Err(Error::PermissionDenied);
        }
        Self::check_mutable(&inode)?;
        if inode.links_count() >= LINK_MAX {
            return Err(Error::TooManyLinks);
        }
        let mut dinode = self.load_dir(dir)?;
        Self::check_mutable(&dinode)?;
        if self.find_entry(dir, &dinode, name)?.is_some() {
            return Err(Error::Exists);
        }
        self.add_entry(dir, &mut dinode, name, ino, inode.file_type())?;
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        let was_orphan = inode.links_count() == 0;
        inode.set_links_count(inode.links_count() + 1);
        inode.set_ctime(Timestamp::now());
        if was_orphan {
            self.remove_orphan(ino)?;
            let mut fresh = self.read_inode(ino)?;
            fresh.set_links_count(inode.links_count());
            fresh.set_ctime(inode.ctime());
            fresh.set_dtime(0);
            inode = fresh;
        }
        self.write_inode(ino, &inode)
    }

    /// Drop one link of `inode` (already removed from its directory).
    /// Orphans it when the count reaches zero.
    fn drop_link(&mut self, ino: u32, inode: &mut Inode) -> Result<()> {
        if inode.is_dir() {
            inode.set_links_count(0);
        } else {
            inode.set_links_count(inode.links_count().saturating_sub(1));
        }
        inode.set_ctime(Timestamp::now());
        if inode.links_count() == 0 {
            self.add_orphan(ino, inode);
        }
        self.write_inode(ino, inode)
    }

    /// Unlink a file or remove an empty directory.
    pub fn remove(&mut self, dir: u32, name: &[u8]) -> Result<u32> {
        self.ensure_writable()?;
        if name == b"." || name == b".." {
            return Err(Error::invalid("cannot remove . or .."));
        }
        let mut dinode = self.load_dir(dir)?;
        Self::check_mutable(&dinode)?;
        let (entry, _) = self
            .find_entry(dir, &dinode, name)?
            .ok_or(Error::NotFound)?;
        let mut inode = self.read_inode(entry.inode)?;
        Self::check_mutable(&inode)?;
        if inode.is_dir() {
            if !self.dir_is_empty(entry.inode)? {
                return Err(Error::NotEmpty);
            }
            Self::dec_dir_links(&mut dinode);
        }
        self.remove_entry(dir, &mut dinode, name)?;
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        self.drop_link(entry.inode, &mut inode)?;
        Ok(entry.inode)
    }

    /// True if `ancestor` is `ino` or one of its parents.
    fn is_ancestor(&self, ancestor: u32, mut ino: u32) -> Result<bool> {
        for _ in 0..4096 {
            if ino == ancestor {
                return Ok(true);
            }
            if ino == ROOT_INO {
                return Ok(false);
            }
            let inode = self.read_inode(ino)?;
            let parent = self
                .find_entry(ino, &inode, b"..")?
                .ok_or_else(|| Error::corrupt("directory without .."))?
                .0
                .inode;
            if parent == ino {
                return Ok(false);
            }
            ino = parent;
        }
        Err(Error::corrupt("directory tree is too deep or loops"))
    }

    /// Rename `sname` in `sdir` to `dname` in `ddir`.
    pub fn rename(
        &mut self,
        sdir: u32,
        sname: &[u8],
        ddir: u32,
        dname: &[u8],
        noreplace: bool,
        exchange: bool,
    ) -> Result<()> {
        self.ensure_writable()?;
        validate_name(dname)?;
        if sname == b"." || sname == b".." {
            return Err(Error::invalid("cannot rename . or .."));
        }
        let sd = self.load_dir(sdir)?;
        Self::check_mutable(&sd)?;
        let (src, _) = self.find_entry(sdir, &sd, sname)?.ok_or(Error::NotFound)?;
        let dd = if ddir == sdir {
            sd.clone()
        } else {
            self.load_dir(ddir)?
        };
        Self::check_mutable(&dd)?;
        let dst = self.find_entry(ddir, &dd, dname)?.map(|(e, _)| e);
        let mut sinode = self.read_inode(src.inode)?;
        Self::check_mutable(&sinode)?;
        let src_is_dir = sinode.is_dir();

        if let Some(d) = &dst {
            if d.inode == src.inode {
                // Same object (hard links or identical names): POSIX no-op.
                return Ok(());
            }
            if noreplace {
                return Err(Error::Exists);
            }
        } else if exchange {
            return Err(Error::NotFound);
        }
        // Both objects must be mutable before anything changes, for a plain
        // replacement and for an exchange alike.
        let target = match &dst {
            Some(d) => {
                let t = self.read_inode(d.inode)?;
                Self::check_mutable(&t)?;
                Some((d.inode, t))
            }
            None => None,
        };
        if src_is_dir && ddir != sdir && self.is_ancestor(src.inode, ddir)? {
            return Err(Error::invalid("cannot move a directory into itself"));
        }

        if exchange {
            let d = dst.expect("checked above");
            return self.exchange(sdir, sname, &src, ddir, dname, &d);
        }

        if let Some((tino, tinode)) = &target {
            match (src_is_dir, tinode.is_dir()) {
                (true, false) => return Err(Error::NotDir),
                (false, true) => return Err(Error::IsDir),
                (true, true) if !self.dir_is_empty(*tino)? => return Err(Error::NotEmpty),
                _ => {}
            }
        }
        let replacing = target.is_some();

        // Link counts follow Linux ext4_rename. Moving a directory always
        // drops the old parent's link from the moved "..". The new parent
        // gains one only when no directory is replaced there; a replaced
        // directory takes its own ".." link with it. In the same parent
        // both sides apply to one inode.
        let now = Timestamp::now();
        // 1. Point the destination name at the source inode.
        let mut dd = self.read_inode(ddir)?;
        if replacing {
            self.retarget_entry(ddir, &mut dd, dname, src.inode, sinode.file_type())?;
        } else {
            if src_is_dir && ddir != sdir {
                self.inc_dir_links(&mut dd)?;
            }
            self.add_entry(ddir, &mut dd, dname, src.inode, sinode.file_type())?;
        }
        dd.set_mtime(now);
        dd.set_ctime(now);
        self.write_inode(ddir, &dd)?;

        // 2. Remove the source name.
        let mut sd = self.read_inode(sdir)?;
        self.remove_entry(sdir, &mut sd, sname)?;
        if src_is_dir && (ddir != sdir || replacing) {
            Self::dec_dir_links(&mut sd);
        }
        sd.set_mtime(now);
        sd.set_ctime(now);
        self.write_inode(sdir, &sd)?;

        // 3. Fix the moved directory's "..".
        if src_is_dir && ddir != sdir {
            self.retarget_entry(src.inode, &mut sinode, b"..", ddir, FileType::Directory)?;
        }
        sinode = if src_is_dir {
            sinode
        } else {
            self.read_inode(src.inode)?
        };
        sinode.set_ctime(now);
        self.write_inode(src.inode, &sinode)?;

        // 4. Drop the replaced target. Its parent's link count was settled
        // in steps 1 and 2.
        if let Some((tino, _)) = target {
            let mut t = self.read_inode(tino)?;
            self.drop_link(tino, &mut t)?;
        }
        Ok(())
    }

    fn exchange(
        &mut self,
        sdir: u32,
        sname: &[u8],
        src: &DirEntry,
        ddir: u32,
        dname: &[u8],
        dst: &DirEntry,
    ) -> Result<()> {
        let sinode = self.read_inode(src.inode)?;
        let dinode = self.read_inode(dst.inode)?;
        if sdir != ddir {
            if sinode.is_dir() && self.is_ancestor(src.inode, ddir)? {
                return Err(Error::invalid("cannot move a directory into itself"));
            }
            if dinode.is_dir() && self.is_ancestor(dst.inode, sdir)? {
                return Err(Error::invalid("cannot move a directory into itself"));
            }
        }
        let now = Timestamp::now();
        let mut sd = self.read_inode(sdir)?;
        self.retarget_entry(sdir, &mut sd, sname, dst.inode, dinode.file_type())?;
        self.write_inode(sdir, &sd)?;
        let mut dd = self.read_inode(ddir)?;
        self.retarget_entry(ddir, &mut dd, dname, src.inode, sinode.file_type())?;
        self.write_inode(ddir, &dd)?;
        if sdir != ddir {
            let s_dir = sinode.is_dir();
            let d_dir = dinode.is_dir();
            if s_dir {
                let mut i = self.read_inode(src.inode)?;
                self.retarget_entry(src.inode, &mut i, b"..", ddir, FileType::Directory)?;
                i.set_ctime(now);
                self.write_inode(src.inode, &i)?;
            }
            if d_dir {
                let mut i = self.read_inode(dst.inode)?;
                self.retarget_entry(dst.inode, &mut i, b"..", sdir, FileType::Directory)?;
                i.set_ctime(now);
                self.write_inode(dst.inode, &i)?;
            }
            if s_dir != d_dir {
                let (mut gain, gain_ino, mut lose, lose_ino) = if s_dir {
                    (self.read_inode(ddir)?, ddir, self.read_inode(sdir)?, sdir)
                } else {
                    (self.read_inode(sdir)?, sdir, self.read_inode(ddir)?, ddir)
                };
                self.inc_dir_links(&mut gain)?;
                Self::dec_dir_links(&mut lose);
                self.write_inode(gain_ino, &gain)?;
                self.write_inode(lose_ino, &lose)?;
            }
        }
        for d in [sdir, ddir] {
            let mut i = self.read_inode(d)?;
            i.set_mtime(now);
            i.set_ctime(now);
            self.write_inode(d, &i)?;
        }
        Ok(())
    }

    pub fn write(&mut self, ino: u32, offset: u64, data: &[u8]) -> Result<usize> {
        self.ensure_writable()?;
        let mut inode = self.read_inode(ino)?;
        match inode.file_type() {
            FileType::Regular => {}
            FileType::Directory => return Err(Error::IsDir),
            _ => return Err(Error::invalid("not a regular file")),
        }
        if inode.has_flag(FL_IMMUTABLE) {
            return Err(Error::PermissionDenied);
        }
        if inode.has_flag(FL_APPEND) && offset != inode.size() {
            return Err(Error::PermissionDenied);
        }
        self.write_file(ino, &mut inode, offset, data)?;
        let now = Timestamp::now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.write_inode(ino, &inode)?;
        Ok(data.len())
    }

    pub fn setattr(&mut self, ino: u32, attr: &SetAttr) -> Result<()> {
        self.ensure_writable()?;
        let mut inode = self.read_inode(ino)?;
        let only_flags = attr.mode.is_none()
            && attr.uid.is_none()
            && attr.gid.is_none()
            && attr.size.is_none()
            && attr.atime.is_none()
            && attr.mtime.is_none()
            && attr.crtime.is_none();
        if (inode.has_flag(FL_IMMUTABLE) || inode.has_flag(FL_APPEND)) && !only_flags {
            return Err(Error::PermissionDenied);
        }
        let now = Timestamp::now();
        if let Some(m) = attr.mode {
            inode.set_mode((inode.mode() & S_IFMT) | (m & !S_IFMT));
        }
        if let Some(u) = attr.uid {
            inode.set_uid(u);
        }
        if let Some(g) = attr.gid {
            inode.set_gid(g);
        }
        if let Some(size) = attr.size {
            match inode.file_type() {
                FileType::Regular => {}
                FileType::Directory => return Err(Error::IsDir),
                _ => return Err(Error::invalid("cannot resize this file type")),
            }
            if size != inode.size() {
                self.truncate(ino, &mut inode, size)?;
                inode.set_mtime(now);
            }
        }
        if let Some(t) = attr.atime {
            inode.set_atime(t);
        }
        if let Some(t) = attr.mtime {
            inode.set_mtime(t);
        }
        if let Some(t) = attr.crtime {
            inode.set_crtime(t);
        }
        if let Some(f) = attr.flags {
            inode.set_flags((inode.flags() & !SETTABLE_FLAGS) | (f & SETTABLE_FLAGS));
        }
        inode.set_ctime(attr.ctime.unwrap_or(now));
        self.write_inode(ino, &inode)
    }

    /// Set (`Some`) or remove (`None`) one attribute.
    pub fn set_xattr(
        &mut self,
        ino: u32,
        index: u8,
        name: &[u8],
        value: Option<&[u8]>,
        create: bool,
        replace: bool,
    ) -> Result<()> {
        self.ensure_writable()?;
        let mut inode = self.read_inode(ino)?;
        if inode.has_flag(FL_IMMUTABLE) || inode.has_flag(FL_APPEND) {
            return Err(Error::PermissionDenied);
        }
        let mut attrs = self.read_xattrs(ino, &inode)?;
        let pos = attrs
            .iter()
            .position(|a| a.index == index && a.name == name);
        match (pos, value) {
            (Some(_), Some(_)) if create => return Err(Error::Exists),
            (None, Some(_)) if replace => return Err(Error::NoAttribute),
            (None, None) => return Err(Error::NoAttribute),
            (Some(p), None) => {
                attrs.remove(p);
            }
            (Some(p), Some(v)) => attrs[p].value = v.to_vec(),
            (None, Some(v)) => attrs.push(crate::xattr::Xattr {
                index,
                name: name.to_vec(),
                value: v.to_vec(),
            }),
        }
        self.store_xattrs(ino, &mut inode, &attrs)?;
        inode.set_ctime(Timestamp::now());
        self.write_inode(ino, &inode)
    }

    pub fn set_label(&mut self, label: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        if label.len() > 16 {
            return Err(Error::NameTooLong);
        }
        self.sb.set_volume_name(label);
        self.mark_sb_dirty();
        Ok(())
    }
}
