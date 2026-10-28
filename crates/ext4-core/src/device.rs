//! Block device abstraction.
//!
//! The engine only needs positioned reads and writes, a flush barrier, and
//! the device size. Platform shims (FSKit, raw `/dev/diskN`, image files)
//! implement [`BlockDevice`]. [`AlignedDevice`] adapts devices that require
//! sector-aligned transfers.

use std::fs::File;
use std::io;
use std::sync::Mutex;

pub trait BlockDevice: Send + Sync {
    /// Read exactly `buf.len()` bytes at byte `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Write all of `buf` at byte `offset`.
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Make all completed writes durable.
    fn flush(&self) -> io::Result<()>;
    /// Device size in bytes.
    fn size(&self) -> u64;
    /// Required transfer alignment in bytes (1 = none).
    fn alignment(&self) -> u32 {
        1
    }
    /// True when the device refuses writes.
    fn read_only(&self) -> bool {
        false
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn alignment(&self) -> u32 {
        (**self).alignment()
    }
    fn read_only(&self) -> bool {
        (**self).read_only()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for std::sync::Arc<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn alignment(&self) -> u32 {
        (**self).alignment()
    }
    fn read_only(&self) -> bool {
        (**self).read_only()
    }
}

/// A regular file or raw device node accessed with `pread`/`pwrite`.
pub struct FileDevice {
    file: File,
    size: u64,
    alignment: u32,
    read_only: bool,
}

impl FileDevice {
    pub fn open(path: impl AsRef<std::path::Path>, writable: bool) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)?;
        Self::from_file(file, writable)
    }

    pub fn from_file(file: File, writable: bool) -> io::Result<Self> {
        let (size, alignment) = device_geometry(&file)?;
        Ok(FileDevice {
            file,
            size,
            alignment,
            read_only: !writable,
        })
    }

    /// Override the detected size (for example to address a partition inside an image).
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }
}

#[cfg(unix)]
fn device_geometry(file: &File) -> io::Result<(u64, u32)> {
    use std::os::unix::fs::FileTypeExt;
    let meta = file.metadata()?;
    let ft = meta.file_type();
    if ft.is_block_device() || ft.is_char_device() {
        raw_device_geometry(file)
    } else {
        Ok((meta.len(), 1))
    }
}

#[cfg(not(unix))]
fn device_geometry(file: &File) -> io::Result<(u64, u32)> {
    Ok((file.metadata()?.len(), 1))
}

#[cfg(target_os = "macos")]
fn raw_device_geometry(file: &File) -> io::Result<(u64, u32)> {
    use std::os::fd::AsRawFd;
    // <sys/disk.h>: DKIOCGETBLOCKSIZE = _IOR('d', 24, uint32_t),
    //               DKIOCGETBLOCKCOUNT = _IOR('d', 25, uint64_t).
    const DKIOCGETBLOCKSIZE: std::ffi::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: std::ffi::c_ulong = 0x4008_6419;
    extern "C" {
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    let fd = file.as_raw_fd();
    let mut bs: u32 = 0;
    let mut count: u64 = 0;
    // SAFETY: the ioctls write exactly one u32 / u64 into the provided pointers.
    unsafe {
        if ioctl(fd, DKIOCGETBLOCKSIZE, &mut bs as *mut u32) != 0 {
            return Err(io::Error::last_os_error());
        }
        if ioctl(fd, DKIOCGETBLOCKCOUNT, &mut count as *mut u64) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((count * bs as u64, bs.max(1)))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn raw_device_geometry(file: &File) -> io::Result<(u64, u32)> {
    use std::io::{Seek, SeekFrom};
    let mut f = file.try_clone()?;
    let size = f.seek(SeekFrom::End(0))?;
    Ok((size, 512))
}

impl BlockDevice for FileDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        if self.read_only {
            return Err(io::Error::from_raw_os_error(30));
        }
        self.file.write_all_at(buf, offset)
    }
    fn flush(&self) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        full_fsync(&self.file)
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn alignment(&self) -> u32 {
        self.alignment
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
}

#[cfg(target_os = "macos")]
fn full_fsync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;
    const F_FULLFSYNC: i32 = 51;
    // <sys/disk.h>: DKIOCSYNCHRONIZE = _IOW('d', 22, dk_synchronize_t) with a
    // 24-byte struct; DKIOCSYNCHRONIZECACHE = _IO('d', 22) is the older form.
    const DKIOCSYNCHRONIZE: std::ffi::c_ulong = 0x8018_6416;
    const DKIOCSYNCHRONIZECACHE: std::ffi::c_ulong = 0x2000_6416;
    const ENOTTY: i32 = 25;
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    let fd = file.as_raw_fd();
    let is_device = file
        .metadata()
        .map(|m| m.file_type().is_block_device() || m.file_type().is_char_device())?;
    if is_device {
        // Raw disks reject fsync/F_FULLFSYNC (ENOTTY); ask the driver to
        // flush the drive's write cache instead. A zeroed request covers the
        // whole medium.
        let mut request = [0u8; 24];
        // SAFETY: the ioctl reads exactly the 24-byte dk_synchronize_t.
        if unsafe { ioctl(fd, DKIOCSYNCHRONIZE, request.as_mut_ptr()) } == 0 {
            return Ok(());
        }
        // SAFETY: DKIOCSYNCHRONIZECACHE takes no argument.
        if unsafe { ioctl(fd, DKIOCSYNCHRONIZECACHE) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        // Some virtual devices (disk images) implement neither; their writes
        // already reached the backing file.
        return if err.raw_os_error() == Some(ENOTTY) {
            Ok(())
        } else {
            Err(err)
        };
    }
    // SAFETY: F_FULLFSYNC takes no argument.
    if unsafe { fcntl(fd, F_FULLFSYNC) } == 0 {
        Ok(())
    } else {
        // Some file systems reject F_FULLFSYNC; fall back.
        file.sync_all()
    }
}

#[cfg(not(target_os = "macos"))]
fn full_fsync(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// An in-memory device, used by tests and by tools that build images.
pub struct MemDevice {
    data: Mutex<Vec<u8>>,
}

impl MemDevice {
    pub fn new(size: usize) -> Self {
        MemDevice {
            data: Mutex::new(vec![0; size]),
        }
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        MemDevice {
            data: Mutex::new(data),
        }
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.data.into_inner().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.data.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl BlockDevice for MemDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let start = offset as usize;
        let end = start.checked_add(buf.len()).filter(|&e| e <= data.len());
        match end {
            Some(end) => {
                buf.copy_from_slice(&data[start..end]);
                Ok(())
            }
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past end of device",
            )),
        }
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let start = offset as usize;
        let end = start.checked_add(buf.len()).filter(|&e| e <= data.len());
        match end {
            Some(end) => {
                data[start..end].copy_from_slice(buf);
                Ok(())
            }
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "write past end of device",
            )),
        }
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap_or_else(|e| e.into_inner()).len() as u64
    }
}

/// Adapts a device with a transfer alignment requirement so callers may use
/// any offset and length. Unaligned edges use read-modify-write.
pub struct AlignedDevice<D: BlockDevice> {
    inner: D,
    align: u64,
}

impl<D: BlockDevice> AlignedDevice<D> {
    pub fn new(inner: D) -> Self {
        let align = inner.alignment().max(1) as u64;
        AlignedDevice { inner, align }
    }

    pub fn inner(&self) -> &D {
        &self.inner
    }

    fn span(&self, offset: u64, len: usize) -> (u64, usize) {
        let start = offset / self.align * self.align;
        let end = (offset + len as u64).div_ceil(self.align) * self.align;
        (start, (end - start) as usize)
    }
}

impl<D: BlockDevice> BlockDevice for AlignedDevice<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if self.align == 1 || (offset % self.align == 0 && buf.len() as u64 % self.align == 0) {
            return self.inner.read_at(offset, buf);
        }
        let (start, len) = self.span(offset, buf.len());
        let mut tmp = vec![0u8; len];
        self.inner.read_at(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        buf.copy_from_slice(&tmp[skip..skip + buf.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.align == 1 || (offset % self.align == 0 && buf.len() as u64 % self.align == 0) {
            return self.inner.write_at(offset, buf);
        }
        let (start, len) = self.span(offset, buf.len());
        let mut tmp = vec![0u8; len];
        self.inner.read_at(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        tmp[skip..skip + buf.len()].copy_from_slice(buf);
        self.inner.write_at(start, &tmp)
    }

    fn flush(&self) -> io::Result<()> {
        self.inner.flush()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn alignment(&self) -> u32 {
        1
    }
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Strict(MemDevice, u32);
    impl BlockDevice for Strict {
        fn read_at(&self, o: u64, b: &mut [u8]) -> io::Result<()> {
            assert!(
                o % self.1 as u64 == 0 && b.len() % self.1 as usize == 0,
                "unaligned read"
            );
            self.0.read_at(o, b)
        }
        fn write_at(&self, o: u64, b: &[u8]) -> io::Result<()> {
            assert!(
                o % self.1 as u64 == 0 && b.len() % self.1 as usize == 0,
                "unaligned write"
            );
            self.0.write_at(o, b)
        }
        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            self.0.size()
        }
        fn alignment(&self) -> u32 {
            self.1
        }
    }

    #[test]
    fn aligned_device_handles_unaligned_edges() {
        let dev = AlignedDevice::new(Strict(MemDevice::new(8192), 4096));
        dev.write_at(1000, &[7u8; 5000]).unwrap();
        let mut back = vec![0u8; 8192];
        dev.read_at(0, &mut back).unwrap();
        assert!(back[..1000].iter().all(|&b| b == 0));
        assert!(back[1000..6000].iter().all(|&b| b == 7));
        assert!(back[6000..].iter().all(|&b| b == 0));
        let mut mid = [0u8; 3];
        dev.read_at(999, &mut mid).unwrap();
        assert_eq!(mid, [0, 7, 7]);
    }
}
