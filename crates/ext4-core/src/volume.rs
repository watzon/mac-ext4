//! The mounted-volume state: device, superblock, group descriptors, the
//! metadata block cache, and the running transaction.
//!
//! All mutation goes through [`Volume::block_mut`] / [`Volume::write_inode`]
//! (metadata, staged in the running transaction) or [`Volume::write_data`]
//! (file contents, written in place before the metadata commit, which gives
//! jbd2 "ordered" semantics). [`Volume::commit`] makes the transaction
//! durable through the journal when the volume has one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::cache::BlockCache;
use crate::device::{AlignedDevice, BlockDevice};
use crate::error::{Error, Result};
use crate::group::GroupDesc;
use crate::inode::Inode;
use crate::journal::Journal;
use crate::superblock::*;

/// Options chosen by the caller at mount time.
#[derive(Clone, Debug, Default)]
pub struct MountOptions {
    /// Never write to the device. Journal replay happens in memory only.
    pub read_only: bool,
    /// Accept metadata checksum mismatches instead of failing (read-only use).
    pub ignore_checksums: bool,
}

/// Counters describing what mount-time recovery did.
#[derive(Clone, Debug, Default)]
pub struct RecoveryReport {
    pub journal_transactions_replayed: u32,
    pub journal_blocks_replayed: u32,
    pub orphans_processed: u32,
}

pub struct Volume {
    pub(crate) dev: Box<dyn BlockDevice>,
    pub sb: Superblock,
    pub groups: Vec<GroupDesc>,
    pub(crate) block_size: usize,
    pub(crate) cache: Mutex<BlockCache>,
    /// Metadata blocks modified by the running transaction.
    pub(crate) dirty: BTreeMap<u64, Arc<Vec<u8>>>,
    /// Journal blocks replayed in memory on a read-only mount.
    pub(crate) overlay: BTreeMap<u64, Arc<Vec<u8>>>,
    pub(crate) dirty_groups: BTreeSet<u32>,
    pub(crate) sb_dirty: bool,
    /// Clusters freed by the running transaction. They are not reused until
    /// the transaction commits, so a crash cannot expose new data through
    /// old metadata.
    pub(crate) freed_in_txn: Vec<(u64, u64)>,
    pub(crate) journal: Option<Journal>,
    pub(crate) read_only: bool,
    pub(crate) read_only_reason: Option<String>,
    pub(crate) ignore_checksums: bool,
    pub(crate) last_commit: Instant,
    pub(crate) mounted_rw: bool,
    /// In-memory orphan list (inode numbers linked through `i_dtime`),
    /// head first, mirroring `s_last_orphan`.
    pub(crate) orphans: Vec<u32>,
    /// Undo log of the operation in progress, see [`Volume::begin_op`].
    undo: Option<Undo>,
    pub recovery: RecoveryReport,
}

/// State needed to roll back one failed operation without discarding the
/// rest of the running transaction.
struct Undo {
    /// Prior staged content of each block the operation touched
    /// (`None` = the block was not staged before).
    blocks: std::collections::HashMap<u64, Option<Arc<Vec<u8>>>>,
    groups: std::collections::HashMap<u32, GroupDesc>,
    dirty_groups: BTreeSet<u32>,
    sb: Superblock,
    sb_dirty: bool,
    freed_len: usize,
    orphans: Vec<u32>,
}

impl Volume {
    /// Open the file system on `dev`. Replays the journal and processes
    /// orphans when needed.
    pub fn open(dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Volume> {
        let dev: Box<dyn BlockDevice> = if dev.alignment() > 1 {
            Box::new(AlignedDevice::new(dev))
        } else {
            dev
        };
        let sb = read_superblock(&*dev)?;
        if !sb.verify_checksum() && !opts.ignore_checksums {
            return Err(Error::corrupt("superblock checksum mismatch"));
        }
        if sb.has_metadata_csum() && sb.checksum_type() != 1 {
            return Err(Error::unsupported(format!(
                "unknown checksum type {}",
                sb.checksum_type()
            )));
        }
        let unknown = sb.unknown_read_features();
        if unknown != 0 {
            return Err(Error::unsupported(format!(
                "incompatible features {unknown:#x}"
            )));
        }
        if sb.has_incompat(incompat::JOURNAL_DEV) {
            return Err(Error::unsupported(
                "this volume is an external journal device",
            ));
        }

        let mut read_only_reason = sb.read_only_reason();
        if dev.read_only() && read_only_reason.is_none() {
            read_only_reason = Some("the device is read-only".into());
        }
        if sb.state() & STATE_ERROR != 0 && read_only_reason.is_none() {
            read_only_reason = Some("the file system has errors; run a check first".into());
        }
        if sb.has_compat(compat::HAS_JOURNAL) && (sb.journal_inum() == 0 || sb.journal_dev() != 0) {
            if sb.has_incompat(incompat::RECOVER) {
                return Err(Error::unsupported("external journals that need recovery"));
            }
            read_only_reason
                .get_or_insert_with(|| "external journals are not supported for writing".into());
        }
        let read_only = opts.read_only || read_only_reason.is_some();

        let block_size = sb.block_size() as usize;
        let mut vol = Volume {
            dev,
            sb,
            groups: Vec::new(),
            block_size,
            cache: Mutex::new(BlockCache::new(cache_capacity(block_size))),
            dirty: BTreeMap::new(),
            overlay: BTreeMap::new(),
            dirty_groups: BTreeSet::new(),
            sb_dirty: false,
            freed_in_txn: Vec::new(),
            journal: None,
            read_only,
            read_only_reason,
            ignore_checksums: opts.ignore_checksums,
            last_commit: Instant::now(),
            mounted_rw: false,
            orphans: Vec::new(),
            undo: None,
            recovery: RecoveryReport::default(),
        };
        vol.load_groups()?;
        vol.load_journal()?;
        if !vol.read_only {
            vol.process_orphans()?;
            vol.mark_mounted()?;
        }
        Ok(vol)
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn read_only_reason(&self) -> Option<&str> {
        self.read_only_reason.as_deref()
    }

    pub fn device(&self) -> &dyn BlockDevice {
        &*self.dev
    }

    pub(crate) fn ensure_writable(&self) -> Result<()> {
        if self.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }

    // ---- geometry ----------------------------------------------------

    pub fn group_count(&self) -> u32 {
        self.groups.len() as u32
    }

    pub fn group_first_block(&self, g: u32) -> u64 {
        self.sb.first_data_block() as u64 + g as u64 * self.sb.blocks_per_group() as u64
    }

    /// Number of blocks in group `g` (the last group may be short).
    pub fn blocks_in_group(&self, g: u32) -> u64 {
        let start = self.group_first_block(g);
        (self.sb.blocks_count() - start).min(self.sb.blocks_per_group() as u64)
    }

    pub fn group_of_block(&self, blk: u64) -> u32 {
        ((blk - self.sb.first_data_block() as u64) / self.sb.blocks_per_group() as u64) as u32
    }

    pub fn descs_per_block(&self) -> u32 {
        self.block_size as u32 / self.sb.desc_size() as u32
    }

    /// Physical block holding group-descriptor block number `i`.
    pub fn gdt_block(&self, i: u32) -> u64 {
        let sb_block = self.sb.first_data_block() as u64;
        if !self.sb.has_incompat(incompat::META_BG) || i < self.sb.first_meta_bg() {
            return sb_block + 1 + i as u64;
        }
        let g = i * self.descs_per_block();
        self.group_first_block(g) + if self.sb.group_has_super(g) { 1 } else { 0 }
    }

    /// Number of GDT blocks in use (excluding reserved GDT blocks).
    pub fn gdt_blocks(&self) -> u32 {
        self.sb.group_count().div_ceil(self.descs_per_block())
    }

    /// Byte offset of the superblock inside the block that contains it.
    fn sb_location(&self) -> (u64, usize) {
        (
            SUPERBLOCK_OFFSET / self.block_size as u64,
            (SUPERBLOCK_OFFSET % self.block_size as u64) as usize,
        )
    }

    fn load_groups(&mut self) -> Result<()> {
        let count = self.sb.group_count();
        let ds = self.sb.desc_size() as usize;
        let dpb = self.descs_per_block();
        let mut groups = Vec::with_capacity(count as usize);
        for i in 0..self.gdt_blocks() {
            let blk = self.read_block(self.gdt_block(i))?;
            for j in 0..dpb {
                let g = i * dpb + j;
                if g >= count {
                    break;
                }
                let off = j as usize * ds;
                let gd = GroupDesc::from_bytes(&blk[off..off + ds]);
                if !gd.verify_checksum(&self.sb, g) && !self.ignore_checksums {
                    return Err(Error::corrupt(format!(
                        "group descriptor {g} checksum mismatch"
                    )));
                }
                groups.push(gd);
            }
        }
        self.groups = groups;
        Ok(())
    }

    // ---- block I/O ---------------------------------------------------

    /// Read metadata block `blk`, seeing staged and replayed changes.
    pub fn read_block(&self, blk: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(d) = self.dirty.get(&blk) {
            return Ok(d.clone());
        }
        if let Some(d) = self.overlay.get(&blk) {
            return Ok(d.clone());
        }
        if let Some(d) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(blk)
        {
            return Ok(d);
        }
        let data = Arc::new(self.read_block_raw(blk)?);
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(blk, data.clone());
        Ok(data)
    }

    /// Run `f` over metadata block `blk` without copying staged blocks.
    pub fn with_block<T>(&self, blk: u64, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        if let Some(d) = self.dirty.get(&blk) {
            return Ok(f(d));
        }
        let b = self.read_block(blk)?;
        Ok(f(&b))
    }

    fn check_range(&self, blk: u64, count: u64) -> Result<()> {
        let total = self.sb.blocks_count();
        if blk >= total || count > total - blk {
            return Err(Error::corrupt(format!(
                "block {blk} (+{count}) is outside the file system"
            )));
        }
        Ok(())
    }

    fn read_block_raw(&self, blk: u64) -> Result<Vec<u8>> {
        self.check_range(blk, 1)?;
        let mut buf = vec![0u8; self.block_size];
        self.dev.read_at(blk * self.block_size as u64, &mut buf)?;
        Ok(buf)
    }

    fn record_undo(&mut self, blk: u64) {
        if let Some(undo) = self.undo.as_mut() {
            undo.blocks
                .entry(blk)
                .or_insert_with(|| self.dirty.get(&blk).cloned());
        }
    }

    /// Stage metadata block `blk` for modification.
    pub fn block_mut(&mut self, blk: u64) -> Result<&mut Vec<u8>> {
        self.ensure_writable()?;
        self.record_undo(blk);
        if !self.dirty.contains_key(&blk) {
            let cur = self.read_block(blk)?;
            self.dirty.insert(blk, cur);
        }
        Ok(Arc::make_mut(
            self.dirty.get_mut(&blk).expect("just inserted"),
        ))
    }

    /// Stage a freshly allocated metadata block, starting from zeros.
    pub fn new_block_mut(&mut self, blk: u64) -> Result<&mut Vec<u8>> {
        self.ensure_writable()?;
        self.check_range(blk, 1)?;
        self.record_undo(blk);
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(blk);
        self.dirty.insert(blk, Arc::new(vec![0; self.block_size]));
        Ok(Arc::make_mut(
            self.dirty.get_mut(&blk).expect("just inserted"),
        ))
    }

    /// Drop a staged metadata block that is being freed in this transaction.
    pub(crate) fn forget_block(&mut self, blk: u64) {
        self.record_undo(blk);
        self.dirty.remove(&blk);
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(blk);
    }

    /// Read `count` contiguous file-data blocks starting at `blk` into `buf`.
    pub fn read_data(&self, blk: u64, buf: &mut [u8]) -> Result<()> {
        let count = (buf.len() as u64).div_ceil(self.block_size as u64);
        self.check_range(blk, count)?;
        if !self.overlay.is_empty() || !self.dirty.is_empty() {
            // Data blocks are never journaled by this engine, but a replayed
            // Linux journal (data=journal) or a staged directory block may
            // cover them. Serve those block-by-block.
            let bs = self.block_size;
            let mut any = false;
            for i in 0..count {
                if self.dirty.contains_key(&(blk + i)) || self.overlay.contains_key(&(blk + i)) {
                    any = true;
                    break;
                }
            }
            if any {
                for (i, chunk) in buf.chunks_mut(bs).enumerate() {
                    let b = self.read_block(blk + i as u64)?;
                    chunk.copy_from_slice(&b[..chunk.len()]);
                }
                return Ok(());
            }
        }
        self.dev.read_at(blk * self.block_size as u64, buf)?;
        Ok(())
    }

    /// Write file data in place. `buf` must cover whole blocks.
    pub fn write_data(&mut self, blk: u64, buf: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        debug_assert!(buf.len() % self.block_size == 0);
        let count = (buf.len() / self.block_size) as u64;
        self.check_range(blk, count)?;
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            for i in 0..count {
                cache.remove(blk + i);
            }
        }
        self.dev.write_at(blk * self.block_size as u64, buf)?;
        Ok(())
    }

    /// Write zeros over `count` blocks starting at `blk`.
    pub fn zero_data(&mut self, blk: u64, count: u64) -> Result<()> {
        const CHUNK: u64 = 256;
        let zeros = vec![0u8; (CHUNK.min(count) as usize) * self.block_size];
        let mut done = 0;
        while done < count {
            let n = CHUNK.min(count - done);
            self.write_data(blk + done, &zeros[..n as usize * self.block_size])?;
            done += n;
        }
        Ok(())
    }

    // ---- inodes ------------------------------------------------------

    pub fn inode_location(&self, ino: u32) -> Result<(u64, usize)> {
        if ino == 0 || ino > self.sb.inodes_count() {
            return Err(Error::corrupt(format!(
                "inode number {ino} is out of range"
            )));
        }
        let ipg = self.sb.inodes_per_group();
        let g = (ino - 1) / ipg;
        let idx = ((ino - 1) % ipg) as u64;
        let isz = self.sb.inode_size() as u64;
        let table = self.groups[g as usize].inode_table();
        let byte = idx * isz;
        Ok((
            table + byte / self.block_size as u64,
            (byte % self.block_size as u64) as usize,
        ))
    }

    pub fn read_inode(&self, ino: u32) -> Result<Inode> {
        let (blk, off) = self.inode_location(ino)?;
        let isz = self.sb.inode_size() as usize;
        let inode = self.with_block(blk, |b| Inode::from_bytes(&b[off..off + isz]))?;
        if !self.ignore_checksums
            && !inode.verify_checksum(&self.sb, ino)
            && !is_unused_slot(&inode)
        {
            return Err(Error::corrupt(format!("inode {ino} checksum mismatch")));
        }
        Ok(inode)
    }

    pub fn write_inode(&mut self, ino: u32, inode: &Inode) -> Result<()> {
        let (blk, off) = self.inode_location(ino)?;
        let mut inode = inode.clone();
        inode.update_checksum(&self.sb, ino);
        let isz = self.sb.inode_size() as usize;
        let b = self.block_mut(blk)?;
        b[off..off + isz].copy_from_slice(&inode.raw[..isz]);
        Ok(())
    }

    /// Modify group descriptor `g` (checksummed and written at commit).
    pub fn group_mut(&mut self, g: u32) -> &mut GroupDesc {
        if let Some(undo) = self.undo.as_mut() {
            undo.groups
                .entry(g)
                .or_insert_with(|| self.groups[g as usize].clone());
        }
        self.dirty_groups.insert(g);
        &mut self.groups[g as usize]
    }

    pub fn mark_sb_dirty(&mut self) {
        self.sb_dirty = true;
    }

    // ---- operation scope ---------------------------------------------

    /// Start an operation whose changes can be rolled back as a unit.
    pub(crate) fn begin_op(&mut self) {
        debug_assert!(self.undo.is_none(), "operations do not nest");
        self.undo = Some(Undo {
            blocks: Default::default(),
            groups: Default::default(),
            dirty_groups: self.dirty_groups.clone(),
            sb: self.sb.clone(),
            sb_dirty: self.sb_dirty,
            freed_len: self.freed_in_txn.len(),
            orphans: self.orphans.clone(),
        });
    }

    /// Finish the operation started by [`Volume::begin_op`]. On failure, the
    /// operation's metadata changes are rolled back; earlier operations in
    /// the running transaction are kept.
    pub(crate) fn end_op<T>(&mut self, result: Result<T>) -> Result<T> {
        let Some(undo) = self.undo.take() else {
            return result;
        };
        match result {
            Ok(v) => {
                self.maybe_commit()?;
                Ok(v)
            }
            Err(e) => {
                for (blk, prior) in undo.blocks {
                    match prior {
                        Some(d) => {
                            self.dirty.insert(blk, d);
                        }
                        None => {
                            self.dirty.remove(&blk);
                        }
                    }
                }
                for (g, gd) in undo.groups {
                    self.groups[g as usize] = gd;
                }
                self.dirty_groups = undo.dirty_groups;
                self.sb = undo.sb;
                self.sb_dirty = undo.sb_dirty;
                self.freed_in_txn.truncate(undo.freed_len);
                self.orphans = undo.orphans;
                Err(e)
            }
        }
    }

    // ---- journal -----------------------------------------------------

    fn load_journal(&mut self) -> Result<()> {
        if !self.sb.has_compat(compat::HAS_JOURNAL)
            || self.sb.journal_inum() == 0
            || self.sb.journal_dev() != 0
        {
            if self.sb.has_incompat(incompat::RECOVER) && !self.sb.has_compat(compat::HAS_JOURNAL) {
                return Err(Error::corrupt(
                    "needs_recovery is set but there is no journal",
                ));
            }
            return Ok(());
        }
        let inum = self.sb.journal_inum();
        let inode = self.read_inode(inum)?;
        let runs = crate::bmap::physical_runs(self, &inode)?;
        let mut journal = Journal::open(&*self.dev, self.block_size as u32, runs, self.sb.uuid())?;
        if !self.read_only && self.sb.blocks_count() > u64::from(u32::MAX) {
            journal.require_64bit(&*self.dev)?;
        }
        let needs = journal.needs_recovery() || self.sb.has_incompat(incompat::RECOVER);
        self.journal = Some(journal);
        if needs && self.journal.as_ref().is_some_and(|j| j.needs_recovery()) {
            self.replay_journal()?;
        } else if needs && !self.read_only {
            // Flag set but journal empty: just clear the flag on mount.
            self.sb
                .set_feature_incompat(self.sb.feature_incompat() & !incompat::RECOVER);
            self.sb_dirty = true;
        }
        Ok(())
    }

    fn replay_journal(&mut self) -> Result<()> {
        let journal = self.journal.as_ref().expect("journal loaded");
        let recovery = journal.scan_recovery(&*self.dev)?;
        self.recovery.journal_transactions_replayed = recovery.transactions;
        self.recovery.journal_blocks_replayed = recovery.blocks.len() as u32;
        if self.read_only {
            self.overlay = journal
                .read_replay_blocks(&*self.dev, &recovery)?
                .into_iter()
                .map(|(b, d)| (b, Arc::new(d)))
                .collect();
        } else {
            let bs = self.block_size as u64;
            journal.read_replay(&*self.dev, &recovery, |first, data| {
                self.check_range(first, data.len() as u64 / bs)?;
                self.dev.write_at(first * bs, data)?;
                Ok(())
            })?;
            self.dev.flush()?;
            let journal = self.journal.as_mut().expect("journal loaded");
            journal.finish_recovery(&*self.dev, recovery.next_sequence)?;
        }
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // The journal may have rewritten the superblock and descriptors.
        let (blk, off) = self.sb_location();
        let sbb = self.read_block(blk)?;
        let mut sb = Superblock::parse(&sbb[off..off + SUPERBLOCK_SIZE])?;
        // Linux keeps the superblock's error state and counters in memory
        // during replay; the replayed copy is authoritative for geometry.
        if !self.read_only {
            sb.set_feature_incompat(sb.feature_incompat() & !incompat::RECOVER);
            self.sb_dirty = true;
        }
        self.sb = sb;
        self.load_groups()?;
        Ok(())
    }

    /// Maximum number of metadata blocks one transaction may hold.
    pub(crate) fn txn_capacity(&self) -> usize {
        match &self.journal {
            Some(j) => j.max_transaction_blocks(),
            None => usize::MAX,
        }
    }

    /// True when the running transaction holds changes.
    pub fn has_pending(&self) -> bool {
        !self.dirty.is_empty() || !self.dirty_groups.is_empty() || self.sb_dirty
    }

    /// Commit when the transaction is large or old.
    pub fn maybe_commit(&mut self) -> Result<()> {
        if !self.has_pending() {
            return Ok(());
        }
        let staged =
            self.dirty.len() + self.dirty_groups.len() / self.descs_per_block().max(1) as usize + 2;
        if staged * 4 >= self.txn_capacity() || self.last_commit.elapsed().as_secs() >= 5 {
            self.commit()?;
        }
        Ok(())
    }

    /// Commit the running transaction.
    pub fn commit(&mut self) -> Result<()> {
        self.commit_inner(true)
    }

    /// Durably write the running transaction's commit block without
    /// checkpointing its metadata to home blocks, then prevent Drop from
    /// touching the volume. Interoperability crash tests only.
    #[doc(hidden)]
    pub fn commit_without_checkpoint_for_test(&mut self) -> Result<()> {
        self.commit_inner(false)?;
        self.mounted_rw = false;
        Ok(())
    }

    fn commit_inner(&mut self, checkpoint: bool) -> Result<()> {
        if self.read_only || !self.has_pending() {
            return Ok(());
        }
        debug_assert!(self.undo.is_none(), "commit inside an operation");
        // Fold descriptor and superblock changes into their blocks. Staging
        // may need to read a block and can fail; the pending state is only
        // cleared once everything is staged, so a failure here leaves the
        // running transaction whole and a later commit can retry it.
        let ds = self.sb.desc_size() as usize;
        let dpb = self.descs_per_block();
        let groups: Vec<u32> = self.dirty_groups.iter().copied().collect();
        for g in groups {
            let mut gd = self.groups[g as usize].clone();
            gd.update_checksum(&self.sb, g);
            self.groups[g as usize] = gd.clone();
            let blk = self.gdt_block(g / dpb);
            let off = (g % dpb) as usize * ds;
            self.block_mut(blk)?[off..off + ds].copy_from_slice(&gd.raw[..ds]);
        }
        if self.sb_dirty || !self.dirty.is_empty() {
            self.sb.set_wtime(now_secs() as u32);
            self.sb.set_wtime_hi((now_secs() >> 32) as u8);
            self.sb.update_checksum();
            let (blk, off) = self.sb_location();
            let raw = self.sb.raw;
            self.block_mut(blk)?[off..off + SUPERBLOCK_SIZE].copy_from_slice(&raw);
        }
        self.dirty_groups.clear();
        self.sb_dirty = false;

        let blocks: Vec<(u64, &[u8])> =
            self.dirty.iter().map(|(b, d)| (*b, d.as_slice())).collect();
        let dev = &*self.dev;
        let result = match self.journal.as_mut() {
            Some(j) if checkpoint => j.commit(dev, &blocks),
            Some(j) => j.commit_without_checkpoint(dev, &blocks),
            None if checkpoint => (|| {
                dev.flush()?;
                crate::journal::write_home_blocks(dev, self.block_size, &blocks)?;
                dev.flush()?;
                Ok(())
            })(),
            None => Err(Error::unsupported(
                "crash-after-commit test requires an internal journal",
            )),
        };
        drop(blocks);
        if let Err(e) = result {
            // The transaction did not reach the disk in a known state;
            // refuse further writes so the on-disk state stays consistent
            // with the last successful commit. The staged blocks stay in
            // `dirty` so reads keep matching the in-memory metadata.
            self.read_only = true;
            self.read_only_reason = Some(format!("a journal commit failed: {e}"));
            return Err(e);
        }
        let dirty = std::mem::take(&mut self.dirty);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for (blk, data) in dirty {
            cache.insert(blk, data);
        }
        drop(cache);
        self.freed_in_txn.clear();
        self.last_commit = Instant::now();
        Ok(())
    }

    fn mark_mounted(&mut self) -> Result<()> {
        let now = now_secs();
        if self.journal.is_some() {
            self.sb
                .set_feature_incompat(self.sb.feature_incompat() | incompat::RECOVER);
        } else {
            self.sb.set_state(self.sb.state() & !STATE_VALID);
        }
        self.sb.set_mnt_count(self.sb.mnt_count().wrapping_add(1));
        self.sb.set_mtime(now as u32);
        self.sb.set_mtime_hi((now >> 32) as u8);
        self.sb_dirty = true;
        self.mounted_rw = true;
        self.commit()
    }

    /// Commit everything and mark the file system clean.
    pub fn unmount(&mut self) -> Result<()> {
        if !self.mounted_rw || self.read_only {
            return Ok(());
        }
        self.release_orphans()?;
        self.commit()?;
        if self.journal.is_some() {
            self.sb
                .set_feature_incompat(self.sb.feature_incompat() & !incompat::RECOVER);
        } else {
            self.sb.set_state(self.sb.state() | STATE_VALID);
        }
        self.sb_dirty = true;
        self.commit()?;
        self.dev.flush()?;
        self.mounted_rw = false;
        Ok(())
    }
}

fn is_unused_slot(inode: &Inode) -> bool {
    inode.raw.iter().all(|&b| b == 0)
}

fn cache_capacity(block_size: usize) -> usize {
    // About 32 MiB of metadata.
    (32 << 20) / block_size
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read and parse the primary superblock.
pub fn read_superblock(dev: &dyn BlockDevice) -> Result<Superblock> {
    let mut buf = vec![0u8; SUPERBLOCK_SIZE];
    dev.read_at(SUPERBLOCK_OFFSET, &mut buf)?;
    Superblock::parse(&buf)
}

/// A summary of a volume, cheap enough for probing.
#[derive(Clone, Debug)]
pub struct ProbeInfo {
    pub label: String,
    pub uuid: [u8; 16],
    /// "ext2", "ext3", or "ext4".
    pub flavor: &'static str,
    pub block_size: u32,
    pub blocks: u64,
    pub readable: bool,
    pub writable: bool,
    pub note: Option<String>,
}

/// Identify an ext2/3/4 file system without mounting it.
pub fn probe(dev: &dyn BlockDevice) -> Result<ProbeInfo> {
    let sb = if dev.alignment() > 1 {
        read_superblock(&AlignedDevice::new(ProbeRef(dev)))?
    } else {
        read_superblock(dev)?
    };
    let unknown = sb.unknown_read_features();
    let readable = unknown == 0 && !sb.has_incompat(incompat::JOURNAL_DEV);
    let mut note = sb.read_only_reason();
    if !readable {
        note = Some(format!("unsupported incompatible features {unknown:#x}"));
    }
    Ok(ProbeInfo {
        label: String::from_utf8_lossy(sb.volume_name()).into_owned(),
        uuid: sb.uuid(),
        flavor: flavor(&sb),
        block_size: sb.block_size(),
        blocks: sb.blocks_count(),
        readable,
        writable: readable && note.is_none() && !dev.read_only(),
        note,
    })
}

/// Pick the conventional name for the feature set.
pub fn flavor(sb: &Superblock) -> &'static str {
    let ext4_incompat = incompat::EXTENTS
        | incompat::BIT64
        | incompat::FLEX_BG
        | incompat::INLINE_DATA
        | incompat::MMP;
    let ext4_ro = ro_compat::HUGE_FILE
        | ro_compat::GDT_CSUM
        | ro_compat::DIR_NLINK
        | ro_compat::EXTRA_ISIZE
        | ro_compat::METADATA_CSUM
        | ro_compat::BIGALLOC;
    if sb.has_incompat(ext4_incompat) || sb.has_ro_compat(ext4_ro) {
        "ext4"
    } else if sb.has_compat(compat::HAS_JOURNAL) {
        "ext3"
    } else {
        "ext2"
    }
}

struct ProbeRef<'a>(&'a dyn BlockDevice);

impl BlockDevice for ProbeRef<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.0.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.0.write_at(offset, buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        self.0.flush()
    }
    fn size(&self) -> u64 {
        self.0.size()
    }
    fn alignment(&self) -> u32 {
        self.0.alignment()
    }
}

#[allow(dead_code)]
fn assert_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<Volume>();
}
