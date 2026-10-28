//! Deterministic mutation stress coverage with an in-memory content model.

mod support;

use std::collections::BTreeMap;

use ext4_core::ops::SetAttr;
use support::*;

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn check_model(fs: &ext4_core::Filesystem, model: &BTreeMap<String, Vec<u8>>) {
    for (name, expected) in model {
        assert_eq!(
            read_all(fs, resolve(fs, name)),
            *expected,
            "content for {name}"
        );
    }
}

#[test]
fn randomized_root_file_operations_stay_consistent() {
    if !oracle_or_skip("randomized_root_file_operations_stay_consistent") {
        return;
    }
    let operations = std::env::var("EXT4_STRESS_OPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500usize);
    let (abs, rel) = mke2fs("interop-stress", "96M", &["-t", "ext4", "-b", "4096"]);
    let fs = mount(&abs, false);
    let mut model = BTreeMap::<String, Vec<u8>>::new();
    let mut state = 0xBADC_0FFE_EE12_3456u64;
    let mut next_name = 0usize;

    for step in 0..operations {
        let choice = next(&mut state) % 6;
        if model.is_empty() || choice == 0 {
            let name = format!("file-{next_name}");
            next_name += 1;
            let inode = fs
                .create(2, name.as_bytes(), 0o100644, 0, 0, (0, 0))
                .unwrap();
            if choice & 1 != 0 {
                let value = pattern((next(&mut state) % 3000) as usize, next(&mut state));
                fs.write(inode.ino, 0, &value).unwrap();
                model.insert(name, value);
            } else {
                model.insert(name, Vec::new());
            }
        } else {
            let index = (next(&mut state) as usize) % model.len();
            let name = model.keys().nth(index).unwrap().clone();
            let inode = resolve(&fs, &name);
            match choice {
                1 => {
                    let offset = (next(&mut state) % 24_000) as usize;
                    let value = pattern((next(&mut state) % 4096) as usize, next(&mut state));
                    fs.write(inode, offset as u64, &value).unwrap();
                    let expected = model.get_mut(&name).unwrap();
                    if expected.len() < offset + value.len() {
                        expected.resize(offset + value.len(), 0);
                    }
                    expected[offset..offset + value.len()].copy_from_slice(&value);
                }
                2 => {
                    let size = (next(&mut state) % 24_000) as usize;
                    fs.setattr(
                        inode,
                        &SetAttr {
                            size: Some(size as u64),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    model.get_mut(&name).unwrap().resize(size, 0);
                }
                3 => {
                    let suffix = format!("-{}", next(&mut state));
                    fs.set_xattr(inode, b"com.apple.stress", suffix.as_bytes(), false, false)
                        .unwrap();
                    assert_eq!(
                        fs.get_xattr(inode, b"com.apple.stress").unwrap(),
                        suffix.as_bytes()
                    );
                }
                4 => {
                    let destination = format!("file-{next_name}");
                    next_name += 1;
                    fs.rename(2, name.as_bytes(), 2, destination.as_bytes(), true, false)
                        .unwrap();
                    let value = model.remove(&name).unwrap();
                    model.insert(destination, value);
                }
                _ => {
                    let removed = fs.remove(2, name.as_bytes()).unwrap();
                    fs.reclaim(removed).unwrap();
                    model.remove(&name);
                }
            }
        }
        if step % 25 == 24 {
            fs.sync().unwrap();
            check_model(&fs, &model);
        }
    }
    fs.sync().unwrap();
    check_model(&fs, &model);
    fs.unmount().unwrap();
    assert_fsck_clean(&rel);
}
