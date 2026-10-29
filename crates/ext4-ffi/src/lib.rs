//! C ABI over `ext4-core`. The contract is `include/ext4fs.h`; keep the two
//! in sync. Every entry point catches panics and reports them as EIO.

#![allow(clippy::missing_safety_doc, non_camel_case_types)]

use std::ffi::{c_char, c_void, CStr};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ext4_core::check::{check, CheckOptions};
use ext4_core::fs::{Attr, StatFs};
use ext4_core::inode::{Timestamp, FL_APPEND, FL_IMMUTABLE, FL_NODUMP};
use ext4_core::koio::IoKind;
use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::ops::SetAttr;
use ext4_core::{BlockDevice, Error, Filesystem, MountOptions};

const EIO: i32 = 5;
const EINVAL: i32 = 22;
const ERANGE: i32 = 34;

// ---- device ------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ext4fs_device {
    pub ctx: *mut c_void,
    pub read: Option<unsafe extern "C" fn(*mut c_void, u64, *mut c_void, usize) -> i32>,
    pub write: Option<unsafe extern "C" fn(*mut c_void, u64, *const c_void, usize) -> i32>,
    pub flush: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    pub size: u64,
    pub alignment: u32,
    pub read_only: bool,
}

/// A caller-provided device. The header requires `ctx` to stay valid and the
/// callbacks to be thread-safe for the lifetime of the mount.
struct CDevice(ext4fs_device);

// SAFETY: the C contract makes the callbacks callable from any thread.
unsafe impl Send for CDevice {}
unsafe impl Sync for CDevice {}

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(if code > 0 { code } else { EIO })
}

impl BlockDevice for CDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let f = self.0.read.ok_or_else(|| errno(EIO))?;
        // SAFETY: buf is valid for buf.len() bytes.
        match unsafe { f(self.0.ctx, offset, buf.as_mut_ptr().cast(), buf.len()) } {
            0 => Ok(()),
            e => Err(errno(e)),
        }
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.0.read_only {
            return Err(errno(30));
        }
        let f = self.0.write.ok_or_else(|| errno(30))?;
        // SAFETY: buf is valid for buf.len() bytes.
        match unsafe { f(self.0.ctx, offset, buf.as_ptr().cast(), buf.len()) } {
            0 => Ok(()),
            e => Err(errno(e)),
        }
    }
    fn flush(&self) -> io::Result<()> {
        match self.0.flush {
            // SAFETY: plain callback.
            Some(f) => match unsafe { f(self.0.ctx) } {
                0 => Ok(()),
                e => Err(errno(e)),
            },
            None => Ok(()),
        }
    }
    fn size(&self) -> u64 {
        self.0.size
    }
    fn alignment(&self) -> u32 {
        self.0.alignment.max(1)
    }
    fn read_only(&self) -> bool {
        self.0.read_only
    }
}

unsafe fn device_from(dev: *const ext4fs_device) -> Result<CDevice, i32> {
    if dev.is_null() {
        return Err(EINVAL);
    }
    Ok(CDevice(*dev))
}

// ---- helpers -----------------------------------------------------------

fn guard(f: impl FnOnce() -> Result<(), i32>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e,
        Err(_) => EIO,
    }
}

fn code(e: Error) -> i32 {
    e.errno()
}

unsafe fn write_msg(msg: *mut c_char, cap: usize, text: &str) {
    if msg.is_null() || cap == 0 {
        return;
    }
    let bytes = text.as_bytes();
    let mut n = bytes.len().min(cap - 1);
    // Do not split a UTF-8 sequence.
    while n > 0 && !text.is_char_boundary(n) {
        n -= 1;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), msg.cast::<u8>(), n);
    *msg.add(n) = 0;
}

unsafe fn bytes<'a>(p: *const u8, len: usize) -> Result<&'a [u8], i32> {
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(EINVAL);
    }
    Ok(std::slice::from_raw_parts(p, len))
}

fn copy_str(dst: &mut [c_char], s: &str) {
    let b = s.as_bytes();
    let mut n = b.len().min(dst.len() - 1);
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    for (d, &c) in dst.iter_mut().zip(&b[..n]) {
        *d = c as c_char;
    }
    dst[n] = 0;
}

/// Copy `data` into a caller buffer with ERANGE semantics.
unsafe fn out_buf(data: &[u8], buf: *mut u8, cap: usize, len_out: *mut usize) -> Result<(), i32> {
    if !len_out.is_null() {
        *len_out = data.len();
    }
    if cap == 0 {
        return Ok(());
    }
    if data.len() > cap || buf.is_null() {
        return Err(ERANGE);
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
    Ok(())
}

// ---- probe -------------------------------------------------------------

#[repr(C)]
pub struct ext4fs_probe_info {
    pub label: [c_char; 68],
    pub uuid: [u8; 16],
    pub flavor: [c_char; 8],
    pub block_size: u32,
    pub blocks: u64,
    pub readable: bool,
    pub writable: bool,
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_probe(
    dev: *const ext4fs_device,
    out: *mut ext4fs_probe_info,
    msg: *mut c_char,
    msg_cap: usize,
) -> i32 {
    guard(|| {
        let dev = device_from(dev)?;
        if out.is_null() {
            return Err(EINVAL);
        }
        match ext4_core::probe(&dev) {
            Ok(info) => {
                let o = &mut *out;
                copy_str(&mut o.label, &info.label);
                o.uuid = info.uuid;
                copy_str(&mut o.flavor, info.flavor);
                o.block_size = info.block_size;
                o.blocks = info.blocks;
                o.readable = info.readable;
                o.writable = info.writable;
                write_msg(msg, msg_cap, info.note.as_deref().unwrap_or(""));
                Ok(())
            }
            Err(e) => {
                write_msg(msg, msg_cap, &e.to_string());
                Err(match e {
                    Error::Io(_) => code(e),
                    _ => EINVAL,
                })
            }
        }
    })
}

// ---- mount -------------------------------------------------------------

/// Opaque volume handle.
pub struct ext4fs_volume {
    fs: Arc<Filesystem>,
    stop: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
}

const MOUNT_READ_ONLY: u32 = 0x1;
const MOUNT_IGNORE_CHECKSUMS: u32 = 0x2;

#[no_mangle]
pub unsafe extern "C" fn ext4fs_mount(
    dev: *const ext4fs_device,
    flags: u32,
    out: *mut *mut ext4fs_volume,
    read_only_out: *mut bool,
    msg: *mut c_char,
    msg_cap: usize,
) -> i32 {
    guard(|| {
        let dev = device_from(dev)?;
        if out.is_null() {
            return Err(EINVAL);
        }
        let opts = MountOptions {
            read_only: flags & MOUNT_READ_ONLY != 0,
            ignore_checksums: flags & MOUNT_IGNORE_CHECKSUMS != 0,
        };
        let fs = match Filesystem::open(Box::new(dev), &opts) {
            Ok(fs) => Arc::new(fs),
            Err(e) => {
                write_msg(msg, msg_cap, &e.to_string());
                return Err(code(e));
            }
        };
        let ro = fs.is_read_only();
        if !read_only_out.is_null() {
            *read_only_out = ro;
        }
        let note = match (ro, fs.read_only_reason()) {
            (true, Some(r)) if flags & MOUNT_READ_ONLY == 0 => r,
            _ => String::new(),
        };
        write_msg(msg, msg_cap, &note);
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = (!ro).then(|| {
            let fs = fs.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ext4fs-commit".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::park_timeout(Duration::from_secs(1));
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let _ = catch_unwind(AssertUnwindSafe(|| fs.tick()));
                    }
                })
                .ok()
        });
        *out = Box::into_raw(Box::new(ext4fs_volume {
            fs,
            stop,
            ticker: ticker.flatten(),
        }));
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_unmount(vol: *mut ext4fs_volume) -> i32 {
    if vol.is_null() {
        return EINVAL;
    }
    let mut v = Box::from_raw(vol);
    guard(move || {
        v.stop.store(true, Ordering::Relaxed);
        if let Some(t) = v.ticker.take() {
            t.thread().unpark();
            let _ = t.join();
        }
        match Arc::try_unwrap(v.fs) {
            Ok(fs) => fs.unmount().map_err(code),
            // Another thread still holds a reference (should not happen):
            // make everything durable; Drop finishes the unmount later.
            Err(fs) => fs.sync().map_err(code),
        }
    })
}

unsafe fn vol_ref<'a>(vol: *mut ext4fs_volume) -> Result<&'a Filesystem, i32> {
    if vol.is_null() {
        return Err(EINVAL);
    }
    Ok(&(*vol).fs)
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_sync(vol: *mut ext4fs_volume) -> i32 {
    guard(|| vol_ref(vol)?.sync().map_err(code))
}

// ---- attributes --------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ext4fs_timespec {
    pub sec: i64,
    pub nsec: u32,
}

impl From<Timestamp> for ext4fs_timespec {
    fn from(t: Timestamp) -> Self {
        ext4fs_timespec {
            sec: t.secs,
            nsec: t.nsecs,
        }
    }
}

impl From<ext4fs_timespec> for Timestamp {
    fn from(t: ext4fs_timespec) -> Self {
        Timestamp {
            secs: t.sec,
            nsecs: t.nsec.min(999_999_999),
        }
    }
}

const UF_NODUMP: u32 = 0x1;
const UF_IMMUTABLE: u32 = 0x2;
const UF_APPEND: u32 = 0x4;

fn bsd_flags(ext: u32) -> u32 {
    let mut f = 0;
    if ext & FL_NODUMP != 0 {
        f |= UF_NODUMP;
    }
    if ext & FL_IMMUTABLE != 0 {
        f |= UF_IMMUTABLE;
    }
    if ext & FL_APPEND != 0 {
        f |= UF_APPEND;
    }
    f
}

fn ext_flags(bsd: u32) -> u32 {
    let mut f = 0;
    if bsd & UF_NODUMP != 0 {
        f |= FL_NODUMP;
    }
    if bsd & UF_IMMUTABLE != 0 {
        f |= FL_IMMUTABLE;
    }
    if bsd & UF_APPEND != 0 {
        f |= FL_APPEND;
    }
    f
}

/// Darwin `makedev`.
fn makedev(major: u32, minor: u32) -> u32 {
    ((major & 0xFF) << 24) | (minor & 0xFF_FFFF)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ext4fs_attr {
    pub ino: u64,
    pub parent_ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub alloc_size: u64,
    pub flags: u32,
    pub rdev: u32,
    pub generation: u32,
    pub has_btime: bool,
    pub kernel_io: bool,
    pub atime: ext4fs_timespec,
    pub mtime: ext4fs_timespec,
    pub ctime: ext4fs_timespec,
    pub btime: ext4fs_timespec,
}

fn fill_attr(a: &Attr) -> ext4fs_attr {
    ext4fs_attr {
        ino: a.ino as u64,
        parent_ino: a.parent as u64,
        mode: a.mode as u32,
        uid: a.uid,
        gid: a.gid,
        nlink: a.nlink,
        size: a.size,
        alloc_size: a.alloc_bytes,
        flags: bsd_flags(a.flags),
        rdev: makedev(a.rdev.0, a.rdev.1),
        generation: a.generation,
        has_btime: a.crtime.is_some(),
        kernel_io: a.kernel_io,
        atime: a.atime.into(),
        mtime: a.mtime.into(),
        ctime: a.ctime.into(),
        btime: a.crtime.unwrap_or_default().into(),
    }
}

unsafe fn put_attr(out: *mut ext4fs_attr, a: &Attr) {
    if !out.is_null() {
        *out = fill_attr(a);
    }
}

fn ino32(ino: u64) -> Result<u32, i32> {
    u32::try_from(ino).map_err(|_| EINVAL)
}

#[repr(C)]
pub struct ext4fs_statfs {
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub avail_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub name_max: u32,
    pub uuid: [u8; 16],
    pub label: [c_char; 68],
    pub flavor: [c_char; 8],
    pub read_only: bool,
    pub max_file_size: u64,
}

fn fill_statfs(o: &mut ext4fs_statfs, s: &StatFs) {
    o.block_size = s.block_size;
    o.total_blocks = s.blocks;
    o.free_blocks = s.free_blocks;
    o.avail_blocks = s.avail_blocks;
    o.total_inodes = s.inodes;
    o.free_inodes = s.free_inodes;
    o.name_max = s.name_max;
    o.uuid = s.uuid;
    copy_str(&mut o.label, &s.label);
    copy_str(&mut o.flavor, s.flavor);
    o.read_only = s.read_only;
    o.max_file_size = s.max_file_size;
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_statfs_get(
    vol: *mut ext4fs_volume,
    out: *mut ext4fs_statfs,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        if out.is_null() {
            return Err(EINVAL);
        }
        fill_statfs(&mut *out, &fs.statfs());
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_set_label(
    vol: *mut ext4fs_volume,
    label: *const u8,
    len: usize,
) -> i32 {
    guard(|| vol_ref(vol)?.set_label(bytes(label, len)?).map_err(code))
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_getattr(
    vol: *mut ext4fs_volume,
    ino: u64,
    out: *mut ext4fs_attr,
) -> i32 {
    guard(|| {
        let a = vol_ref(vol)?.getattr(ino32(ino)?).map_err(code)?;
        put_attr(out, &a);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_lookup(
    vol: *mut ext4fs_volume,
    dir: u64,
    name: *const u8,
    name_len: usize,
    out: *mut ext4fs_attr,
) -> i32 {
    guard(|| {
        let a = vol_ref(vol)?
            .lookup(ino32(dir)?, bytes(name, name_len)?)
            .map_err(code)?;
        put_attr(out, &a);
        Ok(())
    })
}

const SET_MODE: u32 = 0x01;
const SET_UID: u32 = 0x02;
const SET_GID: u32 = 0x04;
const SET_SIZE: u32 = 0x08;
const SET_ATIME: u32 = 0x10;
const SET_MTIME: u32 = 0x20;
const SET_CTIME: u32 = 0x40;
const SET_BTIME: u32 = 0x80;
const SET_FLAGS: u32 = 0x100;

#[repr(C)]
pub struct ext4fs_setattr {
    pub valid: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub flags: u32,
    pub atime: ext4fs_timespec,
    pub mtime: ext4fs_timespec,
    pub ctime: ext4fs_timespec,
    pub btime: ext4fs_timespec,
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_setattr_apply(
    vol: *mut ext4fs_volume,
    ino: u64,
    attr: *const ext4fs_setattr,
    out: *mut ext4fs_attr,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        if attr.is_null() {
            return Err(EINVAL);
        }
        let s = &*attr;
        let has = |bit: u32| s.valid & bit != 0;
        let sa = SetAttr {
            mode: has(SET_MODE).then_some((s.mode & 0o7777) as u16),
            uid: has(SET_UID).then_some(s.uid),
            gid: has(SET_GID).then_some(s.gid),
            size: has(SET_SIZE).then_some(s.size),
            atime: has(SET_ATIME).then(|| s.atime.into()),
            mtime: has(SET_MTIME).then(|| s.mtime.into()),
            ctime: has(SET_CTIME).then(|| s.ctime.into()),
            crtime: has(SET_BTIME).then(|| s.btime.into()),
            flags: has(SET_FLAGS).then(|| ext_flags(s.flags)),
        };
        let a = fs.setattr(ino32(ino)?, &sa).map_err(code)?;
        put_attr(out, &a);
        Ok(())
    })
}

// ---- directories -------------------------------------------------------

pub type ext4fs_dirent_fn = Option<
    unsafe extern "C" fn(*mut c_void, *const u8, usize, u64, u8, u64, *const ext4fs_attr) -> bool,
>;

#[no_mangle]
pub unsafe extern "C" fn ext4fs_readdir(
    vol: *mut ext4fs_volume,
    dir: u64,
    cookie: u64,
    with_attrs: bool,
    f: ext4fs_dirent_fn,
    ctx: *mut c_void,
    verifier_out: *mut u64,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        let f = f.ok_or(EINVAL)?;
        let dir = ino32(dir)?;
        if !verifier_out.is_null() {
            *verifier_out = fs.dir_verifier(dir).map_err(code)?;
        }
        fs.read_dir_plus(dir, cookie, with_attrs, |e, attr, next| {
            let attr = attr.map(fill_attr);
            let attr_ptr = attr
                .as_ref()
                .map_or(std::ptr::null(), |a| a as *const ext4fs_attr);
            // SAFETY: the name and the attributes are valid for this call only.
            unsafe {
                f(
                    ctx,
                    e.name.as_ptr(),
                    e.name.len(),
                    e.inode as u64,
                    e.file_type.dirent_type(),
                    next,
                    attr_ptr,
                )
            }
        })
        .map_err(code)
    })
}

// ---- files -------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn ext4fs_read(
    vol: *mut ext4fs_volume,
    ino: u64,
    offset: u64,
    buf: *mut c_void,
    len: usize,
    read_out: *mut usize,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        if buf.is_null() && len > 0 {
            return Err(EINVAL);
        }
        let slice = if len == 0 {
            &mut [][..]
        } else {
            std::slice::from_raw_parts_mut(buf.cast::<u8>(), len)
        };
        let n = fs.read(ino32(ino)?, offset, slice).map_err(code)?;
        if !read_out.is_null() {
            *read_out = n;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_write(
    vol: *mut ext4fs_volume,
    ino: u64,
    offset: u64,
    buf: *const c_void,
    len: usize,
    written_out: *mut usize,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        let data = bytes(buf.cast(), len)?;
        let n = fs.write(ino32(ino)?, offset, data).map_err(code)?;
        if !written_out.is_null() {
            *written_out = n;
        }
        Ok(())
    })
}

// ---- kernel-offloaded I/O ---------------------------------------------

pub const EXTENT_DATA: u8 = 0;
pub const EXTENT_HOLE: u8 = 1;
pub const EXTENT_UNWRITTEN: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ext4fs_extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub kind: u8,
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_blockmap(
    vol: *mut ext4fs_volume,
    ino: u64,
    offset: u64,
    length: u64,
    write: bool,
    out: *mut ext4fs_extent,
    cap: usize,
    count_out: *mut usize,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        if out.is_null() || cap == 0 || count_out.is_null() {
            return Err(EINVAL);
        }
        let ino = ino32(ino)?;
        let runs = if write {
            fs.map_for_write(ino, offset, length, cap)
        } else {
            fs.map_for_read(ino, offset, length, cap)
        }
        .map_err(code)?;
        let out = std::slice::from_raw_parts_mut(out, cap);
        for (slot, r) in out.iter_mut().zip(&runs) {
            *slot = ext4fs_extent {
                logical: r.logical,
                physical: r.physical,
                length: r.len,
                kind: match r.kind {
                    IoKind::Data => EXTENT_DATA,
                    IoKind::Hole => EXTENT_HOLE,
                    IoKind::Unwritten => EXTENT_UNWRITTEN,
                },
            };
        }
        *count_out = runs.len().min(cap);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_complete_write(
    vol: *mut ext4fs_volume,
    ino: u64,
    offset: u64,
    length: u64,
) -> i32 {
    guard(|| {
        vol_ref(vol)?
            .complete_kernel_write(ino32(ino)?, offset, length)
            .map_err(code)
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_readlink(
    vol: *mut ext4fs_volume,
    ino: u64,
    buf: *mut u8,
    cap: usize,
    len_out: *mut usize,
) -> i32 {
    guard(|| {
        let target = vol_ref(vol)?.readlink(ino32(ino)?).map_err(code)?;
        out_buf(&target, buf, cap, len_out)
    })
}

// ---- namespace ---------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn ext4fs_create(
    vol: *mut ext4fs_volume,
    dir: u64,
    name: *const u8,
    name_len: usize,
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    out: *mut ext4fs_attr,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        let major = (rdev >> 24) & 0xFF;
        let minor = rdev & 0xFF_FFFF;
        let a = fs
            .create(
                ino32(dir)?,
                bytes(name, name_len)?,
                (mode & 0xFFFF) as u16,
                uid,
                gid,
                (major, minor),
            )
            .map_err(code)?;
        put_attr(out, &a);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_symlink(
    vol: *mut ext4fs_volume,
    dir: u64,
    name: *const u8,
    name_len: usize,
    target: *const u8,
    target_len: usize,
    uid: u32,
    gid: u32,
    out: *mut ext4fs_attr,
) -> i32 {
    guard(|| {
        let fs = vol_ref(vol)?;
        let a = fs
            .symlink(
                ino32(dir)?,
                bytes(name, name_len)?,
                bytes(target, target_len)?,
                uid,
                gid,
            )
            .map_err(code)?;
        put_attr(out, &a);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_link(
    vol: *mut ext4fs_volume,
    ino: u64,
    dir: u64,
    name: *const u8,
    name_len: usize,
    out: *mut ext4fs_attr,
) -> i32 {
    guard(|| {
        let a = vol_ref(vol)?
            .link(ino32(ino)?, ino32(dir)?, bytes(name, name_len)?)
            .map_err(code)?;
        put_attr(out, &a);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_remove(
    vol: *mut ext4fs_volume,
    dir: u64,
    name: *const u8,
    name_len: usize,
) -> i32 {
    guard(|| {
        vol_ref(vol)?
            .remove(ino32(dir)?, bytes(name, name_len)?)
            .map(|_| ())
            .map_err(code)
    })
}

const RENAME_NOREPLACE: u32 = 0x1;
const RENAME_EXCHANGE: u32 = 0x2;

#[no_mangle]
pub unsafe extern "C" fn ext4fs_rename(
    vol: *mut ext4fs_volume,
    src_dir: u64,
    src_name: *const u8,
    src_len: usize,
    dst_dir: u64,
    dst_name: *const u8,
    dst_len: usize,
    flags: u32,
) -> i32 {
    guard(|| {
        vol_ref(vol)?
            .rename(
                ino32(src_dir)?,
                bytes(src_name, src_len)?,
                ino32(dst_dir)?,
                bytes(dst_name, dst_len)?,
                flags & RENAME_NOREPLACE != 0,
                flags & RENAME_EXCHANGE != 0,
            )
            .map_err(code)
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_reclaim(vol: *mut ext4fs_volume, ino: u64) -> i32 {
    guard(|| vol_ref(vol)?.reclaim(ino32(ino)?).map_err(code))
}

// ---- xattrs ------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn ext4fs_listxattr(
    vol: *mut ext4fs_volume,
    ino: u64,
    buf: *mut u8,
    cap: usize,
    len_out: *mut usize,
) -> i32 {
    guard(|| {
        let names = vol_ref(vol)?.list_xattrs(ino32(ino)?).map_err(code)?;
        let mut joined = Vec::new();
        for n in names {
            joined.extend_from_slice(&n);
            joined.push(0);
        }
        out_buf(&joined, buf, cap, len_out)
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_getxattr(
    vol: *mut ext4fs_volume,
    ino: u64,
    name: *const u8,
    name_len: usize,
    buf: *mut u8,
    cap: usize,
    len_out: *mut usize,
) -> i32 {
    guard(|| {
        let value = vol_ref(vol)?
            .get_xattr(ino32(ino)?, bytes(name, name_len)?)
            .map_err(code)?;
        out_buf(&value, buf, cap, len_out)
    })
}

const XATTR_CREATE: u32 = 0x1;
const XATTR_REPLACE: u32 = 0x2;

#[no_mangle]
pub unsafe extern "C" fn ext4fs_setxattr(
    vol: *mut ext4fs_volume,
    ino: u64,
    name: *const u8,
    name_len: usize,
    value: *const u8,
    value_len: usize,
    flags: u32,
) -> i32 {
    guard(|| {
        vol_ref(vol)?
            .set_xattr(
                ino32(ino)?,
                bytes(name, name_len)?,
                bytes(value, value_len)?,
                flags & XATTR_CREATE != 0,
                flags & XATTR_REPLACE != 0,
            )
            .map_err(code)
    })
}

#[no_mangle]
pub unsafe extern "C" fn ext4fs_removexattr(
    vol: *mut ext4fs_volume,
    ino: u64,
    name: *const u8,
    name_len: usize,
) -> i32 {
    guard(|| {
        vol_ref(vol)?
            .remove_xattr(ino32(ino)?, bytes(name, name_len)?)
            .map_err(code)
    })
}

// ---- format and check --------------------------------------------------

#[repr(C)]
pub struct ext4fs_mkfs_options {
    pub label: *const c_char,
    pub revision: u32,
    pub block_size: u32,
    pub inode_size: u32,
    pub inode_ratio: u32,
    pub reserved_percent: u32,
}

pub type ext4fs_progress_fn = Option<unsafe extern "C" fn(*mut c_void, f64)>;
pub type ext4fs_message_fn = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

#[no_mangle]
pub unsafe extern "C" fn ext4fs_mkfs(
    dev: *const ext4fs_device,
    opts: *const ext4fs_mkfs_options,
    progress: ext4fs_progress_fn,
    progress_ctx: *mut c_void,
    msg: *mut c_char,
    msg_cap: usize,
) -> i32 {
    guard(|| {
        let dev = device_from(dev)?;
        let mut o = MkfsOptions::default();
        if !opts.is_null() {
            let c = &*opts;
            if !c.label.is_null() {
                o.label = CStr::from_ptr(c.label)
                    .to_str()
                    .map_err(|_| EINVAL)?
                    .to_owned();
            }
            o.revision = match c.revision {
                2 => Revision::Ext2,
                3 => Revision::Ext3,
                0 | 4 => Revision::Ext4,
                _ => {
                    write_msg(msg, msg_cap, "revision must be 2, 3, or 4");
                    return Err(EINVAL);
                }
            };
            o.block_size = (c.block_size != 0).then_some(c.block_size);
            o.inode_size = match c.inode_size {
                0 => None,
                n => Some(u16::try_from(n).map_err(|_| EINVAL)?),
            };
            o.inode_ratio = (c.inode_ratio != 0).then_some(c.inode_ratio);
            o.reserved_percent = (c.reserved_percent != u32::MAX).then_some(c.reserved_percent);
        }
        let mut report = |f: f64| {
            if let Some(p) = progress {
                // SAFETY: caller-supplied callback and context.
                unsafe { p(progress_ctx, f) }
            }
        };
        let aligned = ext4_core::device::AlignedDevice::new(dev);
        match format(&aligned, &o, &mut report) {
            Ok(r) => {
                write_msg(
                    msg,
                    msg_cap,
                    &format!(
                        "created {} blocks of {} bytes, {} inodes",
                        r.blocks, r.block_size, r.inodes
                    ),
                );
                Ok(())
            }
            Err(e) => {
                write_msg(msg, msg_cap, &e.to_string());
                Err(code(e))
            }
        }
    })
}

const CHECK_REPAIR: u32 = 0x1;
const CHECK_QUICK: u32 = 0x2;
const CHECK_FORCE: u32 = 0x4;

#[no_mangle]
pub unsafe extern "C" fn ext4fs_check(
    dev: *const ext4fs_device,
    flags: u32,
    message: ext4fs_message_fn,
    message_ctx: *mut c_void,
    progress: ext4fs_progress_fn,
    progress_ctx: *mut c_void,
) -> i32 {
    guard(|| {
        let dev = device_from(dev)?;
        let send = |text: &str| {
            if let Some(m) = message {
                let mut owned: Vec<u8> = text.bytes().filter(|&b| b != 0).collect();
                owned.push(0);
                // SAFETY: NUL-terminated buffer valid for the call.
                unsafe { m(message_ctx, owned.as_ptr().cast()) }
            }
        };
        let mut msg_fn = |s: &str| send(s);
        let mut prog = |f: f64| {
            if let Some(p) = progress {
                // SAFETY: caller-supplied callback and context.
                unsafe { p(progress_ctx, f) }
            }
        };
        let opts = CheckOptions {
            repair: flags & CHECK_REPAIR != 0,
            quick: flags & CHECK_QUICK != 0,
            force: flags & CHECK_FORCE != 0,
        };
        match check(Box::new(dev), &opts, &mut msg_fn, &mut prog) {
            Ok(r) if r.clean => Ok(()),
            Ok(_) => Err(EIO),
            Err(e) => {
                send(&e.to_string());
                Err(code(e))
            }
        }
    })
}
