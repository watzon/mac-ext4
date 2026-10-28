//! Consistency checker (fsck).
//!
//! The checker keeps the scan state per block group.  It does not retain inode
//! objects after their group has been visited, which keeps its memory use tied
//! to bitmap size rather than filesystem size.

use std::collections::{BTreeMap, HashSet};

use crate::bytes::{le32, set_bit, test_bit};
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::file::has_block_map;
use crate::group::{bitmap_checksum, bitmap_csum_matches, BG_BLOCK_UNINIT, BG_INODE_UNINIT};
use crate::inode::{Inode, RESIZE_INO, ROOT_INO};
use crate::superblock::{compat, incompat, ro_compat, STATE_ERROR, STATE_ORPHAN_FS, STATE_VALID};
use crate::volume::{read_superblock, MountOptions, Volume};
use crate::xattr::XATTR_MAGIC;

/// Options controlling a consistency check.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckOptions {
    /// Apply repairs for accounting, bitmap, and link-count errors.
    pub repair: bool,
    /// Perform only the superblock clean-state probe.
    pub quick: bool,
    /// With `repair`, scan the whole volume even when journal replay and
    /// orphan cleanup leave it clean. Without it, repair behaves like a
    /// preen: replay, clean up, and scan only if the error flag is set.
    pub force: bool,
}

/// Result of [`check`].
#[derive(Clone, Debug, Default)]
pub struct CheckReport {
    /// Problems remaining after the requested operation.
    pub problems: Vec<String>,
    /// Repairs committed to the volume.
    pub repaired: Vec<String>,
    /// True when no problems remain.
    pub clean: bool,
}

#[derive(Default)]
struct Scan {
    problems: Vec<String>,
    /// Supported fixes, each with the problem text it resolves.
    repairable: Vec<(Fix, String)>,
    block_maps: Vec<Vec<u8>>,
    inode_maps: Vec<Vec<u8>>,
    links: BTreeMap<u32, u32>,
    dirs: Vec<u32>,
    /// External xattr blocks and the number of inodes that point at them.
    xattr_refs: BTreeMap<u64, u32>,
    /// Some block owner could not be fully read, so `block_maps` may miss
    /// blocks that are in use.
    blocks_incomplete: bool,
    /// Some inode could not be read, so its type (and so directory counts)
    /// is unknown.
    inodes_incomplete: bool,
    /// Some directory could not be fully read, so `links` may undercount.
    links_incomplete: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Fix {
    BlockBitmap(u32),
    InodeBitmap(u32),
    GroupBlocks(u32),
    GroupInodes(u32),
    SuperBlocks,
    SuperInodes,
    Links(u32, u16),
}

/// Check an ext filesystem.  A read-only check never writes `dev`; repair mode
/// stages supported fixes through `Volume` and cleanly unmounts afterward.
pub fn check(
    dev: Box<dyn BlockDevice>,
    opts: &CheckOptions,
    message: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(f64),
) -> Result<CheckReport> {
    progress(0.0);
    if opts.quick {
        return quick_check(dev, message, progress);
    }

    // Ignore checksum failures at mount so the checker can name and, where
    // possible, repair the affected structure itself.
    let mount = MountOptions {
        read_only: !opts.repair,
        ignore_checksums: true,
    };
    let mut vol = Volume::open(dev, &mount)?;
    if opts.repair
        && !opts.force
        && vol.journal.is_some()
        && vol.sb.state() & STATE_ERROR == 0
        && !vol.is_read_only()
    {
        // Opening read-write replayed the journal and processed orphans,
        // which is all an unclean shutdown of a journaled volume needs.
        let mut repaired = Vec::new();
        let r = vol.recovery.clone();
        if r.journal_transactions_replayed > 0 {
            repaired.push(format!(
                "replayed {} journal transactions",
                r.journal_transactions_replayed
            ));
        }
        if r.orphans_processed > 0 {
            repaired.push(format!("processed {} orphaned inodes", r.orphans_processed));
        }
        for item in &repaired {
            message(item);
        }
        vol.unmount()?;
        progress(1.0);
        return Ok(CheckReport {
            problems: Vec::new(),
            repaired,
            clean: true,
        });
    }
    let mut scan = scan_volume(&vol, message, progress)?;
    let mut repaired = Vec::new();

    if opts.repair {
        apply_repairs(&mut vol, &mut scan, &mut repaired, message)?;
        if scan.problems.is_empty() {
            vol.sb.set_state(vol.sb.state() & !STATE_ERROR);
            vol.mark_sb_dirty();
            repaired.push("cleared filesystem error state".into());
        }
        vol.commit()?;
        vol.unmount()?;
    }

    progress(1.0);
    Ok(CheckReport {
        clean: scan.problems.is_empty(),
        problems: scan.problems,
        repaired,
    })
}

fn quick_check(
    dev: Box<dyn BlockDevice>,
    message: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(f64),
) -> Result<CheckReport> {
    let sb = read_superblock(&*dev)?;
    let mut problems = Vec::new();
    if !sb.verify_checksum() {
        report(&mut problems, message, "superblock checksum mismatch");
    }
    if sb.state() & STATE_VALID == 0 {
        report(&mut problems, message, "filesystem is not marked clean");
    }
    if sb.state() & STATE_ERROR != 0 {
        report(&mut problems, message, "filesystem has the error flag set");
    }
    if sb.has_incompat(incompat::RECOVER) && sb.has_compat(compat::HAS_JOURNAL) {
        report(&mut problems, message, "journal recovery is required");
    }
    if sb.last_orphan() != 0
        || sb.has_ro_compat(ro_compat::ORPHAN_PRESENT)
        || sb.state() & STATE_ORPHAN_FS != 0
    {
        report(&mut problems, message, "filesystem has orphaned inodes");
    }
    progress(1.0);
    Ok(CheckReport {
        clean: problems.is_empty(),
        problems,
        repaired: Vec::new(),
    })
}

fn scan_volume(
    vol: &Volume,
    message: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(f64),
) -> Result<Scan> {
    let mut scan = Scan::default();
    if !vol.sb.verify_checksum() {
        report(&mut scan.problems, message, "superblock checksum mismatch");
    }
    let groups = vol.group_count();
    scan.block_maps = (0..groups)
        .map(|g| initial_block_map(vol, g))
        .collect::<Result<_>>()?;
    scan.inode_maps = (0..groups)
        .map(|g| initial_inode_map(vol, g))
        .collect::<Result<_>>()?;
    mark_metadata(vol, &mut scan, message);

    for g in 0..groups {
        check_group_descriptor(vol, g, &mut scan, message);
        let on_disk_inode_map = raw_inode_map(vol, g)?;
        let inode_limit = vol.sb.inodes_per_group() as usize;
        for bit in 0..inode_limit {
            if !test_bit(&on_disk_inode_map, bit) {
                continue;
            }
            let ino = g * vol.sb.inodes_per_group() + bit as u32 + 1;
            if ino > vol.sb.inodes_count() {
                continue;
            }
            let inode = match vol.read_inode(ino) {
                Ok(inode) => inode,
                Err(Error::Corrupt(what)) => {
                    report(&mut scan.problems, message, format!("inode {ino}: {what}"));
                    // Its blocks, type, and directory entries are unknown.
                    // Keep it allocated and stop repairs that would need them.
                    set_bit(&mut scan.inode_maps[g as usize], bit);
                    scan.blocks_incomplete = true;
                    scan.inodes_incomplete = true;
                    scan.links_incomplete = true;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if inode.mode() == 0 {
                if ino >= vol.sb.first_ino() {
                    report(
                        &mut scan.problems,
                        message,
                        format!("inode {ino} is allocated but has no mode"),
                    );
                }
                continue;
            }
            set_bit(&mut scan.inode_maps[g as usize], bit);
            check_inode(vol, ino, &inode, &mut scan, message);
            if inode.is_dir() {
                scan.dirs.push(ino);
            }
        }
        progress(0.10 + 0.55 * (g + 1) as f64 / groups.max(1) as f64);
    }

    check_xattr_refcounts(vol, &mut scan, message);
    check_directories(vol, &mut scan, message);
    check_bitmaps_and_counts(vol, &mut scan, message);
    progress(0.85);
    Ok(scan)
}

fn check_group_descriptor(vol: &Volume, g: u32, scan: &mut Scan, message: &mut dyn FnMut(&str)) {
    let gd = &vol.groups[g as usize];
    if !gd.verify_checksum(&vol.sb, g) {
        report(
            &mut scan.problems,
            message,
            format!("group {g} descriptor checksum mismatch"),
        );
    }
}

fn initial_block_map(vol: &Volume, g: u32) -> Result<Vec<u8>> {
    let mut map = vec![0; vol.block_size()];
    let valid = vol.blocks_in_group(g) as usize;
    for bit in valid..map.len() * 8 {
        set_bit(&mut map, bit);
    }
    Ok(map)
}

fn initial_inode_map(vol: &Volume, _g: u32) -> Result<Vec<u8>> {
    let mut map = vec![0; vol.block_size()];
    for bit in vol.sb.inodes_per_group() as usize..map.len() * 8 {
        set_bit(&mut map, bit);
    }
    Ok(map)
}

fn mark_metadata(vol: &Volume, scan: &mut Scan, message: &mut dyn FnMut(&str)) {
    for g in 0..vol.group_count() {
        if vol.sb.group_has_super(g) {
            claim_block(vol, scan, vol.group_first_block(g), "superblock", message);
            if !vol.sb.has_incompat(incompat::META_BG) {
                for i in 0..vol.gdt_blocks() + vol.sb.reserved_gdt_blocks() as u32 {
                    claim_block(
                        vol,
                        scan,
                        vol.group_first_block(g) + 1 + i as u64,
                        "group descriptors",
                        message,
                    );
                }
            }
        }
    }
    if vol.sb.has_incompat(incompat::META_BG) {
        for i in 0..vol.gdt_blocks() {
            claim_block(vol, scan, vol.gdt_block(i), "group descriptors", message);
        }
    }
    let table_blocks = (vol.sb.inodes_per_group() as u64 * vol.sb.inode_size() as u64)
        .div_ceil(vol.block_size() as u64);
    if table_blocks > vol.sb.blocks_per_group() as u64 {
        report(
            &mut scan.problems,
            message,
            "inode table exceeds its block group",
        );
        scan.blocks_incomplete = true;
        return;
    }
    for gd in &vol.groups {
        claim_block(vol, scan, gd.block_bitmap(), "block bitmap", message);
        claim_block(vol, scan, gd.inode_bitmap(), "inode bitmap", message);
        for i in 0..table_blocks {
            let Some(block) = gd.inode_table().checked_add(i) else {
                report(
                    &mut scan.problems,
                    message,
                    "inode table block number overflow",
                );
                scan.blocks_incomplete = true;
                break;
            };
            claim_block(vol, scan, block, "inode table", message);
        }
    }
    // Reserved inodes are allocated even when their inode body is zero.
    for ino in 1..vol.sb.first_ino() {
        mark_inode(vol, scan, ino);
    }
}

fn check_inode(
    vol: &Volume,
    ino: u32,
    inode: &Inode,
    scan: &mut Scan,
    message: &mut dyn FnMut(&str),
) {
    if !inode.verify_checksum(&vol.sb, ino) {
        report(
            &mut scan.problems,
            message,
            format!("inode {ino} checksum mismatch"),
        );
    }
    // An external xattr block belongs to the inode whatever its type: fast
    // symlinks, device nodes, and inline-data files can have one too.
    let acl = inode.file_acl();
    if acl != 0 {
        claim_xattr_block(vol, scan, ino, acl, message);
    }
    // The resize inode owns reserved GDT blocks and its indirect-map blocks.
    // Primary GDT overlaps are tolerated by `claim_block`.
    if !has_block_map(vol, inode) {
        return;
    }
    let mapped = if inode.has_flag(crate::inode::FL_EXTENTS) {
        vol.read_extents(inode, ino).map(|(extents, nodes)| {
            let mut blocks = nodes;
            for extent in extents {
                for block in extent.physical..extent.physical + extent.len as u64 {
                    blocks.push(block);
                }
            }
            blocks
        })
    } else {
        let mut blocks = Vec::new();
        let walked = vol.blockmap_walk(inode, |_, block| {
            blocks.push(block);
            Ok(())
        });
        walked.map(|mut nodes| {
            nodes.extend(blocks);
            nodes
        })
    };
    match mapped {
        Ok(blocks) => {
            let owner = if ino == RESIZE_INO {
                "resize inode".to_owned()
            } else {
                format!("inode {ino}")
            };
            for block in blocks {
                claim_block(vol, scan, block, &owner, message);
            }
        }
        Err(e) => {
            let what = match e {
                Error::Corrupt(what) => what,
                e => e.to_string(),
            };
            report(
                &mut scan.problems,
                message,
                format!("inode {ino} block map: {what}"),
            );
            // Blocks behind the unreadable part of the map are still in
            // use as far as anyone knows.
            scan.blocks_incomplete = true;
        }
    }
}

/// Claim external xattr block `blk` for inode `ino`. A block that several
/// inodes share (`h_refcount` > 1) is claimed once; the reference count is
/// checked against the number of owners in [`check_xattr_refcounts`].
fn claim_xattr_block(
    vol: &Volume,
    scan: &mut Scan,
    ino: u32,
    blk: u64,
    message: &mut dyn FnMut(&str),
) {
    if let Some(owners) = scan.xattr_refs.get_mut(&blk) {
        *owners += 1;
        return;
    }
    if blk < vol.sb.first_data_block() as u64 || blk >= vol.sb.blocks_count() {
        report(
            &mut scan.problems,
            message,
            format!("inode {ino} xattr block {blk} is outside the filesystem"),
        );
        return;
    }
    claim_block(vol, scan, blk, &format!("inode {ino} xattr"), message);
    scan.xattr_refs.insert(blk, 1);
}

/// Compare each shared xattr block's `h_refcount` with its owner count.
fn check_xattr_refcounts(vol: &Volume, scan: &mut Scan, message: &mut dyn FnMut(&str)) {
    for (&blk, &owners) in &scan.xattr_refs {
        let header = vol.with_block(blk, |b| (le32(b, 0), le32(b, 4)));
        match header {
            Ok((magic, _)) if magic != XATTR_MAGIC => report(
                &mut scan.problems,
                message,
                format!("xattr block {blk} has a bad header"),
            ),
            Ok((_, refs)) if refs != owners => report(
                &mut scan.problems,
                message,
                format!("xattr block {blk} has reference count {refs}, expected {owners}"),
            ),
            Ok(_) => {}
            Err(e) => report(
                &mut scan.problems,
                message,
                format!("xattr block {blk}: {e}"),
            ),
        }
    }
}

fn check_directories(vol: &Volume, scan: &mut Scan, message: &mut dyn FnMut(&str)) {
    let in_use = |ino: u32, maps: &[Vec<u8>]| inode_marked(vol, maps, ino);
    for &dir in &scan.dirs {
        let mut dot = None;
        let mut dotdot = None;
        let result = vol.read_dir(dir, 0, |entry, _| {
            if entry.name == b"." {
                dot = Some(entry.inode);
            } else if entry.name == b".." {
                dotdot = Some(entry.inode);
                *scan.links.entry(entry.inode).or_default() += 1;
            } else {
                *scan.links.entry(entry.inode).or_default() += 1;
            }
            true
        });
        if let Err(e) = result {
            report(
                &mut scan.problems,
                message,
                format!("directory inode {dir}: {e}"),
            );
            // Entries past the failure are unknown, so counted links are
            // only a lower bound from here on.
            scan.links_incomplete = true;
            continue;
        }
        *scan.links.entry(dir).or_default() += 1; // "."
        if dot != Some(dir) {
            report(
                &mut scan.problems,
                message,
                format!("directory inode {dir} has invalid . entry"),
            );
        }
        let expected_parent = if dir == ROOT_INO {
            ROOT_INO
        } else {
            dotdot.unwrap_or(0)
        };
        if dotdot != Some(expected_parent)
            || expected_parent == 0
            || !in_use(expected_parent, &scan.inode_maps)
        {
            report(
                &mut scan.problems,
                message,
                format!("directory inode {dir} has invalid .. entry"),
            );
        }
    }
    if scan.links_incomplete {
        report(
            &mut scan.problems,
            message,
            "some directories could not be read; link counts will not be lowered",
        );
    }
    for (&ino, &count) in &scan.links {
        if !in_use(ino, &scan.inode_maps) {
            report(
                &mut scan.problems,
                message,
                format!("directory references free inode {ino}"),
            );
            continue;
        }
        match vol.read_inode(ino) {
            Ok(inode) if inode.mode() != 0 && inode.links_count() as u32 != count => {
                let problem = format!(
                    "inode {ino} has link count {}, expected {count}",
                    inode.links_count()
                );
                report(&mut scan.problems, message, problem.clone());
                let lowers = count < inode.links_count() as u32;
                if count <= u16::MAX as u32 && !(lowers && scan.links_incomplete) {
                    scan.repairable
                        .push((Fix::Links(ino, count as u16), problem));
                }
            }
            Ok(_) => {}
            Err(e) => report(&mut scan.problems, message, format!("inode {ino}: {e}")),
        }
    }
}

/// Report `problem` and remember `fix` as the repair that resolves it.
fn report_fix(scan: &mut Scan, message: &mut dyn FnMut(&str), problem: String, fix: Option<Fix>) {
    report(&mut scan.problems, message, problem.clone());
    if let Some(fix) = fix {
        scan.repairable.push((fix, problem));
    }
}

fn check_bitmaps_and_counts(vol: &Volume, scan: &mut Scan, message: &mut dyn FnMut(&str)) {
    if scan.blocks_incomplete {
        report(
            &mut scan.problems,
            message,
            "block ownership could not be fully determined; block bitmaps and free block counts will not be rebuilt",
        );
    }
    // Repairs derived from the scanned maps are only safe when the scan saw
    // every owner. Otherwise they could free blocks that are still in use.
    let blocks_ok = !scan.blocks_incomplete;
    let inodes_ok = !scan.inodes_incomplete;
    let mut free_blocks = 0u64;
    let mut free_inodes = 0u64;
    for g in 0..vol.group_count() {
        let gd = &vol.groups[g as usize];
        let actual_blocks = match raw_block_map(vol, g) {
            Ok(map) => map,
            Err(e) => {
                report(
                    &mut scan.problems,
                    message,
                    format!("group {g} block bitmap: {e}"),
                );
                continue;
            }
        };
        let actual_inodes = match raw_inode_map(vol, g) {
            Ok(map) => map,
            Err(e) => {
                report(
                    &mut scan.problems,
                    message,
                    format!("group {g} inode bitmap: {e}"),
                );
                continue;
            }
        };
        let block_bytes = (vol.sb.clusters_per_group() / 8) as usize;
        let inode_bytes = (vol.sb.inodes_per_group() / 8) as usize;
        if block_bytes > actual_blocks.len() || inode_bytes > actual_inodes.len() {
            report(
                &mut scan.problems,
                message,
                format!("group {g} bitmap coverage exceeds block size"),
            );
            continue;
        }
        // A rewritten bitmap gets a fresh checksum, so the bitmap rebuild
        // also repairs a checksum mismatch.
        if vol.sb.has_metadata_csum() && !gd.has_flag(BG_BLOCK_UNINIT) {
            let csum = bitmap_checksum(&vol.sb, &actual_blocks[..block_bytes]);
            if !bitmap_csum_matches(gd.block_bitmap_csum(), csum, vol.sb.desc_size() as usize) {
                report_fix(
                    scan,
                    message,
                    format!("group {g} block bitmap checksum mismatch"),
                    blocks_ok.then_some(Fix::BlockBitmap(g)),
                );
            }
        }
        if vol.sb.has_metadata_csum() && !gd.has_flag(BG_INODE_UNINIT) {
            let csum = bitmap_checksum(&vol.sb, &actual_inodes[..inode_bytes]);
            if !bitmap_csum_matches(gd.inode_bitmap_csum(), csum, vol.sb.desc_size() as usize) {
                report_fix(
                    scan,
                    message,
                    format!("group {g} inode bitmap checksum mismatch"),
                    Some(Fix::InodeBitmap(g)),
                );
            }
        }
        if !gd.has_flag(BG_BLOCK_UNINIT)
            && !same_bits(
                &actual_blocks,
                &scan.block_maps[g as usize],
                vol.blocks_in_group(g) as usize,
            )
        {
            report_fix(
                scan,
                message,
                format!("group {g} block bitmap differs from allocated blocks"),
                blocks_ok.then_some(Fix::BlockBitmap(g)),
            );
        }
        if !gd.has_flag(BG_INODE_UNINIT)
            && !same_bits(
                &actual_inodes,
                &scan.inode_maps[g as usize],
                vol.sb.inodes_per_group() as usize,
            )
        {
            // Unreadable inodes stay marked in the scanned map, so this
            // rebuild never frees one.
            report_fix(
                scan,
                message,
                format!("group {g} inode bitmap differs from allocated inodes"),
                Some(Fix::InodeBitmap(g)),
            );
        }
        let expected_free_blocks = expected_free_blocks(vol, scan, g);
        let expected_free_inodes = expected_free_inodes(vol, scan, g);
        let expected_dirs = expected_dirs(vol, scan, g);
        let gd = &vol.groups[g as usize];
        if gd.free_blocks_count() != expected_free_blocks {
            report_fix(
                scan,
                message,
                format!("group {g} free block count is incorrect"),
                blocks_ok.then_some(Fix::GroupBlocks(g)),
            );
        }
        if gd.free_inodes_count() != expected_free_inodes || gd.used_dirs_count() != expected_dirs {
            report_fix(
                scan,
                message,
                format!("group {g} free inode or directory count is incorrect"),
                inodes_ok.then_some(Fix::GroupInodes(g)),
            );
        }
        free_blocks += expected_free_blocks as u64;
        free_inodes += expected_free_inodes as u64;
    }
    if vol.sb.free_blocks_count() != free_blocks {
        report_fix(
            scan,
            message,
            "superblock free block count is incorrect".into(),
            blocks_ok.then_some(Fix::SuperBlocks),
        );
    }
    if vol.sb.free_inodes_count() != free_inodes as u32 {
        report_fix(
            scan,
            message,
            "superblock free inode count is incorrect".into(),
            Some(Fix::SuperInodes),
        );
    }
}

fn expected_free_blocks(vol: &Volume, scan: &Scan, g: u32) -> u32 {
    free_bits(
        &scan.block_maps[g as usize],
        vol.blocks_in_group(g) as usize,
    ) as u32
}

fn expected_free_inodes(vol: &Volume, scan: &Scan, g: u32) -> u32 {
    free_bits(
        &scan.inode_maps[g as usize],
        vol.sb.inodes_per_group() as usize,
    ) as u32
}

fn expected_dirs(vol: &Volume, scan: &Scan, g: u32) -> u32 {
    scan.dirs
        .iter()
        .filter(|&&ino| (ino - 1) / vol.sb.inodes_per_group() == g)
        .count() as u32
}

fn apply_repairs(
    vol: &mut Volume,
    scan: &mut Scan,
    repaired: &mut Vec<String>,
    message: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut fixed = HashSet::new();
    for (fix, _) in scan.repairable.clone() {
        if !fixed.insert(fix) {
            continue;
        }
        match fix {
            Fix::BlockBitmap(g) => {
                write_block_bitmap(vol, g, &scan.block_maps[g as usize])?;
                repaired.push(format!("rebuilt group {g} block bitmap"));
            }
            Fix::InodeBitmap(g) => {
                write_inode_bitmap(vol, g, &scan.inode_maps[g as usize])?;
                repaired.push(format!("rebuilt group {g} inode bitmap"));
            }
            Fix::GroupBlocks(g) => {
                let blocks = expected_free_blocks(vol, scan, g);
                vol.group_mut(g).set_free_blocks_count(blocks);
                repaired.push(format!("recounted group {g} free blocks"));
            }
            Fix::GroupInodes(g) => {
                let inodes = expected_free_inodes(vol, scan, g);
                let dirs = expected_dirs(vol, scan, g);
                let gd = vol.group_mut(g);
                gd.set_free_inodes_count(inodes);
                gd.set_used_dirs_count(dirs);
                repaired.push(format!("recounted group {g} inodes"));
            }
            Fix::SuperBlocks => {
                let blocks: u64 = (0..vol.group_count())
                    .map(|g| expected_free_blocks(vol, scan, g) as u64)
                    .sum();
                vol.sb.set_free_blocks_count(blocks);
                vol.mark_sb_dirty();
                repaired.push("recounted superblock free blocks".into());
            }
            Fix::SuperInodes => {
                let inodes: u64 = (0..vol.group_count())
                    .map(|g| expected_free_inodes(vol, scan, g) as u64)
                    .sum();
                vol.sb.set_free_inodes_count(inodes as u32);
                vol.mark_sb_dirty();
                repaired.push("recounted superblock free inodes".into());
            }
            Fix::Links(ino, links) => {
                let mut inode = vol.read_inode(ino)?;
                inode.set_links_count(links);
                vol.write_inode(ino, &inode)?;
                repaired.push(format!("fixed inode {ino} link count"));
            }
        }
    }
    for item in repaired.iter() {
        message(item);
    }
    // Drop exactly the problems whose repair was applied; everything else
    // stays reported.
    for (_, problem) in &scan.repairable {
        if let Some(pos) = scan.problems.iter().position(|p| p == problem) {
            scan.problems.remove(pos);
        }
    }
    Ok(())
}

fn write_block_bitmap(vol: &mut Volume, g: u32, bitmap: &[u8]) -> Result<()> {
    let loc = vol.groups[g as usize].block_bitmap();
    let csum = vol.sb.has_metadata_csum().then(|| {
        bitmap_checksum(
            &vol.sb,
            &bitmap[..(vol.sb.clusters_per_group() / 8) as usize],
        )
    });
    let gd = vol.group_mut(g);
    gd.set_flags(gd.flags() & !BG_BLOCK_UNINIT);
    if let Some(csum) = csum {
        gd.set_block_bitmap_csum(csum);
    }
    vol.block_mut(loc)?.copy_from_slice(bitmap);
    Ok(())
}

fn write_inode_bitmap(vol: &mut Volume, g: u32, bitmap: &[u8]) -> Result<()> {
    let loc = vol.groups[g as usize].inode_bitmap();
    let csum = vol
        .sb
        .has_metadata_csum()
        .then(|| bitmap_checksum(&vol.sb, &bitmap[..(vol.sb.inodes_per_group() / 8) as usize]));
    let gd = vol.group_mut(g);
    gd.set_flags(gd.flags() & !BG_INODE_UNINIT);
    if let Some(csum) = csum {
        gd.set_inode_bitmap_csum(csum);
    }
    vol.block_mut(loc)?.copy_from_slice(bitmap);
    Ok(())
}

fn raw_block_map(vol: &Volume, g: u32) -> Result<Vec<u8>> {
    Ok((*vol.read_block(vol.groups[g as usize].block_bitmap())?).clone())
}

fn raw_inode_map(vol: &Volume, g: u32) -> Result<Vec<u8>> {
    Ok((*vol.read_block(vol.groups[g as usize].inode_bitmap())?).clone())
}

fn claim_block(
    vol: &Volume,
    scan: &mut Scan,
    block: u64,
    owner: &str,
    message: &mut dyn FnMut(&str),
) {
    if block < vol.sb.first_data_block() as u64 || block >= vol.sb.blocks_count() {
        report(
            &mut scan.problems,
            message,
            format!("{owner} references block {block} outside the filesystem"),
        );
        return;
    }
    let group = vol.group_of_block(block) as usize;
    let bit = (block - vol.group_first_block(group as u32)) as usize;
    if test_bit(&scan.block_maps[group], bit) {
        if owner == "resize inode" {
            return;
        }
        report(
            &mut scan.problems,
            message,
            format!("block {block} is claimed more than once ({owner})"),
        );
        return;
    }
    set_bit(&mut scan.block_maps[group], bit);
}

fn mark_inode(vol: &Volume, scan: &mut Scan, ino: u32) {
    if ino == 0 || ino > vol.sb.inodes_count() {
        return;
    }
    let group = (ino - 1) / vol.sb.inodes_per_group();
    let bit = ((ino - 1) % vol.sb.inodes_per_group()) as usize;
    set_bit(&mut scan.inode_maps[group as usize], bit);
}

fn inode_marked(vol: &Volume, maps: &[Vec<u8>], ino: u32) -> bool {
    if ino == 0 || ino > vol.sb.inodes_count() {
        return false;
    }
    let group = (ino - 1) / vol.sb.inodes_per_group();
    test_bit(
        &maps[group as usize],
        ((ino - 1) % vol.sb.inodes_per_group()) as usize,
    )
}

fn same_bits(left: &[u8], right: &[u8], bits: usize) -> bool {
    (0..bits).all(|bit| test_bit(left, bit) == test_bit(right, bit))
}

fn free_bits(bitmap: &[u8], bits: usize) -> usize {
    (0..bits).filter(|&bit| !test_bit(bitmap, bit)).count()
}

fn report(problems: &mut Vec<String>, message: &mut dyn FnMut(&str), problem: impl Into<String>) {
    let problem = problem.into();
    message(&problem);
    problems.push(problem);
}
