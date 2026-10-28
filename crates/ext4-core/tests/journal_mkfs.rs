//! Journal checksum interoperability with images produced by this formatter.

use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::{Filesystem, MemDevice, MountOptions};

#[test]
fn mounts_fresh_csum_v3_formatter_image_read_write() {
    let device = MemDevice::new(64 * 1024 * 1024);
    let options = MkfsOptions {
        revision: Revision::Ext4,
        block_size: Some(4096),
        uuid: Some(*b"journal-csum-v3!"),
        ..Default::default()
    };
    format(&device, &options, &mut |_| {}).unwrap();

    let fs = Filesystem::open(Box::new(device), &MountOptions::default()).unwrap();
    assert!(!fs.is_read_only(), "{:?}", fs.read_only_reason());
    let file = fs.create(2, b"verified", 0o100644, 0, 0, (0, 0)).unwrap();
    fs.write(file.ino, 0, b"journal superblock checksum")
        .unwrap();
    fs.sync().unwrap();
}
