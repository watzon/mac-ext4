use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ext4_core::crc::{crc32_be, crc32c};
use ext4_core::journal::{Journal, Recovery, Run};
use ext4_core::{BlockDevice, MemDevice};

const BLOCK_SIZE: usize = 1024;
const MAGIC: u32 = 0xC03B_3998;
const CSUM_V2: u32 = 0x8;
const CSUM_V3: u32 = 0x10;
const FLAG_ESCAPE: u32 = 0x1;
const FLAG_SAME_UUID: u32 = 0x2;
const FLAG_LAST: u32 = 0x8;

struct Image {
    dev: MemDevice,
    uuid: [u8; 16],
    journal_physical: u64,
}

impl Image {
    fn new(maxlen: u32, first: u32, start: u32, sequence: u32, incompat: u32) -> Self {
        let dev = MemDevice::new(BLOCK_SIZE * 128);
        let uuid = *b"synthetic-jbd2!!";
        let journal_physical = 32;
        let mut superblock = vec![0u8; BLOCK_SIZE];
        put32(&mut superblock, 0, MAGIC);
        put32(&mut superblock, 4, 4);
        put32(&mut superblock, 12, BLOCK_SIZE as u32);
        put32(&mut superblock, 16, maxlen);
        put32(&mut superblock, 20, first);
        put32(&mut superblock, 24, sequence);
        put32(&mut superblock, 28, start);
        put32(&mut superblock, 40, incompat);
        superblock[48..64].copy_from_slice(&uuid);
        if incompat & (CSUM_V2 | CSUM_V3) != 0 {
            superblock[80] = 4;
            let checksum = crc_zero(!0, &superblock, 0xfc);
            put32(&mut superblock, 0xfc, checksum);
        }
        dev.write_at(journal_physical * BLOCK_SIZE as u64, &superblock)
            .unwrap();
        Self {
            dev,
            uuid,
            journal_physical,
        }
    }

    fn run(&self, maxlen: u32) -> Vec<Run> {
        vec![Run {
            logical: 0,
            physical: self.journal_physical,
            len: u64::from(maxlen),
        }]
    }

    fn write_journal(&self, logical: u32, block: &[u8]) {
        self.dev
            .write_at(
                (self.journal_physical + u64::from(logical)) * BLOCK_SIZE as u64,
                block,
            )
            .unwrap();
    }

    fn open(&self, maxlen: u32) -> Journal {
        Journal::open(&self.dev, BLOCK_SIZE as u32, self.run(maxlen), self.uuid).unwrap()
    }
}

fn put32(block: &mut [u8], offset: usize, value: u32) {
    block[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn put16(block: &mut [u8], offset: usize, value: u16) {
    block[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn crc_zero(seed: u32, bytes: &[u8], offset: usize) -> u32 {
    let crc = crc32c(seed, &bytes[..offset]);
    let crc = crc32c(crc, &[0; 4]);
    crc32c(crc, &bytes[offset + 4..])
}

fn descriptor(
    sequence: u32,
    block_number: u64,
    data: &[u8],
    flags: u32,
    incompat: u32,
    uuid: [u8; 16],
) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    put32(&mut block, 0, MAGIC);
    put32(&mut block, 4, 1);
    put32(&mut block, 8, sequence);
    let seed = crc32c(!0, &uuid);
    let data_crc = crc32c(crc32c(seed, &sequence.to_be_bytes()), data);
    put32(&mut block, 12, block_number as u32);
    if incompat & CSUM_V3 != 0 {
        put32(&mut block, 16, flags | FLAG_SAME_UUID | FLAG_LAST);
        put32(&mut block, 20, (block_number >> 32) as u32);
        put32(&mut block, 24, data_crc);
    } else {
        if incompat & CSUM_V2 != 0 {
            put16(&mut block, 16, data_crc as u16);
        }
        put16(&mut block, 18, (flags | FLAG_SAME_UUID | FLAG_LAST) as u16);
        let tail = if incompat & (CSUM_V2 | CSUM_V3) != 0 {
            4
        } else {
            0
        };
        if incompat & 0x2 != 0 {
            put32(&mut block, 20, (block_number >> 32) as u32);
        }
        if tail != 0 {
            let checksum = crc_zero(seed, &block, BLOCK_SIZE - tail);
            put32(&mut block, BLOCK_SIZE - tail, checksum);
        }
    }
    if incompat & CSUM_V3 != 0 {
        let checksum = crc_zero(seed, &block, BLOCK_SIZE - 4);
        put32(&mut block, BLOCK_SIZE - 4, checksum);
    }
    block
}

/// A commit block as Linux writes it: for checksum v2/v3,
/// `jbd2_commit_block_csum_set` leaves `h_chksum_type` and
/// `h_chksum_size` zero and stores the CRC32C of the whole block.
fn commit(sequence: u32, incompat: u32, uuid: [u8; 16]) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    put32(&mut block, 0, MAGIC);
    put32(&mut block, 4, 2);
    put32(&mut block, 8, sequence);
    if incompat & (CSUM_V2 | CSUM_V3) != 0 {
        let checksum = crc_zero(crc32c(!0, &uuid), &block, 16);
        put32(&mut block, 16, checksum);
    }
    block
}

fn revoke(sequence: u32, numbers: &[u64], incompat: u32, uuid: [u8; 16]) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    put32(&mut block, 0, MAGIC);
    put32(&mut block, 4, 5);
    put32(&mut block, 8, sequence);
    let record_size = if incompat & 0x2 != 0 { 8 } else { 4 };
    put32(&mut block, 12, 16 + (numbers.len() * record_size) as u32);
    for (index, number) in numbers.iter().enumerate() {
        let offset = 16 + index * record_size;
        if record_size == 8 {
            block[offset..offset + 8].copy_from_slice(&number.to_be_bytes());
        } else {
            put32(&mut block, offset, *number as u32);
        }
    }
    if incompat & (CSUM_V2 | CSUM_V3) != 0 {
        let seed = crc32c(!0, &uuid);
        let checksum = crc_zero(seed, &block, BLOCK_SIZE - 4);
        put32(&mut block, BLOCK_SIZE - 4, checksum);
    }
    block
}

#[test]
fn replays_v3_tag_and_restores_escaped_magic() {
    let image = Image::new(12, 1, 1, 10, CSUM_V3);
    let mut stored = vec![0xA5; BLOCK_SIZE];
    stored[..4].fill(0);
    image.write_journal(
        1,
        &descriptor(10, 5, &stored, FLAG_ESCAPE, CSUM_V3, image.uuid),
    );
    image.write_journal(2, &stored);
    image.write_journal(3, &commit(10, CSUM_V3, image.uuid));

    let journal = image.open(12);
    let recovery = journal.scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 1);
    assert_eq!(recovery.next_sequence, 11);
    assert_eq!(
        &replayed(&journal, &image.dev, &recovery)[&5][..4],
        &MAGIC.to_be_bytes()
    );
}

#[test]
fn rejects_checksum_failing_final_transaction() {
    let image = Image::new(12, 1, 1, 10, CSUM_V3);
    let data = vec![0x42; BLOCK_SIZE];
    image.write_journal(1, &descriptor(10, 5, &data, 0, CSUM_V3, image.uuid));
    image.write_journal(2, &data);
    let mut bad_commit = commit(10, CSUM_V3, image.uuid);
    bad_commit[100] ^= 1;
    image.write_journal(3, &bad_commit);

    let recovery = image.open(12).scan_recovery(&image.dev).unwrap();
    assert!(recovery.blocks.is_empty());
    assert_eq!(recovery.transactions, 0);
    assert_eq!(recovery.next_sequence, 10);
}

#[test]
fn revocation_skips_older_write_but_later_write_wins() {
    let image = Image::new(16, 1, 1, 10, CSUM_V3);
    let first = vec![0x11; BLOCK_SIZE];
    let second = vec![0x22; BLOCK_SIZE];
    let third = vec![0x33; BLOCK_SIZE];
    image.write_journal(1, &descriptor(10, 5, &first, 0, CSUM_V3, image.uuid));
    image.write_journal(2, &first);
    image.write_journal(3, &commit(10, CSUM_V3, image.uuid));
    image.write_journal(4, &revoke(11, &[5], CSUM_V3, image.uuid));
    image.write_journal(5, &descriptor(11, 6, &second, 0, CSUM_V3, image.uuid));
    image.write_journal(6, &second);
    image.write_journal(7, &commit(11, CSUM_V3, image.uuid));
    image.write_journal(8, &descriptor(12, 5, &third, 0, CSUM_V3, image.uuid));
    image.write_journal(9, &third);
    image.write_journal(10, &commit(12, CSUM_V3, image.uuid));

    let journal = image.open(16);
    let recovery = journal.scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 3);
    assert_eq!(recovery.blocks.len(), 2);
    let blocks = replayed(&journal, &image.dev, &recovery);
    assert_eq!(blocks[&5], third);
    assert_eq!(blocks[&6], second);
}

#[test]
fn follows_transactions_across_log_wrap() {
    let image = Image::new(8, 1, 6, 10, CSUM_V3);
    let data = vec![0x7C; BLOCK_SIZE];
    image.write_journal(6, &descriptor(10, 9, &data, 0, CSUM_V3, image.uuid));
    image.write_journal(7, &data);
    image.write_journal(1, &commit(10, CSUM_V3, image.uuid));

    let journal = image.open(8);
    let recovery = journal.scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 1);
    assert_eq!(replayed(&journal, &image.dev, &recovery)[&9], data);
}

#[test]
fn parses_classic_v2_64bit_tags() {
    let incompat = CSUM_V2 | 0x2;
    let image = Image::new(12, 1, 1, 10, incompat);
    let data = vec![0x3E; BLOCK_SIZE];
    let destination = (1u64 << 32) | 7;
    image.write_journal(
        1,
        &descriptor(10, destination, &data, 0, incompat, image.uuid),
    );
    image.write_journal(2, &data);
    image.write_journal(3, &commit(10, incompat, image.uuid));

    let journal = image.open(12);
    let recovery = journal.scan_recovery(&image.dev).unwrap();
    assert_eq!(
        replayed(&journal, &image.dev, &recovery)[&destination],
        data
    );
}

fn replayed(
    journal: &Journal,
    dev: &dyn BlockDevice,
    recovery: &Recovery,
) -> BTreeMap<u64, Vec<u8>> {
    journal.read_replay_blocks(dev, recovery).unwrap()
}

/// Write a one-block transaction `sequence` at log block `at`; returns the
/// next free log block.
fn write_tx(image: &Image, at: u32, sequence: u32, home: u64, data: &[u8], incompat: u32) -> u32 {
    image.write_journal(
        at,
        &descriptor(sequence, home, data, 0, incompat, image.uuid),
    );
    image.write_journal(at + 1, data);
    image.write_journal(at + 2, &commit(sequence, incompat, image.uuid));
    at + 3
}

#[test]
fn accepts_linux_v2_and_v3_commit_headers_with_zero_type_and_size() {
    for incompat in [CSUM_V2, CSUM_V3, CSUM_V2 | 0x2, CSUM_V3 | 0x2] {
        let image = Image::new(12, 1, 1, 10, incompat);
        let data = vec![0x42; BLOCK_SIZE];
        write_tx(&image, 1, 10, 5, &data, incompat);
        let journal = image.open(12);
        let recovery = journal.scan_recovery(&image.dev).unwrap();
        assert_eq!(recovery.transactions, 1, "incompat={incompat:#x}");
        assert_eq!(replayed(&journal, &image.dev, &recovery)[&5], data);
    }
}

#[test]
fn accepts_commit_headers_from_older_writers_with_type_and_size_four() {
    let image = Image::new(12, 1, 1, 10, CSUM_V3);
    let data = vec![0x42; BLOCK_SIZE];
    write_tx(&image, 1, 10, 5, &data, CSUM_V3);
    let mut block = commit(10, CSUM_V3, image.uuid);
    block[12] = 4;
    block[13] = 4;
    let checksum = crc_zero(crc32c(!0, &image.uuid), &block, 16);
    put32(&mut block, 16, checksum);
    image.write_journal(3, &block);
    assert_eq!(
        image
            .open(12)
            .scan_recovery(&image.dev)
            .unwrap()
            .transactions,
        1
    );
}

#[test]
fn same_transaction_revoke_suppresses_the_write() {
    let image = Image::new(12, 1, 1, 10, CSUM_V3);
    let data = vec![0x42; BLOCK_SIZE];
    image.write_journal(1, &descriptor(10, 5, &data, 0, CSUM_V3, image.uuid));
    image.write_journal(2, &data);
    image.write_journal(3, &revoke(10, &[5], CSUM_V3, image.uuid));
    image.write_journal(4, &commit(10, CSUM_V3, image.uuid));
    let recovery = image.open(12).scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 1);
    assert!(!recovery.blocks.contains_key(&5));
}

#[test]
fn revoke_after_sequence_wrap_suppresses_older_write() {
    let image = Image::new(12, 1, 1, u32::MAX, CSUM_V3);
    let data = vec![0x42; BLOCK_SIZE];
    let next = write_tx(&image, 1, u32::MAX, 5, &data, CSUM_V3);
    image.write_journal(next, &revoke(0, &[5], CSUM_V3, image.uuid));
    image.write_journal(next + 1, &commit(0, CSUM_V3, image.uuid));
    let recovery = image.open(12).scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 2);
    assert_eq!(recovery.next_sequence, 1);
    assert!(!recovery.blocks.contains_key(&5));
}

#[test]
fn write_after_sequence_wrap_survives_older_revoke() {
    let image = Image::new(12, 1, 1, u32::MAX, CSUM_V3);
    let data = vec![0x42; BLOCK_SIZE];
    image.write_journal(1, &revoke(u32::MAX, &[5], CSUM_V3, image.uuid));
    image.write_journal(2, &commit(u32::MAX, CSUM_V3, image.uuid));
    write_tx(&image, 3, 0, 5, &data, CSUM_V3);
    let journal = image.open(12);
    let recovery = journal.scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 2);
    assert_eq!(replayed(&journal, &image.dev, &recovery)[&5], data);
}

#[test]
fn commit_block_alone_is_an_empty_transaction() {
    let image = Image::new(12, 1, 1, 10, CSUM_V3);
    let data = vec![0x42; BLOCK_SIZE];
    image.write_journal(1, &commit(10, CSUM_V3, image.uuid));
    write_tx(&image, 2, 11, 5, &data, CSUM_V3);
    let recovery = image.open(12).scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 2);
    assert_eq!(recovery.next_sequence, 12);
    assert!(recovery.blocks.contains_key(&5));
}

/// Image with `JBD2_FEATURE_COMPAT_CHECKSUM` (checksum v1) set.
fn v1_image() -> Image {
    let image = Image::new(12, 1, 1, 10, 0);
    let mut sb = vec![0; BLOCK_SIZE];
    image
        .dev
        .read_at(image.journal_physical * BLOCK_SIZE as u64, &mut sb)
        .unwrap();
    put32(&mut sb, 36, 1);
    image.write_journal(0, &sb);
    image
}

#[test]
fn v1_checksum_covers_descriptor_then_data_blocks() {
    let image = v1_image();
    let data = vec![0x42; BLOCK_SIZE];
    let desc = descriptor(10, 5, &data, 0, 0, image.uuid);
    image.write_journal(1, &desc);
    image.write_journal(2, &data);
    // Linux `calc_chksums`: CRC32 (big endian) over the descriptor block,
    // then each data block, in log order.
    let mut block = commit(10, 0, image.uuid);
    block[12] = 1;
    block[13] = 4;
    put32(&mut block, 16, crc32_be(crc32_be(!0, &desc), &data));
    image.write_journal(3, &block);
    let journal = image.open(12);
    let recovery = journal.scan_recovery(&image.dev).unwrap();
    assert_eq!(recovery.transactions, 1);
    assert_eq!(replayed(&journal, &image.dev, &recovery)[&5], data);

    // The old data-only checksum must not be accepted.
    put32(&mut block, 16, crc32_be(!0, &data));
    image.write_journal(3, &block);
    assert_eq!(
        image
            .open(12)
            .scan_recovery(&image.dev)
            .unwrap()
            .transactions,
        0
    );
}

#[test]
fn v1_commit_without_checksum_is_accepted() {
    let image = v1_image();
    let data = vec![0x42; BLOCK_SIZE];
    write_tx(&image, 1, 10, 5, &data, 0);
    assert_eq!(
        image
            .open(12)
            .scan_recovery(&image.dev)
            .unwrap()
            .transactions,
        1
    );
}

struct CountingDevice {
    inner: MemDevice,
    reads: AtomicU64,
}

impl BlockDevice for CountingDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

/// Descriptor without checksums whose tags name `homes`, in order.
fn plain_descriptor(sequence: u32, homes: &[u64]) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    put32(&mut block, 0, MAGIC);
    put32(&mut block, 4, 1);
    put32(&mut block, 8, sequence);
    for (i, home) in homes.iter().enumerate() {
        let offset = 12 + i * 8;
        put32(&mut block, offset, *home as u32);
        let mut flags = FLAG_SAME_UUID;
        if i + 1 == homes.len() {
            flags |= FLAG_LAST;
        }
        put16(&mut block, offset + 6, flags as u16);
    }
    block
}

#[test]
fn scan_keeps_positions_and_replay_reads_only_the_final_copies() {
    let image = Image::new(24, 1, 1, 10, 0);
    let homes = [5u64, 6, 7, 8];
    let mut at = 1;
    for (i, sequence) in (10..13).enumerate() {
        image.write_journal(at, &plain_descriptor(sequence, &homes));
        for (k, _) in homes.iter().enumerate() {
            image.write_journal(at + 1 + k as u32, &vec![(i * 16 + k) as u8; BLOCK_SIZE]);
        }
        image.write_journal(at + 5, &commit(sequence, 0, image.uuid));
        at += 6;
    }
    let dev = CountingDevice {
        inner: MemDevice::from_vec(image.dev.snapshot()),
        reads: AtomicU64::new(0),
    };
    let journal = Journal::open(&dev, BLOCK_SIZE as u32, image.run(24), image.uuid).unwrap();
    dev.reads.store(0, Ordering::Relaxed);
    let recovery = journal.scan_recovery(&dev).unwrap();
    assert_eq!(recovery.transactions, 3);
    // Without checksums the scan reads only descriptor and commit blocks
    // (plus the block after the log end), never data payloads.
    assert_eq!(dev.reads.load(Ordering::Relaxed), 7);

    dev.reads.store(0, Ordering::Relaxed);
    let mut calls = Vec::new();
    journal
        .read_replay(&dev, &recovery, |first, data| {
            calls.push((first, data.to_vec()));
            Ok(())
        })
        .unwrap();
    // The four final copies are contiguous in the log: one read, delivered
    // as one run of consecutive home blocks.
    assert_eq!(dev.reads.load(Ordering::Relaxed), 1);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, 5);
    for k in 0..4 {
        assert!(calls[0].1[k * BLOCK_SIZE..(k + 1) * BLOCK_SIZE]
            .iter()
            .all(|&b| b == (32 + k) as u8));
    }
}
