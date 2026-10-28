//! File system creation (mkfs).
//!
//! This module writes the small, conservative ext2/ext3/ext4 layouts used by
//! the command line formatter.  It deliberately keeps all layout decisions in
//! one place so the on-disk accounting is derived from the same allocation map
//! that becomes the group bitmaps.

use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bytes::{put16, put32, put_be32, set_bit, test_bit};
use crate::crc::crc32c;
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::group::{bitmap_checksum, GroupDesc, BG_BLOCK_UNINIT, BG_INODE_UNINIT};
use crate::inode::{Inode, Timestamp, FL_EXTENTS, JOURNAL_INO, ROOT_INO, S_IFDIR, S_IFREG};
use crate::superblock::{
    compat, incompat, ro_compat, Superblock, EXT4_MAGIC, FLAG_SIGNED_HASH, STATE_VALID,
};

const FIRST_INO: u32 = 11;
const DEFAULT_RESERVED_PERCENT: u32 = 5;
const DIR_TAIL_SIZE: usize = 12;
const EXTENT_TAIL_SIZE: usize = 4;

/// Filesystem feature preset selected by the formatter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Revision {
    Ext2,
    Ext3,
    Ext4,
}

/// Input to [`format`].
#[derive(Clone, Debug)]
pub struct MkfsOptions {
    pub label: String,
    pub revision: Revision,
    pub block_size: Option<u32>,
    pub inode_size: Option<u16>,
    pub inode_ratio: Option<u32>,
    pub reserved_percent: Option<u32>,
    pub uuid: Option<[u8; 16]>,
    /// UID and GID assigned to the root directory.
    pub owner: (u32, u32),
}

impl Default for MkfsOptions {
    fn default() -> Self {
        Self {
            label: String::new(),
            revision: Revision::Ext4,
            block_size: None,
            inode_size: None,
            inode_ratio: None,
            reserved_percent: None,
            uuid: None,
            owner: (0, 0),
        }
    }
}

/// Details of the filesystem committed by [`format`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatReport {
    pub block_size: u32,
    pub blocks: u64,
    pub inodes: u32,
    pub groups: u32,
    pub journal_blocks: u32,
    pub uuid: [u8; 16],
    pub features: (u32, u32, u32),
}

#[derive(Clone, Copy)]
struct Geometry {
    block_size: u32,
    first_data_block: u32,
    blocks: u64,
    groups: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    inode_size: u16,
    inode_table_blocks: u32,
    desc_size: usize,
    gdt_blocks: u32,
}

#[derive(Clone, Copy)]
struct GroupLayout {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
}

/// Create an ext2, ext3, or ext4 filesystem directly on `dev`.
pub fn format(
    dev: &dyn BlockDevice,
    opts: &MkfsOptions,
    progress: &mut dyn FnMut(f64),
) -> Result<FormatReport> {
    progress(0.0);
    if dev.read_only() {
        return Err(Error::ReadOnly);
    }
    if opts.label.len() > 16 {
        return Err(Error::invalid("volume label is longer than 16 bytes"));
    }
    let reserved_percent = opts.reserved_percent.unwrap_or(DEFAULT_RESERVED_PERCENT);
    if reserved_percent > 100 {
        return Err(Error::invalid("reserved_percent must be between 0 and 100"));
    }
    let block_size = select_block_size(dev.size(), opts)?;
    let inode_size = opts.inode_size.unwrap_or(256);
    validate_inode_size(inode_size, block_size)?;
    let inode_ratio = opts
        .inode_ratio
        .unwrap_or(if dev.size() < 512 * 1024 * 1024 {
            4096
        } else {
            16384
        });
    if inode_ratio == 0 {
        return Err(Error::invalid("inode_ratio must not be zero"));
    }

    let (feature_compat, feature_incompat, feature_ro) = feature_preset(opts.revision);
    let uuid = match opts.uuid {
        Some(uuid) => uuid,
        None => random_array()?,
    };
    let hash_seed = [random_u32()?, random_u32()?, random_u32()?, random_u32()?];
    let now = now_timestamp();
    let mut geometry = geometry(
        dev.size(),
        block_size,
        inode_size,
        inode_ratio,
        opts.revision == Revision::Ext4,
    )?;
    let flex = opts.revision == Revision::Ext4;
    trim_final_group(&mut geometry)?;
    if geometry.blocks > u32::MAX as u64 && !flex {
        return Err(Error::invalid(
            "filesystem has more than 32-bit block addresses",
        ));
    }

    let mut sb = make_superblock(
        &geometry,
        opts,
        uuid,
        hash_seed,
        now,
        feature_compat,
        feature_incompat,
        feature_ro,
    );

    // Superblock and descriptor backups are reserved first, so the group
    // metadata placement below sees them as used and steps around them.
    let mut block_maps = initial_block_maps(&geometry)?;
    for group in 0..geometry.groups {
        if sb.group_has_super(group) {
            mark_block(&mut block_maps, &geometry, group_first(&geometry, group))?;
            for i in 0..geometry.gdt_blocks {
                mark_block(
                    &mut block_maps,
                    &geometry,
                    group_first(&geometry, group) + 1 + i as u64,
                )?;
            }
        }
    }
    let layouts = make_layout(&geometry, flex, &mut block_maps)?;

    // Clear old signatures before committing new metadata.  The tail write is
    // intentionally done first, so a later metadata write always wins.
    write_bytes(dev, 0, &[0; 1024])?;
    let tail = 128 * 1024u64;
    if dev.size() > tail {
        write_zeros(dev, dev.size() - tail, tail as usize)?;
    }
    progress(0.05);

    // The root and lost+found blocks are placed before the journal, keeping
    // both directories simple one-extent or direct-pointer files.
    let root_blocks = allocate_blocks(&mut block_maps, &geometry, 1)?;
    let lost_blocks = allocate_blocks(
        &mut block_maps,
        &geometry,
        (16 * 1024usize)
            .div_ceil(geometry.block_size as usize)
            .max(1),
    )?;
    let journal_blocks = journal_size(geometry.blocks, opts.revision)?;
    let journal_data = if journal_blocks == 0 {
        Vec::new()
    } else {
        allocate_blocks(&mut block_maps, &geometry, journal_blocks as usize)?
    };
    progress(0.20);

    let mut root = new_inode(
        &sb,
        ROOT_INO,
        S_IFDIR | 0o755,
        opts.owner,
        3,
        geometry.block_size as u64,
        now,
    );
    let mut lost = new_inode(
        &sb,
        FIRST_INO,
        S_IFDIR | 0o700,
        (0, 0),
        2,
        lost_blocks.len() as u64 * geometry.block_size as u64,
        now,
    );
    let mut journal = if journal_blocks != 0 {
        Some(new_inode(
            &sb,
            JOURNAL_INO,
            S_IFREG | 0o600,
            (0, 0),
            1,
            journal_data.len() as u64 * geometry.block_size as u64,
            now,
        ))
    } else {
        None
    };

    let mut extent_blocks = Vec::new();
    if flex {
        extent_blocks.extend(write_extent_map(
            &mut root,
            &sb,
            ROOT_INO,
            &root_blocks,
            &mut block_maps,
            &geometry,
            dev,
        )?);
        extent_blocks.extend(write_extent_map(
            &mut lost,
            &sb,
            FIRST_INO,
            &lost_blocks,
            &mut block_maps,
            &geometry,
            dev,
        )?);
        if let Some(inode) = journal.as_mut() {
            extent_blocks.extend(write_extent_map(
                inode,
                &sb,
                JOURNAL_INO,
                &journal_data,
                &mut block_maps,
                &geometry,
                dev,
            )?);
        }
    } else {
        write_classic_map(&mut root, &root_blocks, &mut block_maps, &geometry, dev)?;
        write_classic_map(&mut lost, &lost_blocks, &mut block_maps, &geometry, dev)?;
        if let Some(inode) = journal.as_mut() {
            write_classic_map(inode, &journal_data, &mut block_maps, &geometry, dev)?;
        }
    }
    let _ = extent_blocks;

    let root_map_blocks = count_file_map_blocks(&root, flex, &geometry, &root_blocks);
    root.set_blocks_512(
        &sb,
        (root_blocks.len() as u64 + root_map_blocks) * geometry.block_size as u64 / 512,
    )?;
    let lost_map_blocks = count_file_map_blocks(&lost, flex, &geometry, &lost_blocks);
    lost.set_blocks_512(
        &sb,
        (lost_blocks.len() as u64 + lost_map_blocks) * geometry.block_size as u64 / 512,
    )?;
    if let Some(inode) = journal.as_mut() {
        let map_blocks = count_file_map_blocks(inode, flex, &geometry, &journal_data);
        inode.set_blocks_512(
            &sb,
            (journal_data.len() as u64 + map_blocks) * geometry.block_size as u64 / 512,
        )?;
    }

    if flex {
        root.update_checksum(&sb, ROOT_INO);
        lost.update_checksum(&sb, FIRST_INO);
        if let Some(inode) = journal.as_mut() {
            inode.update_checksum(&sb, JOURNAL_INO);
        }
    }

    write_directory_block(
        dev,
        &sb,
        &root,
        ROOT_INO,
        root_blocks[0],
        &[
            (ROOT_INO, b"."),
            (ROOT_INO, b".."),
            (FIRST_INO, b"lost+found"),
        ],
    )?;
    for (i, &block) in lost_blocks.iter().enumerate() {
        if i == 0 {
            write_directory_block(
                dev,
                &sb,
                &lost,
                FIRST_INO,
                block,
                &[(FIRST_INO, b"."), (ROOT_INO, b"..")],
            )?;
        } else {
            write_empty_directory_block(dev, &sb, &lost, FIRST_INO, block)?;
        }
    }

    if let Some(inode) = journal.as_ref() {
        initialize_journal(
            dev,
            &sb,
            inode,
            &journal_data,
            opts.revision == Revision::Ext4,
        )?;
    }
    progress(0.45);

    let inode_maps = make_inode_maps(&geometry, flex)?;
    let mut groups = Vec::with_capacity(geometry.groups as usize);
    let mut free_blocks_total = 0u64;
    let mut free_inodes_total = 0u64;
    for group in 0..geometry.groups {
        let used_blocks = count_set_bits(
            &block_maps[group as usize],
            blocks_in_group(&geometry, group) as usize,
        );
        let group_blocks = blocks_in_group(&geometry, group);
        let free_blocks = group_blocks
            .checked_sub(used_blocks as u64)
            .ok_or_else(|| Error::corrupt("mkfs block accounting underflow"))?;
        let used_inodes = if group == 0 { FIRST_INO } else { 0 };
        let free_inodes = geometry.inodes_per_group - used_inodes;
        let mut desc = GroupDesc::new(geometry.desc_size);
        let layout = layouts[group as usize];
        desc.set_block_bitmap(layout.block_bitmap);
        desc.set_inode_bitmap(layout.inode_bitmap);
        desc.set_inode_table(layout.inode_table);
        desc.set_free_blocks_count(free_blocks as u32);
        desc.set_free_inodes_count(free_inodes);
        desc.set_used_dirs_count(if group == 0 { 2 } else { 0 });
        desc.set_itable_unused(if flex {
            if group == 0 {
                geometry.inodes_per_group - FIRST_INO
            } else {
                geometry.inodes_per_group
            }
        } else {
            0
        });
        if flex && group != 0 {
            let mut flags = BG_INODE_UNINIT;
            if group_block_bitmap_only_owns_backup(&sb, &geometry, &block_maps, group) {
                flags |= BG_BLOCK_UNINIT;
            }
            desc.set_flags(flags);
        }
        if flex {
            let block_len = geometry.blocks_per_group.div_ceil(8) as usize;
            let inode_len = (geometry.inodes_per_group / 8) as usize;
            desc.set_block_bitmap_csum(bitmap_checksum(
                &sb,
                &block_maps[group as usize][..block_len],
            ));
            desc.set_inode_bitmap_csum(bitmap_checksum(
                &sb,
                &inode_maps[group as usize][..inode_len],
            ));
        }
        free_blocks_total += free_blocks;
        free_inodes_total += free_inodes as u64;
        groups.push(desc);
    }
    sb.set_free_blocks_count(free_blocks_total);
    sb.set_free_inodes_count(free_inodes_total as u32);
    sb.set_r_blocks_count(geometry.blocks * reserved_percent as u64 / 100);
    if let Some(inode) = journal.as_ref() {
        let mut saved = [0u32; 17];
        for (i, chunk) in inode.i_block().chunks_exact(4).enumerate() {
            saved[i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        saved[15] = inode.size_hi();
        saved[16] = inode.size_lo();
        sb.set_jnl_blocks(&saved);
    }

    // Descriptor checksums include bitmap checksum fields, so this is the last
    // descriptor mutation before every primary and backup GDT is written.
    for (group, desc) in groups.iter_mut().enumerate() {
        desc.update_checksum(&sb, group as u32);
    }
    sb.update_checksum();

    write_super_and_gdts(dev, &sb, &geometry, &groups)?;
    for group in 0..geometry.groups {
        write_block(
            dev,
            layouts[group as usize].block_bitmap,
            geometry.block_size,
            &block_maps[group as usize],
        )?;
        write_block(
            dev,
            layouts[group as usize].inode_bitmap,
            geometry.block_size,
            &inode_maps[group as usize],
        )?;
    }

    // ext2 and ext3 have initialized inode tables.  ext4 leaves unused tables
    // uninitialized, but clears the table blocks containing reserved and live
    // inodes so no stale inode is ever exposed as allocated.
    if flex {
        let needed = ((FIRST_INO as usize * geometry.inode_size as usize)
            .div_ceil(geometry.block_size as usize)) as u32;
        let zero = vec![0; geometry.block_size as usize];
        for i in 0..needed {
            write_block(
                dev,
                layouts[0].inode_table + i as u64,
                geometry.block_size,
                &zero,
            )?;
        }
    } else {
        let zero = vec![0; geometry.block_size as usize];
        for layout in &layouts {
            for i in 0..geometry.inode_table_blocks {
                write_block(
                    dev,
                    layout.inode_table + i as u64,
                    geometry.block_size,
                    &zero,
                )?;
            }
        }
    }
    write_inode(dev, &geometry, &layouts, ROOT_INO, &root)?;
    write_inode(dev, &geometry, &layouts, FIRST_INO, &lost)?;
    if let Some(inode) = journal.as_ref() {
        write_inode(dev, &geometry, &layouts, JOURNAL_INO, inode)?;
    }
    progress(0.90);

    dev.flush()?;
    progress(1.0);
    Ok(FormatReport {
        block_size: geometry.block_size,
        blocks: geometry.blocks,
        inodes: geometry.groups * geometry.inodes_per_group,
        groups: geometry.groups,
        journal_blocks,
        uuid,
        features: (feature_compat, feature_incompat, feature_ro),
    })
}

fn select_block_size(size: u64, requested: &MkfsOptions) -> Result<u32> {
    let block_size =
        requested
            .block_size
            .unwrap_or(if size < 512 * 1024 * 1024 { 1024 } else { 4096 });
    if !matches!(block_size, 1024 | 2048 | 4096 | 65536) {
        return Err(Error::invalid(
            "block size must be 1024, 2048, 4096, or 65536",
        ));
    }
    if block_size == 65536 && requested.revision != Revision::Ext4 {
        return Err(Error::invalid("64 KiB blocks are supported only for ext4"));
    }
    Ok(block_size)
}

fn validate_inode_size(inode_size: u16, block_size: u32) -> Result<()> {
    if inode_size < 128 || !inode_size.is_power_of_two() || inode_size as u32 > block_size {
        return Err(Error::invalid(
            "inode size must be a power of two from 128 through the block size",
        ));
    }
    Ok(())
}

fn feature_preset(revision: Revision) -> (u32, u32, u32) {
    let base_compat = compat::EXT_ATTR | compat::DIR_INDEX;
    let base_incompat = incompat::FILETYPE;
    let base_ro = ro_compat::SPARSE_SUPER | ro_compat::LARGE_FILE;
    match revision {
        Revision::Ext2 => (base_compat, base_incompat, base_ro),
        Revision::Ext3 => (base_compat | compat::HAS_JOURNAL, base_incompat, base_ro),
        Revision::Ext4 => (
            base_compat | compat::HAS_JOURNAL,
            base_incompat | incompat::EXTENTS | incompat::BIT64 | incompat::FLEX_BG,
            base_ro
                | ro_compat::HUGE_FILE
                | ro_compat::DIR_NLINK
                | ro_compat::EXTRA_ISIZE
                | ro_compat::METADATA_CSUM,
        ),
    }
}

fn geometry(
    device_bytes: u64,
    block_size: u32,
    inode_size: u16,
    inode_ratio: u32,
    ext4: bool,
) -> Result<Geometry> {
    let blocks = device_bytes / block_size as u64;
    let first_data_block = if block_size == 1024 { 1 } else { 0 };
    if blocks <= first_data_block as u64 {
        return Err(Error::invalid(
            "device is too small to contain an ext filesystem",
        ));
    }
    if !ext4 && blocks > u32::MAX as u64 {
        return Err(Error::invalid(
            "ext2/ext3 filesystem exceeds 32-bit block addressing",
        ));
    }
    // e2fsprogs limits 64 KiB filesystem block groups to 65,528 blocks.
    // Larger values make its on-disk geometry validator reject the volume.
    let blocks_per_group = if block_size == 65536 {
        65528
    } else {
        8 * block_size
    };
    let groups_u64 = (blocks - first_data_block as u64).div_ceil(blocks_per_group as u64);
    if groups_u64 == 0 || groups_u64 > u32::MAX as u64 {
        return Err(Error::invalid(
            "device requires more block groups than ext can represent",
        ));
    }
    let groups = groups_u64 as u32;
    let target =
        ((blocks_per_group as u64 * block_size as u64) / inode_ratio as u64).max(FIRST_INO as u64);
    let inodes_per_block = block_size / inode_size as u32;
    let mut inodes_per_group = target.div_ceil(inodes_per_block as u64) as u32 * inodes_per_block;
    inodes_per_group = inodes_per_group.div_ceil(8) * 8;
    let inode_bitmap_capacity = (blocks_per_group / inodes_per_block) * inodes_per_block;
    inodes_per_group = inodes_per_group.min((8 * block_size).min(inode_bitmap_capacity));
    if inodes_per_group < FIRST_INO || inodes_per_group == 0 {
        return Err(Error::invalid(
            "inode geometry cannot represent reserved inodes",
        ));
    }
    if groups as u64 * inodes_per_group as u64 > u32::MAX as u64 {
        return Err(Error::invalid(
            "device requires more inodes than ext can represent",
        ));
    }

    let inode_table_blocks = inodes_per_group / inodes_per_block;
    let desc_size = if ext4 { 64 } else { 32 };
    let gdt_blocks = groups.div_ceil(block_size / desc_size as u32);
    Ok(Geometry {
        block_size,
        first_data_block,
        blocks,
        groups,
        blocks_per_group,
        inodes_per_group,
        inode_size,
        inode_table_blocks,
        desc_size,
        gdt_blocks,
    })
}

/// Drop a final partial group that is too small to be worth keeping.
///
/// This is the mke2fs rule: a partial group must hold at least the
/// metadata a group can need plus 50 data blocks.  Packed flex_bg metadata
/// is placed separately and never causes whole groups to be dropped.
fn trim_final_group(geometry: &mut Geometry) -> Result<()> {
    let overhead = 3 + geometry.inode_table_blocks as u64 + geometry.gdt_blocks as u64;
    if overhead > geometry.blocks_per_group as u64 {
        return Err(Error::invalid(
            "group metadata does not fit in one block group",
        ));
    }
    let rem =
        (geometry.blocks - geometry.first_data_block as u64) % geometry.blocks_per_group as u64;
    if rem == 0 || rem >= overhead + 50 {
        return Ok(());
    }
    if geometry.groups == 1 {
        return Err(Error::invalid(
            "device is too small to hold ext filesystem metadata",
        ));
    }
    geometry.groups -= 1;
    geometry.blocks -= rem;
    geometry.gdt_blocks = geometry
        .groups
        .div_ceil((geometry.block_size as usize / geometry.desc_size) as u32);
    Ok(())
}

/// Choose and reserve the block bitmap, inode bitmap, and inode table of
/// every group.  `maps` must already contain the superblock and descriptor
/// backups.
///
/// Without flex_bg each group holds its own metadata right after its
/// backups.  With flex_bg the metadata of a flex group is packed the way
/// mke2fs packs it: all block bitmaps, then all inode bitmaps, then all
/// inode tables.  The runs start in the first group of the flex group and
/// spill into the following groups, stepping around backup blocks.
fn make_layout(geometry: &Geometry, flex: bool, maps: &mut [Vec<u8>]) -> Result<Vec<GroupLayout>> {
    let mut layouts = vec![
        GroupLayout {
            block_bitmap: 0,
            inode_bitmap: 0,
            inode_table: 0
        };
        geometry.groups as usize
    ];
    let itable = geometry.inode_table_blocks as u64;
    if !flex {
        for group in 0..geometry.groups {
            let first = group_first(geometry, group);
            let block_bitmap = first + group_overhead(geometry, group);
            if block_bitmap + 2 + itable > first + blocks_in_group(geometry, group) {
                return Err(Error::invalid(
                    "device is too small to hold ext filesystem metadata",
                ));
            }
            for block in block_bitmap..block_bitmap + 2 + itable {
                mark_block(maps, geometry, block)?;
            }
            layouts[group as usize] = GroupLayout {
                block_bitmap,
                inode_bitmap: block_bitmap + 1,
                inode_table: block_bitmap + 2,
            };
        }
        return Ok(layouts);
    }

    let flex_size = 16u32;
    for flex_start in (0..geometry.groups).step_by(flex_size as usize) {
        let flex_end = (flex_start + flex_size).min(geometry.groups);
        let span = (
            group_first(geometry, flex_start),
            group_first(geometry, flex_end - 1) + blocks_in_group(geometry, flex_end - 1),
        );
        // Each kind of metadata gets a run as long as the flex group has
        // members, so a full flex group keeps the same layout as mke2fs.
        let table_offset = (flex_end - flex_start) as u64;
        let mut cursor = span.0;
        for group in flex_start..flex_end {
            let block = place_metadata(maps, geometry, cursor, span, 1)?;
            layouts[group as usize].block_bitmap = block;
            cursor = block + 1;
        }
        cursor = cursor.max(layouts[flex_start as usize].block_bitmap + table_offset);
        for group in flex_start..flex_end {
            let block = place_metadata(maps, geometry, cursor, span, 1)?;
            layouts[group as usize].inode_bitmap = block;
            cursor = block + 1;
        }
        cursor = cursor.max(layouts[flex_start as usize].inode_bitmap + table_offset);
        for group in flex_start..flex_end {
            let block = place_metadata(maps, geometry, cursor, span, itable)?;
            layouts[group as usize].inode_table = block;
            cursor = block + itable;
        }
    }
    Ok(layouts)
}

/// Reserve `len` free contiguous blocks for group metadata and return the
/// first one.  The search starts at `cursor`, then retries from the start
/// of the flex group `span`, then from the start of the filesystem.
fn place_metadata(
    maps: &mut [Vec<u8>],
    geometry: &Geometry,
    cursor: u64,
    span: (u64, u64),
    len: u64,
) -> Result<u64> {
    let found = find_free_run(maps, geometry, cursor, span.1, len)
        .or_else(|| find_free_run(maps, geometry, span.0, span.1, len))
        .or_else(|| {
            find_free_run(
                maps,
                geometry,
                geometry.first_data_block as u64,
                geometry.blocks,
                len,
            )
        })
        .ok_or_else(|| Error::invalid("device is too small to hold ext filesystem metadata"))?;
    for block in found..found + len {
        mark_block(maps, geometry, block)?;
    }
    Ok(found)
}

/// First block of `len` free contiguous blocks inside `start..end`.
fn find_free_run(
    maps: &[Vec<u8>],
    geometry: &Geometry,
    start: u64,
    end: u64,
    len: u64,
) -> Option<u64> {
    let mut run_start = start;
    let mut block = start;
    while block < end {
        if block_in_use(maps, geometry, block) {
            run_start = block + 1;
        } else if block + 1 - run_start == len {
            return Some(run_start);
        }
        block += 1;
    }
    None
}

fn block_in_use(maps: &[Vec<u8>], geometry: &Geometry, block: u64) -> bool {
    let relative = block - geometry.first_data_block as u64;
    let group = (relative / geometry.blocks_per_group as u64) as usize;
    let bit = (relative % geometry.blocks_per_group as u64) as usize;
    test_bit(&maps[group], bit)
}

#[allow(clippy::too_many_arguments)]
fn make_superblock(
    geometry: &Geometry,
    opts: &MkfsOptions,
    uuid: [u8; 16],
    hash_seed: [u32; 4],
    now: Timestamp,
    feature_compat: u32,
    feature_incompat: u32,
    feature_ro: u32,
) -> Superblock {
    let mut sb = Superblock::zeroed();
    sb.set_inodes_count(geometry.groups * geometry.inodes_per_group);
    sb.set_blocks_count(geometry.blocks);
    sb.set_first_data_block(geometry.first_data_block);
    sb.set_log_block_size(geometry.block_size.trailing_zeros() - 10);
    sb.set_log_cluster_size(geometry.block_size.trailing_zeros() - 10);
    sb.set_blocks_per_group(geometry.blocks_per_group);
    sb.set_clusters_per_group(geometry.blocks_per_group);
    sb.set_inodes_per_group(geometry.inodes_per_group);
    sb.set_mtime(0);
    sb.set_wtime(now.secs as u32);
    sb.set_max_mnt_count(u16::MAX);
    sb.set_magic(EXT4_MAGIC);
    sb.set_state(STATE_VALID);
    sb.set_errors(1);
    sb.set_lastcheck(now.secs as u32);
    sb.set_creator_os(0);
    sb.set_rev_level(1);
    sb.set_first_ino(FIRST_INO);
    sb.set_inode_size(geometry.inode_size);
    sb.set_feature_compat(feature_compat);
    sb.set_feature_incompat(feature_incompat);
    sb.set_feature_ro_compat(feature_ro);
    sb.set_uuid(uuid);
    sb.set_volume_name(opts.label.as_bytes());
    sb.set_hash_seed(hash_seed);
    sb.set_def_hash_version(1);
    sb.set_flags(FLAG_SIGNED_HASH);
    sb.set_default_mount_opts(0x0004 | 0x0008);
    sb.set_mkfs_time(now.secs as u32);
    sb.set_checksum_type(1);
    sb.set_kbytes_written(0);
    if opts.revision == Revision::Ext4 {
        sb.set_desc_size(64);
        sb.set_log_groups_per_flex(4);
        sb.set_min_extra_isize(32);
        sb.set_want_extra_isize(32);
        sb.set_journal_inum(JOURNAL_INO);
        sb.set_jnl_backup_type(1);
    } else if opts.revision == Revision::Ext3 {
        sb.set_journal_inum(JOURNAL_INO);
        sb.set_jnl_backup_type(1);
    }
    sb
}

fn group_first(geometry: &Geometry, group: u32) -> u64 {
    geometry.first_data_block as u64 + group as u64 * geometry.blocks_per_group as u64
}

fn blocks_in_group(geometry: &Geometry, group: u32) -> u64 {
    let start = group_first(geometry, group);
    (geometry.blocks - start).min(geometry.blocks_per_group as u64)
}

fn group_overhead(geometry: &Geometry, group: u32) -> u64 {
    let has_super = group == 0
        || group == 1
        || (group % 2 == 1 && (power_of(group, 3) || power_of(group, 5) || power_of(group, 7)));
    if has_super {
        1 + geometry.gdt_blocks as u64
    } else {
        0
    }
}

fn power_of(mut n: u32, base: u32) -> bool {
    if n == 0 {
        return false;
    }
    while n % base == 0 {
        n /= base;
    }
    n == 1
}

fn initial_block_maps(geometry: &Geometry) -> Result<Vec<Vec<u8>>> {
    let bytes = geometry.block_size as usize;
    let mut maps = Vec::with_capacity(geometry.groups as usize);
    for group in 0..geometry.groups {
        let mut map = vec![0; bytes];
        let valid = blocks_in_group(geometry, group) as usize;
        for bit in valid..bytes * 8 {
            set_bit(&mut map, bit);
        }
        maps.push(map);
    }
    Ok(maps)
}

fn make_inode_maps(geometry: &Geometry, _flex: bool) -> Result<Vec<Vec<u8>>> {
    let mut maps = Vec::with_capacity(geometry.groups as usize);
    for group in 0..geometry.groups {
        let mut map = vec![0; geometry.block_size as usize];
        for bit in geometry.inodes_per_group as usize..map.len() * 8 {
            set_bit(&mut map, bit);
        }
        if group == 0 {
            for bit in 0..FIRST_INO as usize {
                set_bit(&mut map, bit);
            }
        }
        maps.push(map);
    }
    Ok(maps)
}

fn mark_block(maps: &mut [Vec<u8>], geometry: &Geometry, block: u64) -> Result<()> {
    if block < geometry.first_data_block as u64 || block >= geometry.blocks {
        return Err(Error::corrupt(
            "mkfs tried to allocate a block outside the filesystem",
        ));
    }
    let relative = block - geometry.first_data_block as u64;
    let group = (relative / geometry.blocks_per_group as u64) as usize;
    let bit = (relative % geometry.blocks_per_group as u64) as usize;
    if test_bit(&maps[group], bit) {
        return Err(Error::NoSpace);
    }
    set_bit(&mut maps[group], bit);
    Ok(())
}

fn allocate_blocks(maps: &mut [Vec<u8>], geometry: &Geometry, count: usize) -> Result<Vec<u64>> {
    let mut blocks = Vec::with_capacity(count);
    for (group, map) in maps.iter_mut().enumerate().take(geometry.groups as usize) {
        let valid = blocks_in_group(geometry, group as u32) as usize;
        for bit in 0..valid {
            if !test_bit(map, bit) {
                set_bit(map, bit);
                blocks.push(group_first(geometry, group as u32) + bit as u64);
                if blocks.len() == count {
                    return Ok(blocks);
                }
            }
        }
    }
    Err(Error::NoSpace)
}

fn new_inode(
    sb: &Superblock,
    ino: u32,
    mode: u16,
    owner: (u32, u32),
    links: u16,
    size: u64,
    now: Timestamp,
) -> Inode {
    let mut inode = Inode::new(sb.inode_size() as usize);
    inode.set_mode(mode);
    inode.set_uid(owner.0);
    inode.set_gid(owner.1);
    inode.set_links_count(links);
    inode.set_size(size);
    inode.set_atime(now);
    inode.set_ctime(now);
    inode.set_mtime(now);
    inode.set_extra_isize(32);
    inode.set_crtime(now);
    if sb.has_incompat(incompat::EXTENTS) {
        inode.set_flags(FL_EXTENTS);
    }
    // The inode number is intentionally part of this helper's signature: it
    // documents that every constructed inode later receives an inode checksum.
    let _ = ino;
    inode
}

fn runs(blocks: &[u64]) -> Vec<(u32, u64, u32)> {
    let mut result = Vec::new();
    let mut logical = 0u32;
    let mut start = 0usize;
    while start < blocks.len() {
        let physical = blocks[start];
        let mut end = start + 1;
        while end < blocks.len() && blocks[end] == blocks[end - 1] + 1 {
            end += 1;
        }
        let mut remaining = end - start;
        let mut p = physical;
        while remaining > 0 {
            let len = remaining.min(32768) as u32;
            result.push((logical, p, len));
            logical += len;
            p += len as u64;
            remaining -= len as usize;
        }
        start = end;
    }
    result
}

fn write_extent_map(
    inode: &mut Inode,
    sb: &Superblock,
    ino: u32,
    blocks: &[u64],
    maps: &mut [Vec<u8>],
    geometry: &Geometry,
    dev: &dyn BlockDevice,
) -> Result<Vec<u64>> {
    let extents = runs(blocks);
    inode.i_block_mut().fill(0);
    if extents.len() <= 4 {
        write_extent_header(inode.i_block_mut(), extents.len() as u16, 4, 0);
        for (i, &(logical, physical, len)) in extents.iter().enumerate() {
            write_extent(
                &mut inode.i_block_mut()[12 + i * 12..],
                logical,
                physical,
                len,
            );
        }
        return Ok(Vec::new());
    }
    let capacity = (geometry.block_size as usize
        - 12
        - if sb.has_metadata_csum() {
            EXTENT_TAIL_SIZE
        } else {
            0
        })
        / 12;
    if extents.len() > capacity {
        return Err(Error::FileTooLarge);
    }
    let leaf = allocate_blocks(maps, geometry, 1)?[0];
    let mut node = vec![0; geometry.block_size as usize];
    write_extent_header(&mut node, extents.len() as u16, capacity as u16, 0);
    for (i, &(logical, physical, len)) in extents.iter().enumerate() {
        write_extent(&mut node[12 + i * 12..], logical, physical, len);
    }
    if sb.has_metadata_csum() {
        let tail = node.len() - EXTENT_TAIL_SIZE;
        let csum = crc32c(Inode::csum_seed(sb, ino, inode.generation()), &node[..tail]);
        put32(&mut node, tail, csum);
    }
    write_block(dev, leaf, geometry.block_size, &node)?;
    write_extent_header(inode.i_block_mut(), 1, 4, 1);
    put32(inode.i_block_mut(), 12, 0);
    put32(inode.i_block_mut(), 16, leaf as u32);
    put16(inode.i_block_mut(), 20, (leaf >> 32) as u16);
    Ok(vec![leaf])
}

fn write_extent_header(buf: &mut [u8], entries: u16, max: u16, depth: u16) {
    put16(buf, 0, 0xF30A);
    put16(buf, 2, entries);
    put16(buf, 4, max);
    put16(buf, 6, depth);
    put32(buf, 8, 0);
}

fn write_extent(buf: &mut [u8], logical: u32, physical: u64, len: u32) {
    put32(buf, 0, logical);
    put16(buf, 4, len as u16);
    put16(buf, 6, (physical >> 32) as u16);
    put32(buf, 8, physical as u32);
}

fn write_classic_map(
    inode: &mut Inode,
    blocks: &[u64],
    maps: &mut [Vec<u8>],
    geometry: &Geometry,
    dev: &dyn BlockDevice,
) -> Result<()> {
    inode.i_block_mut().fill(0);
    let pointers = geometry.block_size as usize / 4;
    let mut at = 0usize;
    for slot in 0..12 {
        if at == blocks.len() {
            return Ok(());
        }
        put32(inode.i_block_mut(), slot * 4, blocks[at] as u32);
        at += 1;
    }
    if at == blocks.len() {
        return Ok(());
    }
    let single = allocate_blocks(maps, geometry, 1)?[0];
    put32(inode.i_block_mut(), 12 * 4, single as u32);
    let mut page = vec![0; geometry.block_size as usize];
    for i in 0..pointers.min(blocks.len() - at) {
        put32(&mut page, i * 4, blocks[at] as u32);
        at += 1;
    }
    write_block(dev, single, geometry.block_size, &page)?;
    if at == blocks.len() {
        return Ok(());
    }
    let double = allocate_blocks(maps, geometry, 1)?[0];
    put32(inode.i_block_mut(), 13 * 4, double as u32);
    let mut root = vec![0; geometry.block_size as usize];
    for i in 0..pointers {
        if at == blocks.len() {
            break;
        }
        let leaf = allocate_blocks(maps, geometry, 1)?[0];
        put32(&mut root, i * 4, leaf as u32);
        let mut child = vec![0; geometry.block_size as usize];
        for j in 0..pointers.min(blocks.len() - at) {
            put32(&mut child, j * 4, blocks[at] as u32);
            at += 1;
        }
        write_block(dev, leaf, geometry.block_size, &child)?;
    }
    write_block(dev, double, geometry.block_size, &root)?;
    if at == blocks.len() {
        return Ok(());
    }
    let triple = allocate_blocks(maps, geometry, 1)?[0];
    put32(inode.i_block_mut(), 14 * 4, triple as u32);
    let mut top = vec![0; geometry.block_size as usize];
    for i in 0..pointers {
        if at == blocks.len() {
            break;
        }
        let middle = allocate_blocks(maps, geometry, 1)?[0];
        put32(&mut top, i * 4, middle as u32);
        let mut mid_page = vec![0; geometry.block_size as usize];
        for j in 0..pointers {
            if at == blocks.len() {
                break;
            }
            let leaf = allocate_blocks(maps, geometry, 1)?[0];
            put32(&mut mid_page, j * 4, leaf as u32);
            let mut child = vec![0; geometry.block_size as usize];
            for k in 0..pointers.min(blocks.len() - at) {
                put32(&mut child, k * 4, blocks[at] as u32);
                at += 1;
            }
            write_block(dev, leaf, geometry.block_size, &child)?;
        }
        write_block(dev, middle, geometry.block_size, &mid_page)?;
    }
    write_block(dev, triple, geometry.block_size, &top)?;
    if at == blocks.len() {
        Ok(())
    } else {
        Err(Error::FileTooLarge)
    }
}

fn count_file_map_blocks(inode: &Inode, flex: bool, geometry: &Geometry, data: &[u64]) -> u64 {
    if flex {
        return u64::from(runs(data).len() > 4);
    }
    let p = geometry.block_size as usize / 4;
    let n = data.len();
    if n <= 12 {
        return 0;
    }
    let mut maps = 1u64;
    if n <= 12 + p {
        return maps;
    }
    let after_single = n - 12 - p;
    maps += 1 + after_single.div_ceil(p) as u64;
    if after_single <= p * p {
        return maps;
    }
    let after_double = after_single - p * p;
    maps += 1 + after_double.div_ceil(p * p) as u64 + after_double.div_ceil(p) as u64;
    let _ = inode;
    maps
}

fn write_directory_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    inode: &Inode,
    ino: u32,
    block: u64,
    entries: &[(u32, &[u8])],
) -> Result<()> {
    let size = sb.block_size() as usize;
    let tail = if sb.has_metadata_csum() {
        DIR_TAIL_SIZE
    } else {
        0
    };
    let usable = size - tail;
    let mut data = vec![0; size];
    let mut offset = 0usize;
    for (index, &(entry_ino, name)) in entries.iter().enumerate() {
        let min = (8 + name.len() + 3) & !3;
        let length = if index + 1 == entries.len() {
            usable - offset
        } else {
            min
        };
        if length < min || length > usable - offset {
            return Err(Error::corrupt("mkfs directory entry does not fit"));
        }
        put32(&mut data, offset, entry_ino);
        put16(&mut data, offset + 4, encode_rec_len(length));
        data[offset + 6] = name.len() as u8;
        data[offset + 7] = 2;
        data[offset + 8..offset + 8 + name.len()].copy_from_slice(name);
        offset += length;
    }
    if sb.has_metadata_csum() {
        directory_tail(&mut data, inode, sb, ino)?;
    }
    write_block(dev, block, sb.block_size(), &data)
}

fn write_empty_directory_block(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    inode: &Inode,
    ino: u32,
    block: u64,
) -> Result<()> {
    let size = sb.block_size() as usize;
    let tail = if sb.has_metadata_csum() {
        DIR_TAIL_SIZE
    } else {
        0
    };
    let mut data = vec![0; size];
    put16(&mut data, 4, encode_rec_len(size - tail));
    if sb.has_metadata_csum() {
        directory_tail(&mut data, inode, sb, ino)?;
    }
    write_block(dev, block, sb.block_size(), &data)
}

fn encode_rec_len(length: usize) -> u16 {
    if length == 65536 {
        0
    } else {
        length as u16
    }
}

fn directory_tail(data: &mut [u8], inode: &Inode, sb: &Superblock, ino: u32) -> Result<()> {
    if data.len() < DIR_TAIL_SIZE {
        return Err(Error::corrupt(
            "directory block is smaller than checksum tail",
        ));
    }
    let tail = data.len() - DIR_TAIL_SIZE;
    put32(data, tail, 0);
    put16(data, tail + 4, DIR_TAIL_SIZE as u16);
    data[tail + 6] = 0;
    data[tail + 7] = 0xDE;
    let csum = crc32c(Inode::csum_seed(sb, ino, inode.generation()), &data[..tail]);
    put32(data, tail + 8, csum);
    Ok(())
}

fn initialize_journal(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    inode: &Inode,
    data_blocks: &[u64],
    ext4: bool,
) -> Result<()> {
    if data_blocks.is_empty() {
        return Ok(());
    }
    let bs = sb.block_size() as usize;
    let zero_count = if ext4 {
        data_blocks.len().min(2)
    } else {
        data_blocks.len()
    };
    let zero = vec![0; bs];
    for &block in &data_blocks[..zero_count] {
        write_block(dev, block, sb.block_size(), &zero)?;
    }
    let mut journal = vec![0; bs];
    put_be32(&mut journal, 0, 0xC03B3998);
    put_be32(&mut journal, 4, 4);
    put_be32(&mut journal, 8, 0);
    put_be32(&mut journal, 12, sb.block_size());
    put_be32(&mut journal, 16, data_blocks.len() as u32);
    put_be32(&mut journal, 20, 1);
    put_be32(&mut journal, 24, 1);
    put_be32(&mut journal, 28, 0);
    put_be32(&mut journal, 32, 0);
    if ext4 {
        put_be32(&mut journal, 40, 0x10 | 0x2);
        journal[80] = 4;
    }
    journal[48..64].copy_from_slice(&sb.uuid());
    put_be32(&mut journal, 64, 1);
    journal[0x100..0x110].copy_from_slice(&sb.uuid());
    if ext4 {
        let csum = crc32c(!0, &journal[..1024]);
        put_be32(&mut journal, 0xFC, csum);
    }
    let _ = inode;
    write_block(dev, data_blocks[0], sb.block_size(), &journal)
}

fn journal_size(blocks: u64, revision: Revision) -> Result<u32> {
    if revision == Revision::Ext2 {
        return Ok(0);
    }
    if blocks < 2048 {
        return Err(Error::invalid(
            "ext3/ext4 requires at least 2048 blocks for its journal",
        ));
    }
    Ok(if blocks < 32768 {
        1024
    } else if blocks < 262144 {
        4096
    } else if blocks < 524288 {
        8192
    } else if blocks < 4_194_304 {
        16384
    } else if blocks < 8_388_608 {
        32768
    } else if blocks < 16_777_216 {
        65536
    } else if blocks < 33_554_432 {
        131072
    } else {
        262144
    })
}

/// True when the block bitmap of `group` may be left uninitialized: the
/// group holds only its own superblock and descriptor backup.  Any other
/// metadata placed in the group, including packed flex_bg tables of other
/// groups, is set in its bitmap and rules this out.  The last group is
/// never uninitialized, as e2fsck requires.
fn group_block_bitmap_only_owns_backup(
    sb: &Superblock,
    geometry: &Geometry,
    maps: &[Vec<u8>],
    group: u32,
) -> bool {
    if group + 1 == geometry.groups
        || !sb.group_has_super(group)
        || blocks_in_group(geometry, group) != geometry.blocks_per_group as u64
    {
        return false;
    }
    let backup = 1 + geometry.gdt_blocks as usize;
    let valid = blocks_in_group(geometry, group) as usize;
    (backup..valid).all(|bit| !test_bit(&maps[group as usize], bit))
}

fn count_set_bits(bitmap: &[u8], limit: usize) -> usize {
    let full = limit / 8;
    let rest = limit % 8;
    let mut count: usize = bitmap[..full]
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum();
    if rest != 0 {
        count += (bitmap[full] & ((1 << rest) - 1)).count_ones() as usize;
    }
    count
}

fn write_super_and_gdts(
    dev: &dyn BlockDevice,
    sb: &Superblock,
    geometry: &Geometry,
    groups: &[GroupDesc],
) -> Result<()> {
    let mut gdt = vec![0; geometry.gdt_blocks as usize * geometry.block_size as usize];
    for (group, desc) in groups.iter().enumerate() {
        let off = group * geometry.desc_size;
        gdt[off..off + geometry.desc_size].copy_from_slice(&desc.raw);
    }
    for group in 0..geometry.groups {
        if !sb.group_has_super(group) {
            continue;
        }
        let start = group_first(geometry, group);
        let mut copy = sb.clone();
        copy.set_block_group_nr(group as u16);
        copy.update_checksum();
        if group == 0 {
            write_main_superblock(dev, &copy, geometry.block_size)?;
        } else {
            let mut block = vec![0; geometry.block_size as usize];
            block[..1024].copy_from_slice(&copy.raw);
            write_block(dev, start, geometry.block_size, &block)?;
        }
        for i in 0..geometry.gdt_blocks as usize {
            let from = i * geometry.block_size as usize;
            write_block(
                dev,
                start + 1 + i as u64,
                geometry.block_size,
                &gdt[from..from + geometry.block_size as usize],
            )?;
        }
    }
    Ok(())
}

fn write_main_superblock(dev: &dyn BlockDevice, sb: &Superblock, block_size: u32) -> Result<()> {
    write_bytes(dev, 1024, &sb.raw)?;
    if block_size == 1024 {
        return Ok(());
    }
    Ok(())
}

fn write_inode(
    dev: &dyn BlockDevice,
    geometry: &Geometry,
    layouts: &[GroupLayout],
    ino: u32,
    inode: &Inode,
) -> Result<()> {
    let zero_based = ino
        .checked_sub(1)
        .ok_or_else(|| Error::corrupt("inode zero"))?;
    let group = zero_based / geometry.inodes_per_group;
    let index = zero_based % geometry.inodes_per_group;
    let offset = layouts[group as usize].inode_table * geometry.block_size as u64
        + index as u64 * geometry.inode_size as u64;
    write_bytes(dev, offset, &inode.raw)
}

fn write_block(dev: &dyn BlockDevice, block: u64, block_size: u32, data: &[u8]) -> Result<()> {
    if data.len() != block_size as usize {
        return Err(Error::corrupt("mkfs attempted a short block write"));
    }
    write_bytes(dev, block * block_size as u64, data)
}

fn write_zeros(dev: &dyn BlockDevice, offset: u64, len: usize) -> Result<()> {
    let zero = vec![0; len];
    write_bytes(dev, offset, &zero)
}

fn write_bytes(dev: &dyn BlockDevice, offset: u64, data: &[u8]) -> Result<()> {
    let alignment = dev.alignment().max(1) as u64;
    if alignment == 1 || (offset % alignment == 0 && data.len() as u64 % alignment == 0) {
        dev.write_at(offset, data)?;
        return Ok(());
    }
    let begin = offset / alignment * alignment;
    let end = (offset + data.len() as u64).div_ceil(alignment) * alignment;
    if end > dev.size() {
        return Err(Error::invalid("write extends beyond device"));
    }
    let mut scratch = vec![0; (end - begin) as usize];
    dev.read_at(begin, &mut scratch)?;
    let from = (offset - begin) as usize;
    scratch[from..from + data.len()].copy_from_slice(data);
    dev.write_at(begin, &scratch)?;
    Ok(())
}

fn random_array() -> Result<[u8; 16]> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn random_u32() -> Result<u32> {
    let mut bytes = [0; 4];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u32::from_ne_bytes(bytes))
}

fn now_timestamp() -> Timestamp {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        secs: duration.as_secs() as i64,
        nsecs: duration.subsec_nanos(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_devices_default_to_one_kib_blocks() {
        let opts = MkfsOptions::default();
        assert_eq!(select_block_size(4 * 1024 * 1024, &opts).unwrap(), 1024);
        assert_eq!(select_block_size(512 * 1024 * 1024, &opts).unwrap(), 4096);
    }

    #[test]
    fn inode_geometry_fills_table_blocks() {
        let geo = geometry(64 * 1024 * 1024, 1024, 256, 4096, false).unwrap();
        assert_eq!(geo.inodes_per_group % 8, 0);
        assert_eq!(
            (geo.inodes_per_group as u64 * geo.inode_size as u64) % geo.block_size as u64,
            0
        );
        assert!(geo.inodes_per_group <= 8 * geo.block_size);
    }

    #[test]
    fn sparse_super_groups_match_format_rule() {
        let geo = geometry(64 * 1024 * 1024, 1024, 256, 4096, false).unwrap();
        assert_eq!(group_overhead(&geo, 0), 1 + geo.gdt_blocks as u64);
        assert_eq!(group_overhead(&geo, 1), 1 + geo.gdt_blocks as u64);
        assert_eq!(group_overhead(&geo, 2), 0);
        assert_eq!(group_overhead(&geo, 3), 1 + geo.gdt_blocks as u64);
    }
}
