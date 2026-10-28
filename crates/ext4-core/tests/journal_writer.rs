use std::sync::atomic::{AtomicU64, Ordering};

use ext4_core::crc::{crc32_be, crc32c};
use ext4_core::journal::{Journal, Run};
use ext4_core::{BlockDevice, MemDevice};

const BLOCK_SIZE: usize = 1024;
const MAGIC: u32 = 0xC03B_3998;
const CSUM_V2: u32 = 0x8;
const CSUM_V3: u32 = 0x10;

fn put32(block: &mut [u8], offset: usize, value: u32) {
    block[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn crc_zero(seed: u32, bytes: &[u8], offset: usize) -> u32 {
    let crc = crc32c(seed, &bytes[..offset]);
    let crc = crc32c(crc, &[0; 4]);
    crc32c(crc, &bytes[offset + 4..])
}

fn new_journal(incompat: u32) -> (MemDevice, [u8; 16], Vec<Run>) {
    new_journal_with(0, incompat)
}

fn new_journal_with(compat: u32, incompat: u32) -> (MemDevice, [u8; 16], Vec<Run>) {
    let dev = MemDevice::new(BLOCK_SIZE * 128);
    let uuid = *b"writer-test-jbd2";
    let mut superblock = vec![0u8; BLOCK_SIZE];
    put32(&mut superblock, 0, MAGIC);
    put32(&mut superblock, 4, 4);
    put32(&mut superblock, 12, BLOCK_SIZE as u32);
    put32(&mut superblock, 16, 24);
    put32(&mut superblock, 20, 1);
    put32(&mut superblock, 24, 30);
    put32(&mut superblock, 36, compat);
    put32(&mut superblock, 40, incompat);
    superblock[48..64].copy_from_slice(&uuid);
    if incompat & (CSUM_V2 | CSUM_V3) != 0 {
        superblock[80] = 4;
        let checksum = crc_zero(!0, &superblock, 0xfc);
        put32(&mut superblock, 0xfc, checksum);
    }
    dev.write_at(32 * BLOCK_SIZE as u64, &superblock).unwrap();
    (
        dev,
        uuid,
        vec![Run {
            logical: 0,
            physical: 32,
            len: 24,
        }],
    )
}

#[test]
fn uncheckpointed_writer_transactions_are_recoverable_for_supported_checksum_formats() {
    for incompat in [0, CSUM_V2, CSUM_V3, CSUM_V2 | 0x2, CSUM_V3 | 0x2] {
        let (dev, uuid, runs) = new_journal(incompat);
        let mut journal = Journal::open(&dev, BLOCK_SIZE as u32, runs.clone(), uuid).unwrap();
        let mut escaped = vec![0x55; BLOCK_SIZE];
        escaped[..4].copy_from_slice(&MAGIC.to_be_bytes());
        let plain = vec![0xAB; BLOCK_SIZE];
        journal
            .commit_without_checkpoint(&dev, &[(4, escaped.as_slice()), (5, plain.as_slice())])
            .unwrap();

        let reopened = Journal::open(&dev, BLOCK_SIZE as u32, runs, uuid).unwrap();
        assert!(reopened.needs_recovery(), "incompat={incompat:#x}");
        let recovery = reopened.scan_recovery(&dev).unwrap();
        assert_eq!(recovery.transactions, 1, "incompat={incompat:#x}");
        assert_eq!(recovery.next_sequence, 31, "incompat={incompat:#x}");
        let blocks = reopened.read_replay_blocks(&dev, &recovery).unwrap();
        assert_eq!(blocks[&4], escaped, "incompat={incompat:#x}");
        assert_eq!(blocks[&5], plain, "incompat={incompat:#x}");
    }
}

#[test]
fn checkpointed_writer_copies_home_blocks_and_empties_the_journal() {
    let (dev, uuid, runs) = new_journal(CSUM_V3);
    let mut journal = Journal::open(&dev, BLOCK_SIZE as u32, runs.clone(), uuid).unwrap();
    let data = vec![0xCC; BLOCK_SIZE];
    journal.commit(&dev, &[(7, data.as_slice())]).unwrap();

    let mut home = vec![0u8; BLOCK_SIZE];
    dev.read_at(7 * BLOCK_SIZE as u64, &mut home).unwrap();
    assert_eq!(home, data);
    let reopened = Journal::open(&dev, BLOCK_SIZE as u32, runs, uuid).unwrap();
    assert!(!reopened.needs_recovery());
    assert_eq!(reopened.scan_recovery(&dev).unwrap().next_sequence, 31);
}

#[test]
fn enabling_64bit_tags_preserves_high_home_block_numbers() {
    let (dev, uuid, runs) = new_journal(CSUM_V3);
    let mut journal = Journal::open(&dev, BLOCK_SIZE as u32, runs.clone(), uuid).unwrap();
    journal.require_64bit(&dev).unwrap();
    let data = vec![0x9A; BLOCK_SIZE];
    let high_block = (1u64 << 32) | 17;
    journal
        .commit_without_checkpoint(&dev, &[(high_block, data.as_slice())])
        .unwrap();

    let reopened = Journal::open(&dev, BLOCK_SIZE as u32, runs, uuid).unwrap();
    let recovery = reopened.scan_recovery(&dev).unwrap();
    let blocks = reopened.read_replay_blocks(&dev, &recovery).unwrap();
    assert_eq!(blocks[&high_block], data);
}

fn log_block(dev: &MemDevice, logical: u64) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    dev.read_at((32 + logical) * BLOCK_SIZE as u64, &mut block)
        .unwrap();
    block
}

#[test]
fn v1_writer_checksums_descriptor_then_data_like_linux() {
    let (dev, uuid, runs) = new_journal_with(0x1, 0);
    let mut journal = Journal::open(&dev, BLOCK_SIZE as u32, runs.clone(), uuid).unwrap();
    let mut escaped = vec![0x55; BLOCK_SIZE];
    escaped[..4].copy_from_slice(&MAGIC.to_be_bytes());
    let plain = vec![0xAB; BLOCK_SIZE];
    journal
        .commit_without_checkpoint(&dev, &[(4, escaped.as_slice()), (5, plain.as_slice())])
        .unwrap();

    // Log layout: descriptor at 1, data at 2 and 3, commit at 4. The CRC
    // covers the blocks exactly as stored in the log (escaped copy).
    let (desc, d1, d2, commit) = (
        log_block(&dev, 1),
        log_block(&dev, 2),
        log_block(&dev, 3),
        log_block(&dev, 4),
    );
    assert_eq!(&d1[..4], &[0; 4]);
    let expected = crc32_be(crc32_be(crc32_be(!0, &desc), &d1), &d2);
    assert_eq!((commit[12], commit[13]), (1, 4));
    assert_eq!(
        u32::from_be_bytes(commit[16..20].try_into().unwrap()),
        expected
    );

    let reopened = Journal::open(&dev, BLOCK_SIZE as u32, runs, uuid).unwrap();
    let recovery = reopened.scan_recovery(&dev).unwrap();
    assert_eq!(recovery.transactions, 1);
    let blocks = reopened.read_replay_blocks(&dev, &recovery).unwrap();
    assert_eq!(blocks[&4], escaped);
    assert_eq!(blocks[&5], plain);
}

#[test]
fn v2_and_v3_writer_commit_headers_match_linux() {
    for incompat in [CSUM_V2, CSUM_V3] {
        let (dev, uuid, runs) = new_journal(incompat);
        let mut journal = Journal::open(&dev, BLOCK_SIZE as u32, runs, uuid).unwrap();
        let data = vec![0x11; BLOCK_SIZE];
        journal
            .commit_without_checkpoint(&dev, &[(4, data.as_slice())])
            .unwrap();
        let commit = log_block(&dev, 3);
        assert_eq!(u32::from_be_bytes(commit[4..8].try_into().unwrap()), 2);
        // `jbd2_commit_block_csum_set` writes zero type and size.
        assert_eq!((commit[12], commit[13]), (0, 0), "incompat={incompat:#x}");
        assert_eq!(
            u32::from_be_bytes(commit[16..20].try_into().unwrap()),
            crc_zero(crc32c(!0, &uuid), &commit, 16)
        );
    }
}

struct CountingDevice {
    inner: MemDevice,
    writes: AtomicU64,
    flushes: AtomicU64,
}

impl BlockDevice for CountingDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

#[test]
fn commit_coalesces_log_and_checkpoint_writes() {
    let (mem, uuid, runs) = new_journal(CSUM_V3);
    let dev = CountingDevice {
        inner: mem,
        writes: AtomicU64::new(0),
        flushes: AtomicU64::new(0),
    };
    let mut journal = Journal::open(&dev, BLOCK_SIZE as u32, runs.clone(), uuid).unwrap();
    let data: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i + 1; BLOCK_SIZE]).collect();
    let blocks: Vec<(u64, &[u8])> = data
        .iter()
        .enumerate()
        .map(|(i, d)| (90 + i as u64, d.as_slice()))
        .collect();
    journal.commit(&dev, &blocks).unwrap();
    // Superblock, descriptor and data in one write; then the commit block,
    // one checkpoint write for the eight adjacent home blocks, and the
    // superblock that empties the journal.
    assert_eq!(dev.writes.load(Ordering::Relaxed), 4);
    assert_eq!(dev.flushes.load(Ordering::Relaxed), 4);
    for (i, d) in data.iter().enumerate() {
        let mut home = vec![0u8; BLOCK_SIZE];
        dev.read_at((90 + i as u64) * BLOCK_SIZE as u64, &mut home)
            .unwrap();
        assert_eq!(&home, d);
    }
    let reopened = Journal::open(&dev, BLOCK_SIZE as u32, runs, uuid).unwrap();
    assert!(!reopened.needs_recovery());
}

#[test]
fn coalesced_log_writes_follow_a_fragmented_journal_mapping() {
    let (dev, uuid, _) = new_journal(CSUM_V3);
    // Move journal blocks 10.. to a second physical run.
    let runs = vec![
        Run {
            logical: 0,
            physical: 32,
            len: 10,
        },
        Run {
            logical: 10,
            physical: 80,
            len: 14,
        },
    ];
    let mut journal = Journal::open(&dev, BLOCK_SIZE as u32, runs.clone(), uuid).unwrap();
    let data: Vec<Vec<u8>> = (0..12u8).map(|i| vec![i + 1; BLOCK_SIZE]).collect();
    let blocks: Vec<(u64, &[u8])> = data
        .iter()
        .enumerate()
        .map(|(i, d)| (100 + i as u64, d.as_slice()))
        .collect();
    journal.commit_without_checkpoint(&dev, &blocks).unwrap();
    // Log block 10 must be at physical 80, not 42.
    let mut moved = vec![0u8; BLOCK_SIZE];
    dev.read_at(80 * BLOCK_SIZE as u64, &mut moved).unwrap();
    assert_eq!(moved, data[8]);

    let reopened = Journal::open(&dev, BLOCK_SIZE as u32, runs, uuid).unwrap();
    let recovery = reopened.scan_recovery(&dev).unwrap();
    assert_eq!(recovery.transactions, 1);
    let replay = reopened.read_replay_blocks(&dev, &recovery).unwrap();
    for (i, d) in data.iter().enumerate() {
        assert_eq!(&replay[&(100 + i as u64)], d);
    }
}
