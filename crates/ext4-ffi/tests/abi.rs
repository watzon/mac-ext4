//! Exercise the C ABI end to end over an image file: mkfs, mount, create,
//! write, read back, readdir, xattrs, rename, remove, reclaim, unmount.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;

use ext4fs::*;

unsafe extern "C" fn dev_read(ctx: *mut c_void, off: u64, buf: *mut c_void, len: usize) -> i32 {
    let f = &*(ctx as *const File);
    let s = std::slice::from_raw_parts_mut(buf.cast::<u8>(), len);
    match f.read_exact_at(s, off) {
        Ok(()) => 0,
        Err(e) => e.raw_os_error().unwrap_or(5),
    }
}

unsafe extern "C" fn dev_write(ctx: *mut c_void, off: u64, buf: *const c_void, len: usize) -> i32 {
    let f = &*(ctx as *const File);
    let s = std::slice::from_raw_parts(buf.cast::<u8>(), len);
    // A 4 KiB-sector device: the engine must only send aligned transfers.
    assert!(
        off % 4096 == 0 && len % 4096 == 0,
        "unaligned write {off}+{len}"
    );
    match f.write_all_at(s, off) {
        Ok(()) => 0,
        Err(e) => e.raw_os_error().unwrap_or(5),
    }
}

unsafe extern "C" fn dev_flush(ctx: *mut c_void) -> i32 {
    let f = &*(ctx as *const File);
    match f.sync_all() {
        Ok(()) => 0,
        Err(_) => 5,
    }
}

fn device(file: &File, size: u64) -> ext4fs_device {
    ext4fs_device {
        ctx: file as *const File as *mut c_void,
        read: Some(dev_read),
        write: Some(dev_write),
        flush: Some(dev_flush),
        size,
        alignment: 4096,
        read_only: false,
    }
}

unsafe extern "C" fn collect(
    ctx: *mut c_void,
    name: *const u8,
    len: usize,
    ino: u64,
    _t: u8,
    _next: u64,
    attr: *const ext4fs_attr,
) -> bool {
    let v = &mut *(ctx as *mut Vec<(String, u64, Option<ext4fs_attr>)>);
    v.push((
        String::from_utf8_lossy(std::slice::from_raw_parts(name, len)).into_owned(),
        ino,
        attr.as_ref().copied(),
    ));
    true
}

#[test]
fn abi_round_trip() {
    let path = std::env::temp_dir().join(format!("ext4fs-abi-{}.img", std::process::id()));
    let size = 64u64 << 20;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.set_len(size).unwrap();
    let dev = device(&file, size);
    unsafe {
        let label = c"abi";
        let opts = ext4fs_mkfs_options {
            label: label.as_ptr(),
            revision: 4,
            block_size: 0,
            inode_size: 0,
            inode_ratio: 0,
            reserved_percent: u32::MAX,
        };
        let mut msg = [0 as std::ffi::c_char; 256];
        assert_eq!(
            ext4fs_mkfs(
                &dev,
                &opts,
                None,
                std::ptr::null_mut(),
                msg.as_mut_ptr(),
                msg.len()
            ),
            0
        );

        let mut info: ext4fs_probe_info = std::mem::zeroed();
        assert_eq!(ext4fs_probe(&dev, &mut info, std::ptr::null_mut(), 0), 0);
        assert!(info.writable);

        let mut vol: *mut ext4fs_volume = std::ptr::null_mut();
        let mut ro = true;
        let rc = ext4fs_mount(&dev, 0, &mut vol, &mut ro, msg.as_mut_ptr(), msg.len());
        assert_eq!(rc, 0, "{:?}", std::ffi::CStr::from_ptr(msg.as_ptr()));
        assert!(!ro);

        let mut st: ext4fs_statfs = std::mem::zeroed();
        assert_eq!(ext4fs_statfs_get(vol, &mut st), 0);
        // The module reports this as its maximum file size, and macOS turns
        // it into FILESIZEBITS. ext4 with extents and huge_file allows
        // 2^32 - 1 blocks.
        assert_eq!(
            st.max_file_size,
            u64::from(u32::MAX) * u64::from(st.block_size)
        );
        assert_eq!(
            std::ffi::CStr::from_ptr(st.label.as_ptr())
                .to_str()
                .unwrap(),
            "abi"
        );

        let mut a = ext4fs_attr::default();
        let name = b"file.txt";
        assert_eq!(
            ext4fs_create(
                vol,
                2,
                name.as_ptr(),
                name.len(),
                0o100644,
                501,
                20,
                0,
                &mut a
            ),
            0
        );
        let data = b"hello through the C ABI";
        let mut n = 0usize;
        assert_eq!(
            ext4fs_write(vol, a.ino, 0, data.as_ptr().cast(), data.len(), &mut n),
            0
        );
        assert_eq!(n, data.len());
        let mut back = [0u8; 64];
        assert_eq!(
            ext4fs_read(vol, a.ino, 0, back.as_mut_ptr().cast(), back.len(), &mut n),
            0
        );
        assert_eq!(&back[..n], data);

        // Kernel-offloaded write: map past EOF, put the data on the device
        // at the mapped offset, complete, and read it back.
        assert!(a.kernel_io, "extent files use kernel-offloaded I/O");
        let more = [7u8; 8192];
        let mut extents = [ext4fs_extent::default(); 8];
        let mut count = 0usize;
        assert_eq!(
            ext4fs_blockmap(
                vol,
                a.ino,
                4096,
                more.len() as u64,
                true,
                extents.as_mut_ptr(),
                8,
                &mut count
            ),
            0
        );
        assert!(count >= 1);
        assert_eq!(extents[0].logical, 4096);
        assert_eq!(extents[0].kind, EXTENT_UNWRITTEN);
        file.write_all_at(&more, extents[0].physical).unwrap();
        assert_eq!(
            ext4fs_complete_write(vol, a.ino, 4096, more.len() as u64),
            0
        );
        let mut big = vec![0u8; 4096 + more.len()];
        assert_eq!(
            ext4fs_read(vol, a.ino, 0, big.as_mut_ptr().cast(), big.len(), &mut n),
            0
        );
        assert_eq!(n, big.len());
        assert!(big[data.len()..4096].iter().all(|&b| b == 0));
        assert_eq!(&big[4096..], &more[..]);
        assert_eq!(
            ext4fs_blockmap(
                vol,
                a.ino,
                0,
                big.len() as u64,
                false,
                extents.as_mut_ptr(),
                8,
                &mut count
            ),
            0
        );
        // 1 KiB blocks: block 0 holds the first write, then a hole up to
        // the kernel-written range.
        let got: Vec<_> = extents[..count]
            .iter()
            .map(|e| (e.logical, e.length, e.kind))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, 1024, EXTENT_DATA),
                (1024, 3072, EXTENT_HOLE),
                (4096, more.len() as u64, EXTENT_DATA)
            ]
        );

        // Duplicate create fails with EEXIST.
        assert_eq!(
            ext4fs_create(vol, 2, name.as_ptr(), name.len(), 0o100644, 0, 0, 0, &mut a),
            17
        );

        let xn = b"com.apple.quarantine";
        let xv = b"0081;12345678;Safari;";
        assert_eq!(
            ext4fs_setxattr(
                vol,
                a.ino,
                xn.as_ptr(),
                xn.len(),
                xv.as_ptr(),
                xv.len(),
                0x1
            ),
            0
        );
        let mut len = 0usize;
        assert_eq!(
            ext4fs_getxattr(
                vol,
                a.ino,
                xn.as_ptr(),
                xn.len(),
                std::ptr::null_mut(),
                0,
                &mut len
            ),
            0
        );
        assert_eq!(len, xv.len());
        let mut small = [0u8; 4];
        assert_eq!(
            ext4fs_getxattr(
                vol,
                a.ino,
                xn.as_ptr(),
                xn.len(),
                small.as_mut_ptr(),
                small.len(),
                &mut len
            ),
            34
        );
        let mut names = [0u8; 64];
        assert_eq!(
            ext4fs_listxattr(vol, a.ino, names.as_mut_ptr(), names.len(), &mut len),
            0
        );
        assert_eq!(&names[..len], b"com.apple.quarantine\0");

        let dn = b"dir";
        let mut d = ext4fs_attr::default();
        assert_eq!(
            ext4fs_create(vol, 2, dn.as_ptr(), dn.len(), 0o040755, 0, 0, 0, &mut d),
            0
        );
        assert_eq!(d.parent_ino, 2);
        assert_eq!(
            ext4fs_rename(
                vol,
                2,
                name.as_ptr(),
                name.len(),
                d.ino,
                name.as_ptr(),
                name.len(),
                0
            ),
            0
        );

        let mut entries: Vec<(String, u64, Option<ext4fs_attr>)> = Vec::new();
        let mut verifier = 0u64;
        assert_eq!(
            ext4fs_readdir(
                vol,
                d.ino,
                0,
                true,
                Some(collect),
                (&mut entries as *mut Vec<_>).cast(),
                &mut verifier
            ),
            0
        );
        let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(names, vec![".", "..", "file.txt"]);
        // Attributes come with the entries, except for "." and "..".
        assert!(entries[0].2.is_none() && entries[1].2.is_none());
        let file_attr = entries[2].2.expect("attributes for file.txt");
        let mut direct = ext4fs_attr::default();
        assert_eq!(ext4fs_getattr(vol, entries[2].1, &mut direct), 0);
        assert_eq!(
            (file_attr.ino, file_attr.size, file_attr.mode),
            (direct.ino, direct.size, direct.mode)
        );
        assert_ne!(verifier, 0);

        // Non-empty rmdir fails; unlink then reclaim frees the inode.
        assert_eq!(ext4fs_remove(vol, 2, dn.as_ptr(), dn.len()), 66);
        assert_eq!(ext4fs_remove(vol, d.ino, name.as_ptr(), name.len()), 0);
        assert_eq!(ext4fs_reclaim(vol, a.ino), 0);
        assert_eq!(ext4fs_getattr(vol, a.ino, &mut a), 2);
        assert_eq!(ext4fs_remove(vol, 2, dn.as_ptr(), dn.len()), 0);
        assert_eq!(ext4fs_unmount(vol), 0);

        // Remount read-only and check the clean state.
        let mut vol2: *mut ext4fs_volume = std::ptr::null_mut();
        assert_eq!(
            ext4fs_mount(&dev, 1, &mut vol2, &mut ro, std::ptr::null_mut(), 0),
            0
        );
        assert!(ro);
        let mut out = ext4fs_attr::default();
        assert_eq!(ext4fs_lookup(vol2, 2, dn.as_ptr(), dn.len(), &mut out), 2);
        assert_eq!(ext4fs_unmount(vol2), 0);
    }
    let _ = std::fs::remove_file(path);
}
