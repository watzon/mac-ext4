//! Mutation interoperability against e2fsprogs images.

mod support;

use ext4_core::ops::SetAttr;
use ext4_core::Error;
use support::*;

#[test]
fn namespace_data_and_xattr_mutations_remain_fsck_clean() {
    if !oracle_or_skip("namespace_data_and_xattr_mutations_remain_fsck_clean") {
        return;
    }
    let (abs, rel) = mke2fs("interop-operations", "64M", &["-t", "ext4", "-b", "4096"]);
    let mut expected: Vec<u8>;
    {
        let fs = mount(&abs, false);
        let left = fs.mkdir(2, b"left", 0o755, 501, 20).unwrap();
        let right = fs.mkdir(2, b"right", 0o755, 501, 20).unwrap();
        let file = fs
            .create(left.ino, b"file", 0o100644, 501, 20, (0, 0))
            .unwrap();

        let prefix = pattern(3000, 1);
        let middle = pattern(5000, 2);
        let suffix = pattern(2000, 3);
        fs.write(file.ino, 0, &prefix).unwrap();
        fs.write(file.ino, 8192, &middle).unwrap();
        fs.write(file.ino, 13192, &suffix).unwrap();
        expected = [
            prefix.as_slice(),
            &[0; 5192],
            middle.as_slice(),
            suffix.as_slice(),
        ]
        .concat();
        assert_eq!(read_all(&fs, file.ino), expected);

        fs.link(file.ino, right.ino, b"hard").unwrap();
        fs.symlink(right.ino, b"link", b"../left/file", 501, 20)
            .unwrap();
        fs.rename(left.ino, b"file", right.ino, b"moved", false, false)
            .unwrap();
        assert_eq!(
            fs.readlink(resolve(&fs, "right/link")).unwrap(),
            b"../left/file"
        );
        assert_eq!(read_all(&fs, resolve(&fs, "right/moved")), expected);

        fs.set_xattr(file.ino, b"com.apple.short", b"value", false, false)
            .unwrap();
        for n in 0..12 {
            let name = format!("com.apple.large-{n:02}");
            let value = pattern(180, n + 100);
            fs.set_xattr(file.ino, name.as_bytes(), &value, false, false)
                .unwrap();
            assert_eq!(fs.get_xattr(file.ino, name.as_bytes()).unwrap(), value);
        }
        fs.remove_xattr(file.ino, b"com.apple.large-03").unwrap();
        assert!(matches!(
            fs.get_xattr(file.ino, b"com.apple.large-03"),
            Err(Error::NoAttribute)
        ));

        fs.setattr(
            file.ino,
            &SetAttr {
                size: Some(9000),
                ..Default::default()
            },
        )
        .unwrap();
        let mut truncated = expected[..9000].to_vec();
        fs.setattr(
            file.ino,
            &SetAttr {
                size: Some(12000),
                ..Default::default()
            },
        )
        .unwrap();
        truncated.resize(12000, 0);
        assert_eq!(read_all(&fs, file.ino), truncated);

        assert!(matches!(
            fs.rename(2, b"left", left.ino, b"inside", false, false),
            Err(Error::InvalidArgument(_))
        ));
        let removed = fs.remove(right.ino, b"hard").unwrap();
        assert_eq!(removed, file.ino);
        fs.sync().unwrap();
        fs.unmount().unwrap();
        expected = truncated;
    }

    assert_fsck_clean(&rel);
    assert!(debugfs_r(&rel, "cat right/moved")
        .contains(String::from_utf8_lossy(&expected[..64]).as_ref()));
    let fs = mount(&abs, true);
    assert_eq!(read_all(&fs, resolve(&fs, "right/moved")), expected);
    assert_eq!(
        fs.get_xattr(resolve(&fs, "right/moved"), b"com.apple.short")
            .unwrap(),
        b"value"
    );
}
