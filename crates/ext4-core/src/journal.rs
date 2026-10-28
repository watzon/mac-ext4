//! jbd2 journal: recovery (replay) and transaction commit.
//!
//! jbd2 structures are big-endian, unlike ext4 metadata. This module keeps
//! journal block addressing private so callers only need to provide the inode
//! block map that backs an internal journal.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crc::{crc32_be, crc32c};
use crate::device::BlockDevice;
use crate::error::{Error, Result};

const MAGIC: u32 = 0xC03B_3998;
const DESCRIPTOR_BLOCK: u32 = 1;
const COMMIT_BLOCK: u32 = 2;
const SUPERBLOCK_V1: u32 = 3;
const SUPERBLOCK_V2: u32 = 4;
const REVOKE_BLOCK: u32 = 5;

const COMPAT_CHECKSUM: u32 = 0x1;
const INCOMPAT_REVOKE: u32 = 0x1;
const INCOMPAT_64BIT: u32 = 0x2;
const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
const INCOMPAT_CSUM_V2: u32 = 0x8;
const INCOMPAT_CSUM_V3: u32 = 0x10;
const INCOMPAT_FAST_COMMIT: u32 = 0x20;
const KNOWN_INCOMPAT: u32 = INCOMPAT_REVOKE
    | INCOMPAT_64BIT
    | INCOMPAT_ASYNC_COMMIT
    | INCOMPAT_CSUM_V2
    | INCOMPAT_CSUM_V3
    | INCOMPAT_FAST_COMMIT;

const TAG_ESCAPE: u32 = 0x1;
const TAG_SAME_UUID: u32 = 0x2;
const TAG_LAST: u32 = 0x8;

const SUPERBLOCK_BYTES: usize = 1024;
const SUPER_CHECKSUM_OFFSET: usize = 0xFC;
const HEADER_BYTES: usize = 12;
const COMMIT_CHECKSUM_OFFSET: usize = 0x10;
/// `h_chksum_type` value for the checksum-v1 (`COMPAT_CHECKSUM`) CRC32.
const CRC32_CHKSUM: u8 = 1;
const CRC32_CHKSUM_SIZE: u8 = 4;
/// Upper bound on the bytes moved by one coalesced device read or write.
/// It also bounds the memory that replay and commit use for staging.
const IO_BATCH_BYTES: usize = 4 << 20;

/// A contiguous run of journal blocks: `len` file-system blocks starting at
/// journal logical block `logical`, stored at physical block `physical`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub logical: u64,
    pub physical: u64,
    pub len: u64,
}

/// Where the final copy of a replayed block lives in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogCopy {
    /// Journal logical block that holds the copy.
    pub position: u32,
    /// The copy started with the jbd2 magic number, which was zeroed when it
    /// was logged and must be put back.
    pub escaped: bool,
}

/// Result of scanning the journal for committed transactions.
///
/// Scanning keeps only where each block lives in the log, not its content,
/// so memory does not grow with the journal payload. Use
/// [`Journal::read_replay`] or [`Journal::read_replay_blocks`] to get the
/// block contents.
#[derive(Debug, Default)]
pub struct Recovery {
    /// Every file-system block the journal replays, after revoke processing
    /// (last committed copy wins), and where its final copy is in the log.
    pub blocks: BTreeMap<u64, LogCopy>,
    /// Number of committed transactions found.
    pub transactions: u32,
    /// Sequence number the next transaction must use.
    pub next_sequence: u32,
}

#[derive(Clone, Copy)]
struct Features {
    compat: u32,
    incompat: u32,
}

impl Features {
    fn has_v2_checksum(self) -> bool {
        self.incompat & INCOMPAT_CSUM_V2 != 0
    }

    fn has_v3_checksum(self) -> bool {
        self.incompat & INCOMPAT_CSUM_V3 != 0
    }

    fn has_block_checksum(self) -> bool {
        self.has_v2_checksum() || self.has_v3_checksum()
    }

    fn has_64bit(self) -> bool {
        self.incompat & INCOMPAT_64BIT != 0
    }

    fn has_fast_commit(self) -> bool {
        self.incompat & INCOMPAT_FAST_COMMIT != 0
    }

    fn has_v1_checksum(self) -> bool {
        self.compat & COMPAT_CHECKSUM != 0 && !self.has_block_checksum()
    }
}

#[derive(Clone)]
struct Tag {
    block: u64,
    flags: u32,
    checksum: Option<u32>,
}

struct Transaction {
    sequence: u32,
    writes: Vec<(u64, LogCopy)>,
    revokes: Vec<u64>,
    /// Running checksum-v1 CRC over descriptor and data blocks in log order.
    v1_checksum: u32,
}

impl Transaction {
    fn new(sequence: u32) -> Self {
        Transaction {
            sequence,
            writes: Vec::new(),
            revokes: Vec::new(),
            v1_checksum: !0,
        }
    }
}

/// True when transaction id `x` is newer than `y`, allowing for wrap.
/// This is Linux `tid_gt`.
fn tid_gt(x: u32, y: u32) -> bool {
    (x.wrapping_sub(y) as i32) > 0
}

pub struct Journal {
    block_size: usize,
    runs: Vec<Run>,
    first: u32,
    log_end: u32,
    sequence: u32,
    start: u32,
    features: Features,
    checksum_seed: u32,
    superblock: Vec<u8>,
}

impl Journal {
    /// Open the internal journal described by `runs` (the journal inode's
    /// block map) and read its superblock.
    pub fn open(
        dev: &dyn BlockDevice,
        fs_block_size: u32,
        mut runs: Vec<Run>,
        fs_uuid: [u8; 16],
    ) -> Result<Journal> {
        let block_size =
            usize::try_from(fs_block_size).map_err(|_| Error::corrupt("journal block size"))?;
        if block_size < SUPERBLOCK_BYTES {
            return Err(Error::corrupt(
                "journal block size is smaller than its superblock",
            ));
        }
        if runs.is_empty() {
            return Err(Error::corrupt("journal has no mapped blocks"));
        }
        runs.sort_unstable_by_key(|run| run.logical);
        validate_runs(dev, &runs, fs_block_size)?;

        let superblock = read_mapped_block(dev, &runs, block_size, 0)?;
        if be32(&superblock, 0)? != MAGIC {
            return Err(Error::corrupt("journal superblock magic"));
        }
        let version = be32(&superblock, 4)?;
        if version != SUPERBLOCK_V1 && version != SUPERBLOCK_V2 {
            return Err(Error::corrupt("journal superblock type"));
        }
        if be32(&superblock, 12)? != fs_block_size {
            return Err(Error::corrupt("journal and filesystem block sizes differ"));
        }
        let maxlen = be32(&superblock, 16)?;
        let first = be32(&superblock, 20)?;
        let sequence = be32(&superblock, 24)?;
        let start = be32(&superblock, 28)?;
        if maxlen == 0 || u64::from(maxlen) > mapped_length(&runs)? {
            return Err(Error::corrupt("journal length exceeds inode mapping"));
        }
        ensure_mapped(&runs, maxlen)?;

        let (features, journal_uuid) = if version == SUPERBLOCK_V2 {
            let features = Features {
                compat: be32(&superblock, 36)?,
                incompat: be32(&superblock, 40)?,
            };
            if features.incompat & !KNOWN_INCOMPAT != 0 {
                return Err(Error::unsupported("unknown journal incompatible feature"));
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&superblock[48..64]);
            if uuid != fs_uuid {
                return Err(Error::corrupt("journal UUID differs from filesystem UUID"));
            }
            if features.has_block_checksum() {
                if superblock[80] != 4 {
                    return Err(Error::unsupported("journal checksum type"));
                }
                let expected = be32(&superblock, SUPER_CHECKSUM_OFFSET)?;
                let actual =
                    crc_with_zero(!0, &superblock[..SUPERBLOCK_BYTES], SUPER_CHECKSUM_OFFSET)?;
                if expected != actual {
                    return Err(Error::corrupt("journal superblock checksum"));
                }
            }
            (features, uuid)
        } else {
            (
                Features {
                    compat: 0,
                    incompat: 0,
                },
                fs_uuid,
            )
        };

        let fc_blocks = if features.has_fast_commit() {
            let count = be32(&superblock, 0x54)?;
            if count == 0 {
                256
            } else {
                count
            }
        } else {
            0
        };
        let log_end = maxlen
            .checked_sub(fc_blocks)
            .ok_or_else(|| Error::corrupt("journal fast commit area"))?;
        if first == 0 || first >= log_end {
            return Err(Error::corrupt("journal first log block"));
        }
        if start != 0 && (start < first || start >= log_end) {
            return Err(Error::corrupt("journal start block"));
        }

        Ok(Journal {
            block_size,
            runs,
            first,
            log_end,
            sequence,
            start,
            features,
            checksum_seed: crc32c(!0, &journal_uuid),
            superblock,
        })
    }

    /// True when the journal superblock says transactions await replay.
    pub fn needs_recovery(&self) -> bool {
        self.start != 0
    }

    /// Require 64-bit journal block numbers before a filesystem with more
    /// than 2^32 blocks can use this journal for writes.
    pub fn require_64bit(&mut self, dev: &dyn BlockDevice) -> Result<()> {
        if self.features.has_64bit() {
            return Ok(());
        }
        if self.start != 0 {
            return Err(Error::unsupported(
                "cannot upgrade a journal with pending transactions to 64-bit block numbers",
            ));
        }
        if be32(&self.superblock, 4)? != SUPERBLOCK_V2 {
            return Err(Error::unsupported(
                "journal version does not support 64-bit block numbers",
            ));
        }
        self.features.incompat |= INCOMPAT_64BIT;
        put_be32(&mut self.superblock, 40, self.features.incompat)?;
        self.write_superblock(dev)?;
        dev.flush()?;
        Ok(())
    }

    /// Scan the log and compute the blocks to replay. Does not write.
    pub fn scan_recovery(&self, dev: &dyn BlockDevice) -> Result<Recovery> {
        if self.start == 0 {
            return Ok(Recovery {
                next_sequence: self.sequence,
                ..Recovery::default()
            });
        }
        if self.features.has_fast_commit() {
            return Err(Error::unsupported("fast commit replay"));
        }

        // First pass: identify complete, checksum-valid transactions. Only
        // tags, positions, and revokes are kept; data blocks are read just
        // to verify checksums, in bounded batches.
        let mut transactions = Vec::new();
        let mut position = self.start;
        let mut expected_sequence = self.sequence;
        let mut consumed = 0u64;
        let limit = u64::from(self.log_end - self.first);
        let mut current: Option<Transaction> = None;
        let read_data = self.features.has_v1_checksum() || self.features.has_block_checksum();
        let mut scratch = Vec::new();

        while consumed < limit {
            let block = self.read_block(dev, position)?;
            let magic = be32(&block, 0)?;
            let block_type = be32(&block, 4)?;
            let sequence = be32(&block, 8)?;
            if magic != MAGIC || sequence != expected_sequence {
                break;
            }
            consumed += 1;
            match block_type {
                DESCRIPTOR_BLOCK => {
                    let tags = match self.parse_descriptor(&block) {
                        Ok(tags) => tags,
                        Err(DescriptorError::Checksum) => break,
                        Err(DescriptorError::Corrupt(error)) => return Err(error),
                    };
                    let tx = current.get_or_insert_with(|| Transaction::new(sequence));
                    if tx.sequence != sequence {
                        return Err(Error::corrupt("journal descriptor transaction sequence"));
                    }
                    // Every tag owns the next log block, as in Linux
                    // `count_tags`. A descriptor whose blocks do not fit in
                    // the log cannot belong to a complete transaction.
                    let count = tags.len() as u64;
                    if count > limit - consumed {
                        break;
                    }
                    consumed += count;
                    if self.features.has_v1_checksum() {
                        tx.v1_checksum = crc32_be(tx.v1_checksum, &block);
                    }
                    position = self.advance(position);
                    let first_data = position;
                    for tag in &tags {
                        tx.writes.push((
                            tag.block,
                            LogCopy {
                                position,
                                escaped: tag.flags & TAG_ESCAPE != 0,
                            },
                        ));
                        position = self.advance(position);
                    }
                    if read_data
                        && !self.verify_logged_data(
                            dev,
                            sequence,
                            first_data,
                            &tags,
                            tx,
                            &mut scratch,
                        )?
                    {
                        // A damaged data block ends recovery before this
                        // transaction.
                        return Ok(self.recovery_from_transactions(transactions));
                    }
                    // The position is already the next control block.
                    continue;
                }
                REVOKE_BLOCK => {
                    let revokes = match self.parse_revoke(&block) {
                        Ok(revokes) => revokes,
                        Err(RevokeError::Checksum) => break,
                        Err(RevokeError::Corrupt(error)) => return Err(error),
                    };
                    let tx = current.get_or_insert_with(|| Transaction::new(sequence));
                    if tx.sequence != sequence {
                        return Err(Error::corrupt("journal revoke transaction sequence"));
                    }
                    tx.revokes.extend(revokes);
                }
                COMMIT_BLOCK => {
                    // A commit block alone is a valid, empty transaction.
                    let tx = current.take().unwrap_or_else(|| Transaction::new(sequence));
                    if !self.verify_commit(&block, &tx) {
                        break;
                    }
                    transactions.push(tx);
                    expected_sequence = expected_sequence.wrapping_add(1);
                }
                _ => break,
            }
            position = self.advance(position);
        }

        Ok(self.recovery_from_transactions(transactions))
    }

    /// Read the data blocks that follow a descriptor (starting at log block
    /// `first`) and check their tag checksums. Also folds them into the
    /// checksum-v1 CRC. Returns false when a tag checksum does not match.
    fn verify_logged_data(
        &self,
        dev: &dyn BlockDevice,
        sequence: u32,
        first: u32,
        tags: &[Tag],
        tx: &mut Transaction,
        scratch: &mut Vec<u8>,
    ) -> Result<bool> {
        let bs = self.block_size;
        let batch = (IO_BATCH_BYTES / bs).max(1);
        let mut position = first;
        let mut done = 0;
        while done < tags.len() {
            let (offset, n) = self.log_span(position, (tags.len() - done).min(batch))?;
            scratch.resize(n * bs, 0);
            dev.read_at(offset, scratch)?;
            for (data, tag) in scratch.chunks_exact(bs).zip(&tags[done..done + n]) {
                if !self.verify_data_checksum(sequence, data, tag.checksum) {
                    return Ok(false);
                }
                if self.features.has_v1_checksum() {
                    tx.v1_checksum = crc32_be(tx.v1_checksum, data);
                }
            }
            done += n;
            for _ in 0..n {
                position = self.advance(position);
            }
        }
        Ok(true)
    }

    /// Read the final copies listed in `recovery` and pass them to
    /// `f(first_home_block, data)` in ascending home-block order. `data`
    /// holds one or more whole blocks for consecutive home blocks, with
    /// escaped magic numbers restored. Contiguous log reads are coalesced
    /// and memory stays bounded by a fixed batch size.
    pub fn read_replay(
        &self,
        dev: &dyn BlockDevice,
        recovery: &Recovery,
        mut f: impl FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let bs = self.block_size;
        let batch = (IO_BATCH_BYTES / bs).max(1);
        let mut buf = vec![0u8; batch.min(recovery.blocks.len()) * bs];
        let mut entries = recovery.blocks.iter().map(|(home, copy)| (*home, *copy));
        let mut chunk: Vec<(u64, LogCopy)> = Vec::with_capacity(batch.min(recovery.blocks.len()));
        loop {
            chunk.clear();
            chunk.extend(entries.by_ref().take(batch));
            if chunk.is_empty() {
                return Ok(());
            }
            let buf = &mut buf[..chunk.len() * bs];
            let mut i = 0;
            while i < chunk.len() {
                let start = chunk[i].1.position;
                let (offset, span) = self.log_span(start, chunk.len() - i)?;
                let mut n = 1;
                while n < span && chunk[i + n].1.position == start + n as u32 {
                    n += 1;
                }
                dev.read_at(offset, &mut buf[i * bs..(i + n) * bs])?;
                i += n;
            }
            for (data, (_, copy)) in buf.chunks_exact_mut(bs).zip(&chunk) {
                if copy.escaped {
                    data[..4].copy_from_slice(&MAGIC.to_be_bytes());
                }
            }
            let mut i = 0;
            while i < chunk.len() {
                let home = chunk[i].0;
                let mut n = 1;
                while i + n < chunk.len() && chunk[i + n].0 == home + n as u64 {
                    n += 1;
                }
                f(home, &buf[i * bs..(i + n) * bs])?;
                i += n;
            }
        }
    }

    /// Read every block that `recovery` replays into memory. Intended for
    /// read-only mounts, which keep the replay set as an overlay, and tests.
    pub fn read_replay_blocks(
        &self,
        dev: &dyn BlockDevice,
        recovery: &Recovery,
    ) -> Result<BTreeMap<u64, Vec<u8>>> {
        let bs = self.block_size;
        let mut out = BTreeMap::new();
        self.read_replay(dev, recovery, |first, data| {
            for (i, block) in data.chunks_exact(bs).enumerate() {
                out.insert(first + i as u64, block.to_vec());
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Mark the journal empty after the caller wrote the replayed blocks.
    pub fn finish_recovery(&mut self, dev: &dyn BlockDevice, next_sequence: u32) -> Result<()> {
        self.start = 0;
        self.sequence = next_sequence;
        self.write_superblock(dev)?;
        dev.flush()?;
        Ok(())
    }

    /// Largest number of file-system blocks one `commit` call can accept.
    pub fn max_transaction_blocks(&self) -> usize {
        let tags = self.tags_per_descriptor();
        if tags == 0 {
            return 0;
        }
        let area = usize::try_from(self.log_end - self.first).unwrap_or(usize::MAX);
        if area < 3 {
            return 0;
        }
        // Keep one block free. It makes the bound valid even for journals
        // whose circular head and tail eventually meet.
        let available = area - 1;
        let mut data = available.saturating_sub(1) * tags / (tags + 1);
        while data > 0 && data + div_ceil(data, tags) + 1 > available {
            data -= 1;
        }
        data
    }

    /// Durably commit `blocks` as one transaction, then checkpoint them to
    /// their home locations and mark the journal empty again.
    pub fn commit(&mut self, dev: &dyn BlockDevice, blocks: &[(u64, &[u8])]) -> Result<()> {
        self.commit_inner(dev, blocks, true)
    }

    /// Write a committed transaction but deliberately leave checkpointing to
    /// recovery. This is intended for interoperability tests.
    #[doc(hidden)]
    pub fn commit_without_checkpoint(
        &mut self,
        dev: &dyn BlockDevice,
        blocks: &[(u64, &[u8])],
    ) -> Result<()> {
        self.commit_inner(dev, blocks, false)
    }

    fn commit_inner(
        &mut self,
        dev: &dyn BlockDevice,
        blocks: &[(u64, &[u8])],
        checkpoint: bool,
    ) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        if dev.read_only() {
            return Err(Error::ReadOnly);
        }
        if blocks.len() > self.max_transaction_blocks() {
            return Err(Error::NoSpace);
        }
        for (_, data) in blocks {
            if data.len() != self.block_size {
                return Err(Error::invalid("journalled block has the wrong size"));
            }
        }

        let sequence = self.sequence;
        self.start = self.first;
        self.stamp_superblock()?;

        // The superblock (log block 0) and the descriptor and data blocks
        // are written in as few device writes as possible. Their order does
        // not matter until the flush below.
        let bs = self.block_size;
        let v1 = self.features.has_v1_checksum();
        let tag_capacity = self.tags_per_descriptor();
        // Superblock, descriptors, and data blocks.
        let log_blocks = 1 + div_ceil(blocks.len(), tag_capacity) + blocks.len();
        let mut out = Coalescer::new(dev, log_blocks.saturating_mul(bs));
        out.push(self.log_offset(0)?, &self.superblock)?;
        let mut position = self.first;
        let mut v1_crc = !0u32;
        let mut descriptor = vec![0u8; bs];
        for chunk in blocks.chunks(tag_capacity) {
            descriptor.fill(0);
            put_be32(&mut descriptor, 0, MAGIC)?;
            put_be32(&mut descriptor, 4, DESCRIPTOR_BLOCK)?;
            put_be32(&mut descriptor, 8, sequence)?;
            let mut offset = HEADER_BYTES;
            for (index, (home, original)) in chunk.iter().enumerate() {
                let escaped = original[..4] == MAGIC.to_be_bytes();
                let mut flags = TAG_SAME_UUID;
                if escaped {
                    flags |= TAG_ESCAPE;
                }
                if index + 1 == chunk.len() {
                    flags |= TAG_LAST;
                }
                let checksum = if self.features.has_block_checksum() {
                    self.logged_data_checksum(sequence, original, escaped)
                } else {
                    0
                };
                offset = self.write_tag(&mut descriptor, offset, *home, flags, checksum)?;
            }
            if self.features.has_block_checksum() {
                let checksum = crc_with_zero(self.checksum_seed, &descriptor, bs - 4)?;
                put_be32(&mut descriptor, bs - 4, checksum)?;
            }
            if v1 {
                v1_crc = crc32_be(v1_crc, &descriptor);
            }
            out.push(self.log_offset(position)?, &descriptor)?;
            position = self.advance(position);

            for (_, original) in chunk {
                let stored = out.push(self.log_offset(position)?, original)?;
                if stored[..4] == MAGIC.to_be_bytes() {
                    stored[..4].fill(0);
                }
                if v1 {
                    v1_crc = crc32_be(v1_crc, stored);
                }
                position = self.advance(position);
            }
        }
        out.finish()?;
        drop(out);
        dev.flush()?;

        let mut commit = vec![0u8; bs];
        put_be32(&mut commit, 0, MAGIC)?;
        put_be32(&mut commit, 4, COMMIT_BLOCK)?;
        put_be32(&mut commit, 8, sequence)?;
        let (seconds, nanos) = journal_time();
        put_be64(&mut commit, 0x30, seconds)?;
        put_be32(&mut commit, 0x38, nanos)?;
        if self.features.has_block_checksum() {
            // Linux `jbd2_commit_block_csum_set` leaves the type and size
            // fields zero for checksum v2 and v3.
            let checksum = crc_with_zero(self.checksum_seed, &commit, COMMIT_CHECKSUM_OFFSET)?;
            put_be32(&mut commit, COMMIT_CHECKSUM_OFFSET, checksum)?;
        } else if v1 {
            commit[12] = CRC32_CHKSUM;
            commit[13] = CRC32_CHKSUM_SIZE;
            put_be32(&mut commit, COMMIT_CHECKSUM_OFFSET, v1_crc)?;
        }
        self.write_block(dev, position, &commit)?;
        dev.flush()?;

        if !checkpoint {
            return Ok(());
        }
        write_home_blocks(dev, bs, blocks)?;
        dev.flush()?;
        self.start = 0;
        self.sequence = sequence.wrapping_add(1);
        self.write_superblock(dev)?;
        dev.flush()?;
        Ok(())
    }

    fn recovery_from_transactions(&self, transactions: Vec<Transaction>) -> Recovery {
        // Second pass: keep the newest revoke of each block. A revoke that
        // reached the journal suppresses every write of that block in its
        // own and older transactions; Linux cancels superseded revokes
        // before writing them (`jbd2_journal_cancel_revoke`).
        let mut revoked = HashMap::<u64, u32>::new();
        for tx in &transactions {
            for &block in &tx.revokes {
                revoked
                    .entry(block)
                    .and_modify(|sequence| {
                        if tid_gt(tx.sequence, *sequence) {
                            *sequence = tx.sequence;
                        }
                    })
                    .or_insert(tx.sequence);
            }
        }

        // Third pass: replay in transaction order, as Linux
        // `jbd2_journal_test_revoke` does: a write survives only when its
        // transaction is newer than the block's revoke.
        let mut recovery = Recovery {
            blocks: BTreeMap::new(),
            transactions: u32::try_from(transactions.len()).unwrap_or(u32::MAX),
            next_sequence: self.sequence,
        };
        for tx in transactions {
            for (block, copy) in tx.writes {
                if revoked
                    .get(&block)
                    .is_none_or(|&sequence| tid_gt(tx.sequence, sequence))
                {
                    recovery.blocks.insert(block, copy);
                }
            }
            recovery.next_sequence = tx.sequence.wrapping_add(1);
        }
        recovery
    }

    fn parse_descriptor(&self, block: &[u8]) -> std::result::Result<Vec<Tag>, DescriptorError> {
        if self.features.has_block_checksum() {
            let expected = be32(block, self.block_size - 4).map_err(DescriptorError::Corrupt)?;
            let actual = crc_with_zero(self.checksum_seed, block, self.block_size - 4)
                .map_err(DescriptorError::Corrupt)?;
            if expected != actual {
                return Err(DescriptorError::Checksum);
            }
        }
        let limit = self.block_size
            - if self.features.has_block_checksum() {
                4
            } else {
                0
            };
        let mut offset = HEADER_BYTES;
        let mut tags = Vec::new();
        loop {
            let minimum = if self.features.has_v3_checksum() {
                16
            } else if self.features.has_64bit() {
                12
            } else {
                8
            };
            if offset.checked_add(minimum).is_none_or(|end| end > limit) {
                return Err(DescriptorError::Corrupt(Error::corrupt(
                    "journal descriptor tag",
                )));
            }
            let low = be32(block, offset).map_err(DescriptorError::Corrupt)?;
            let (flags, high, checksum, size) = if self.features.has_v3_checksum() {
                (
                    be32(block, offset + 4).map_err(DescriptorError::Corrupt)?,
                    be32(block, offset + 8).map_err(DescriptorError::Corrupt)?,
                    Some(be32(block, offset + 12).map_err(DescriptorError::Corrupt)?),
                    16,
                )
            } else {
                let flags = u32::from(be16(block, offset + 6).map_err(DescriptorError::Corrupt)?);
                let high = if self.features.has_64bit() {
                    be32(block, offset + 8).map_err(DescriptorError::Corrupt)?
                } else {
                    0
                };
                let checksum = if self.features.has_v2_checksum() {
                    Some(u32::from(
                        be16(block, offset + 4).map_err(DescriptorError::Corrupt)?,
                    ))
                } else {
                    None
                };
                (flags, high, checksum, minimum)
            };
            offset += size;
            if flags & TAG_SAME_UUID == 0 {
                let end = offset
                    .checked_add(16)
                    .ok_or_else(|| DescriptorError::Corrupt(Error::corrupt("journal tag UUID")))?;
                if end > limit {
                    return Err(DescriptorError::Corrupt(Error::corrupt("journal tag UUID")));
                }
                offset = end;
            }
            tags.push(Tag {
                block: u64::from(low) | (u64::from(high) << 32),
                flags,
                checksum,
            });
            if flags & TAG_LAST != 0 {
                return Ok(tags);
            }
        }
    }

    fn parse_revoke(&self, block: &[u8]) -> std::result::Result<Vec<u64>, RevokeError> {
        if self.features.has_block_checksum() {
            let expected = be32(block, self.block_size - 4).map_err(RevokeError::Corrupt)?;
            let actual = crc_with_zero(self.checksum_seed, block, self.block_size - 4)
                .map_err(RevokeError::Corrupt)?;
            if expected != actual {
                return Err(RevokeError::Checksum);
            }
        }
        let count = usize::try_from(be32(block, 12).map_err(RevokeError::Corrupt)?)
            .map_err(|_| RevokeError::Corrupt(Error::corrupt("journal revoke count")))?;
        let limit = self.block_size
            - if self.features.has_block_checksum() {
                4
            } else {
                0
            };
        if count < 16 || count > limit {
            return Err(RevokeError::Corrupt(Error::corrupt("journal revoke count")));
        }
        let record_size = if self.features.has_64bit() { 8 } else { 4 };
        if (count - 16) % record_size != 0 {
            return Err(RevokeError::Corrupt(Error::corrupt(
                "journal revoke record length",
            )));
        }
        let mut records = Vec::with_capacity((count - 16) / record_size);
        let mut offset = 16;
        while offset < count {
            let record = if record_size == 8 {
                be64(block, offset).map_err(RevokeError::Corrupt)?
            } else {
                u64::from(be32(block, offset).map_err(RevokeError::Corrupt)?)
            };
            records.push(record);
            offset += record_size;
        }
        Ok(records)
    }

    fn verify_data_checksum(&self, sequence: u32, data: &[u8], expected: Option<u32>) -> bool {
        match expected {
            None => true,
            Some(expected) => {
                let actual = self.data_checksum(sequence, data);
                if self.features.has_v3_checksum() {
                    actual == expected
                } else {
                    (actual as u16) == (expected as u16)
                }
            }
        }
    }

    fn data_checksum(&self, sequence: u32, data: &[u8]) -> u32 {
        let crc = crc32c(self.checksum_seed, &sequence.to_be_bytes());
        crc32c(crc, data)
    }

    /// Tag checksum of `original` as it is stored in the log, where an
    /// escaped block has its first four bytes zeroed. Avoids a copy.
    fn logged_data_checksum(&self, sequence: u32, original: &[u8], escaped: bool) -> u32 {
        if !escaped {
            return self.data_checksum(sequence, original);
        }
        let crc = crc32c(self.checksum_seed, &sequence.to_be_bytes());
        crc32c(crc32c(crc, &[0; 4]), &original[4..])
    }

    fn verify_commit(&self, block: &[u8], tx: &Transaction) -> bool {
        let Ok(stored) = be32(block, COMMIT_CHECKSUM_OFFSET) else {
            return false;
        };
        if self.features.has_block_checksum() {
            // Like Linux `jbd2_commit_block_csum_verify`, only the CRC32C
            // matters. Linux writes zero type and size fields here.
            return crc_with_zero(self.checksum_seed, block, COMMIT_CHECKSUM_OFFSET)
                .is_ok_and(|actual| actual == stored);
        }
        if self.features.has_v1_checksum() {
            // Linux `do_one_pass` accepts a matching CRC32, or a commit
            // block that carries no checksum at all.
            let (kind, size) = (block[12], block[13]);
            return (kind == CRC32_CHKSUM && size == CRC32_CHKSUM_SIZE && stored == tx.v1_checksum)
                || (kind == 0 && size == 0 && stored == 0);
        }
        true
    }

    fn write_tag(
        &self,
        descriptor: &mut [u8],
        offset: usize,
        block: u64,
        flags: u32,
        checksum: u32,
    ) -> Result<usize> {
        if self.features.has_v3_checksum() {
            put_be32(descriptor, offset, block as u32)?;
            put_be32(descriptor, offset + 4, flags)?;
            put_be32(descriptor, offset + 8, (block >> 32) as u32)?;
            put_be32(descriptor, offset + 12, checksum)?;
            Ok(offset + 16)
        } else {
            put_be32(descriptor, offset, block as u32)?;
            if self.features.has_v2_checksum() {
                put_be16(descriptor, offset + 4, checksum as u16)?;
            }
            put_be16(descriptor, offset + 6, flags as u16)?;
            let end = if self.features.has_64bit() {
                put_be32(descriptor, offset + 8, (block >> 32) as u32)?;
                offset + 12
            } else {
                offset + 8
            };
            Ok(end)
        }
    }

    fn tags_per_descriptor(&self) -> usize {
        let tag_size = if self.features.has_v3_checksum() {
            16
        } else if self.features.has_64bit() {
            12
        } else {
            8
        };
        let tail = if self.features.has_block_checksum() {
            4
        } else {
            0
        };
        self.block_size.saturating_sub(HEADER_BYTES + tail) / tag_size
    }

    fn advance(&self, block: u32) -> u32 {
        let next = block + 1;
        if next == self.log_end {
            self.first
        } else {
            next
        }
    }

    fn read_block(&self, dev: &dyn BlockDevice, logical: u32) -> Result<Vec<u8>> {
        read_mapped_block(dev, &self.runs, self.block_size, u64::from(logical))
    }

    fn write_block(&self, dev: &dyn BlockDevice, logical: u32, block: &[u8]) -> Result<()> {
        write_mapped_block(dev, &self.runs, self.block_size, u64::from(logical), block)
    }

    /// Device byte offset of journal logical block `logical`.
    fn log_offset(&self, logical: u32) -> Result<u64> {
        mapped_offset(&self.runs, self.block_size, u64::from(logical))
    }

    /// Device byte offset of log block `position` and how many blocks, up
    /// to `max` (at least one), follow it contiguously on the device
    /// without wrapping around the end of the log.
    fn log_span(&self, position: u32, max: usize) -> Result<(u64, usize)> {
        let logical = u64::from(position);
        let run = self
            .runs
            .iter()
            .find(|run| logical >= run.logical && logical - run.logical < run.len)
            .ok_or_else(|| Error::corrupt("journal block outside inode mapping"))?;
        let in_run = run.logical + run.len - logical;
        let to_end = u64::from(self.log_end.saturating_sub(position)).max(1);
        let n = in_run.min(to_end).min(max.max(1) as u64) as usize;
        Ok((self.log_offset(position)?, n))
    }

    /// Store the in-memory sequence and start in the superblock buffer and
    /// refresh its checksum.
    fn stamp_superblock(&mut self) -> Result<()> {
        put_be32(&mut self.superblock, 24, self.sequence)?;
        put_be32(&mut self.superblock, 28, self.start)?;
        if self.features.has_block_checksum() {
            put_be32(&mut self.superblock, SUPER_CHECKSUM_OFFSET, 0)?;
            let checksum = crc_with_zero(
                !0,
                &self.superblock[..SUPERBLOCK_BYTES],
                SUPER_CHECKSUM_OFFSET,
            )?;
            put_be32(&mut self.superblock, SUPER_CHECKSUM_OFFSET, checksum)?;
        }
        Ok(())
    }

    fn write_superblock(&mut self, dev: &dyn BlockDevice) -> Result<()> {
        self.stamp_superblock()?;
        self.write_block(dev, 0, &self.superblock)
    }
}

/// Gathers block writes to consecutive device offsets into one write, up
/// to [`IO_BATCH_BYTES`].
struct Coalescer<'a> {
    dev: &'a dyn BlockDevice,
    offset: u64,
    buf: Vec<u8>,
}

impl<'a> Coalescer<'a> {
    /// `expected` is the number of bytes the caller plans to queue; the
    /// buffer never grows past [`IO_BATCH_BYTES`].
    fn new(dev: &'a dyn BlockDevice, expected: usize) -> Self {
        Coalescer {
            dev,
            offset: 0,
            buf: Vec::with_capacity(expected.min(IO_BATCH_BYTES)),
        }
    }

    /// Queue `data` for device offset `offset` and return the queued copy,
    /// which the caller may still modify before it is written.
    fn push(&mut self, offset: u64, data: &[u8]) -> Result<&mut [u8]> {
        if !self.buf.is_empty()
            && (self.offset + self.buf.len() as u64 != offset
                || self.buf.len() + data.len() > IO_BATCH_BYTES)
        {
            self.finish()?;
        }
        if self.buf.is_empty() {
            self.offset = offset;
        }
        let start = self.buf.len();
        self.buf.extend_from_slice(data);
        Ok(&mut self.buf[start..])
    }

    /// Write everything queued.
    fn finish(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.dev.write_at(self.offset, &self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// Write `blocks` to their home locations. Blocks with consecutive home
/// numbers go out in one device write.
pub(crate) fn write_home_blocks(
    dev: &dyn BlockDevice,
    block_size: usize,
    blocks: &[(u64, &[u8])],
) -> Result<()> {
    let bs = u64::try_from(block_size).map_err(|_| Error::invalid("filesystem block size"))?;
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by_key(|&i| blocks[i].0);
    let batch = (IO_BATCH_BYTES / block_size).max(1);
    let mut scratch = Vec::new();
    let mut i = 0;
    while i < order.len() {
        let (home, first) = blocks[order[i]];
        let mut n = 1;
        while n < batch
            && i + n < order.len()
            && Some(blocks[order[i + n]].0) == home.checked_add(n as u64)
        {
            n += 1;
        }
        let offset = home.checked_mul(bs).ok_or(Error::Range)?;
        if n == 1 {
            dev.write_at(offset, first)?;
        } else {
            scratch.clear();
            for &index in &order[i..i + n] {
                scratch.extend_from_slice(blocks[index].1);
            }
            dev.write_at(offset, &scratch)?;
        }
        i += n;
    }
    Ok(())
}

enum DescriptorError {
    Checksum,
    Corrupt(Error),
}

enum RevokeError {
    Checksum,
    Corrupt(Error),
}

fn validate_runs(dev: &dyn BlockDevice, runs: &[Run], block_size: u32) -> Result<()> {
    let mut end = 0u64;
    for run in runs {
        if run.len == 0 || run.logical < end {
            return Err(Error::corrupt("journal block runs"));
        }
        end = run
            .logical
            .checked_add(run.len)
            .ok_or_else(|| Error::corrupt("journal run length"))?;
        let physical_end = run
            .physical
            .checked_add(run.len)
            .ok_or_else(|| Error::corrupt("journal run location"))?;
        let byte_end = physical_end
            .checked_mul(u64::from(block_size))
            .ok_or_else(|| Error::corrupt("journal run byte location"))?;
        if byte_end > dev.size() {
            return Err(Error::corrupt("journal run exceeds device"));
        }
    }
    Ok(())
}

fn mapped_length(runs: &[Run]) -> Result<u64> {
    let last = runs
        .last()
        .ok_or_else(|| Error::corrupt("journal has no block runs"))?;
    last.logical
        .checked_add(last.len)
        .ok_or_else(|| Error::corrupt("journal run length"))
}

fn ensure_mapped(runs: &[Run], blocks: u32) -> Result<()> {
    let mut expected = 0u64;
    for run in runs {
        if expected >= u64::from(blocks) {
            break;
        }
        if run.logical != expected {
            return Err(Error::corrupt("journal inode has a hole"));
        }
        expected = run
            .logical
            .checked_add(run.len)
            .ok_or_else(|| Error::corrupt("journal run length"))?;
    }
    if expected < u64::from(blocks) {
        return Err(Error::corrupt("journal inode has a hole"));
    }
    Ok(())
}

fn read_mapped_block(
    dev: &dyn BlockDevice,
    runs: &[Run],
    block_size: usize,
    logical: u64,
) -> Result<Vec<u8>> {
    let offset = mapped_offset(runs, block_size, logical)?;
    let mut block = vec![0u8; block_size];
    dev.read_at(offset, &mut block)?;
    Ok(block)
}

fn write_mapped_block(
    dev: &dyn BlockDevice,
    runs: &[Run],
    block_size: usize,
    logical: u64,
    block: &[u8],
) -> Result<()> {
    if block.len() != block_size {
        return Err(Error::invalid("journal block length"));
    }
    let offset = mapped_offset(runs, block_size, logical)?;
    dev.write_at(offset, block)?;
    Ok(())
}

fn mapped_offset(runs: &[Run], block_size: usize, logical: u64) -> Result<u64> {
    let run = runs
        .iter()
        .find(|run| logical >= run.logical && logical - run.logical < run.len)
        .ok_or_else(|| Error::corrupt("journal block outside inode mapping"))?;
    let physical = run
        .physical
        .checked_add(logical - run.logical)
        .ok_or_else(|| Error::corrupt("journal physical block"))?;
    physical
        .checked_mul(u64::try_from(block_size).map_err(|_| Error::corrupt("journal block size"))?)
        .ok_or_else(|| Error::corrupt("journal byte offset"))
}

fn crc_with_zero(seed: u32, bytes: &[u8], zero_offset: usize) -> Result<u32> {
    let zero_end = zero_offset
        .checked_add(4)
        .ok_or_else(|| Error::corrupt("journal checksum field"))?;
    if zero_end > bytes.len() {
        return Err(Error::corrupt("journal checksum field"));
    }
    let crc = crc32c(seed, &bytes[..zero_offset]);
    let crc = crc32c(crc, &[0; 4]);
    Ok(crc32c(crc, &bytes[zero_end..]))
}

fn be16(bytes: &[u8], offset: usize) -> Result<u16> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| Error::corrupt("journal field"))?;
    let field = bytes
        .get(offset..end)
        .ok_or_else(|| Error::corrupt("journal field"))?;
    Ok(u16::from_be_bytes([field[0], field[1]]))
}

fn be32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| Error::corrupt("journal field"))?;
    let field = bytes
        .get(offset..end)
        .ok_or_else(|| Error::corrupt("journal field"))?;
    Ok(u32::from_be_bytes([field[0], field[1], field[2], field[3]]))
}

fn be64(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| Error::corrupt("journal field"))?;
    let field = bytes
        .get(offset..end)
        .ok_or_else(|| Error::corrupt("journal field"))?;
    Ok(u64::from_be_bytes([
        field[0], field[1], field[2], field[3], field[4], field[5], field[6], field[7],
    ]))
}

fn put_be16(bytes: &mut [u8], offset: usize, value: u16) -> Result<()> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| Error::invalid("journal field"))?;
    let field = bytes
        .get_mut(offset..end)
        .ok_or_else(|| Error::invalid("journal field"))?;
    field.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_be32(bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| Error::invalid("journal field"))?;
    let field = bytes
        .get_mut(offset..end)
        .ok_or_else(|| Error::invalid("journal field"))?;
    field.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_be64(bytes: &mut [u8], offset: usize, value: u64) -> Result<()> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| Error::invalid("journal field"))?;
    let field = bytes
        .get_mut(offset..end)
        .ok_or_else(|| Error::invalid("journal field"))?;
    field.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn div_ceil(value: usize, divisor: usize) -> usize {
    value / divisor + usize::from(value % divisor != 0)
}

fn journal_time() -> (u64, u32) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs(), duration.subsec_nanos()),
        Err(_) => (0, 0),
    }
}
