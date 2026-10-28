//! Change the type GUID of one GPT partition entry in place.
//!
//! Only the type field of one entry changes. The partition GUID, name,
//! bounds, and attributes stay as they were. Both the primary and the backup
//! table are rewritten with fresh CRC32 values.

use std::io;

use ext4_core::BlockDevice;

/// GPT type GUID of a Linux file system data partition.
pub const LINUX_FILESYSTEM: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

const SIGNATURE: &[u8; 8] = b"EFI PART";

/// One parsed partition entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 1-based entry number (matches the `sN` suffix of macOS BSD names).
    pub index: u32,
    pub type_guid: String,
    pub first_lba: u64,
    pub last_lba: u64,
    pub name: String,
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Standard CRC-32 (IEEE 802.3, reflected, as used by UEFI).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Parse `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX` into on-disk (mixed-endian) bytes.
pub fn guid_to_bytes(text: &str) -> io::Result<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(err(format!("not a GUID: {text}")));
    }
    let mut raw = [0u8; 16];
    for (i, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| err("bad GUID"))?;
    }
    let mut out = raw;
    out[0..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
    out[4..6].copy_from_slice(&[raw[5], raw[4]]);
    out[6..8].copy_from_slice(&[raw[7], raw[6]]);
    Ok(out)
}

pub fn guid_to_string(b: &[u8]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// A GPT header and its entry array.
struct Table {
    lba: u64,
    header: Vec<u8>,
    entries: Vec<u8>,
    entries_lba: u64,
    entry_size: usize,
    count: usize,
}

fn read_table(dev: &dyn BlockDevice, sector: u64, lba: u64) -> io::Result<Table> {
    let mut sec = vec![0u8; sector as usize];
    dev.read_at(lba * sector, &mut sec)?;
    if &sec[0..8] != SIGNATURE {
        return Err(err(format!("no GPT header at sector {lba}")));
    }
    let header_size = le32(&sec, 12) as usize;
    if !(92..=sector as usize).contains(&header_size) {
        return Err(err("GPT header has an invalid size"));
    }
    let mut header = sec[..header_size].to_vec();
    let stored = le32(&header, 16);
    header[16..20].fill(0);
    if crc32(&header) != stored {
        return Err(err(format!(
            "GPT header at sector {lba} has a bad checksum"
        )));
    }
    header[16..20].copy_from_slice(&stored.to_le_bytes());
    if le64(&header, 24) != lba {
        return Err(err("GPT header does not describe its own location"));
    }
    let entries_lba = le64(&header, 72);
    let count = le32(&header, 80) as usize;
    let entry_size = le32(&header, 84) as usize;
    if entry_size < 128 || entry_size % 8 != 0 || count == 0 || count > 1024 {
        return Err(err("GPT entry array has an invalid shape"));
    }
    let len = (count * entry_size).div_ceil(sector as usize) * sector as usize;
    let mut entries = vec![0u8; len];
    dev.read_at(entries_lba * sector, &mut entries)?;
    if crc32(&entries[..count * entry_size]) != le32(&header, 88) {
        return Err(err(format!(
            "GPT entry array for header {lba} has a bad checksum"
        )));
    }
    Ok(Table {
        lba,
        header,
        entries,
        entries_lba,
        entry_size,
        count,
    })
}

/// Find the sector size by locating the GPT header.
fn detect_sector(dev: &dyn BlockDevice) -> io::Result<u64> {
    for sector in [dev.alignment().max(512) as u64, 512, 4096] {
        let mut sig = [0u8; 8];
        if dev.read_at(sector, &mut sig).is_ok() && &sig == SIGNATURE {
            return Ok(sector);
        }
    }
    Err(err("the disk has no GUID partition table"))
}

fn parse_entries(t: &Table) -> Vec<Entry> {
    (0..t.count)
        .filter_map(|i| {
            let e = &t.entries[i * t.entry_size..(i + 1) * t.entry_size];
            if e[..16].iter().all(|&b| b == 0) {
                return None;
            }
            let units: Vec<u16> = e[56..128]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            Some(Entry {
                index: i as u32 + 1,
                type_guid: guid_to_string(&e[..16]),
                first_lba: le64(e, 32),
                last_lba: le64(e, 40),
                name: String::from_utf16_lossy(&units),
            })
        })
        .collect()
}

/// List partitions and the sector size.
pub fn list(dev: &dyn BlockDevice) -> io::Result<(u64, Vec<Entry>)> {
    let sector = detect_sector(dev)?;
    let primary = read_table(dev, sector, 1)?;
    Ok((sector, parse_entries(&primary)))
}

fn write_table(dev: &dyn BlockDevice, sector: u64, t: &mut Table) -> io::Result<()> {
    let entries_crc = crc32(&t.entries[..t.count * t.entry_size]);
    t.header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    t.header[16..20].fill(0);
    let crc = crc32(&t.header);
    t.header[16..20].copy_from_slice(&crc.to_le_bytes());
    dev.write_at(t.entries_lba * sector, &t.entries)?;
    let mut sec = vec![0u8; sector as usize];
    dev.read_at(t.lba * sector, &mut sec)?;
    sec[..t.header.len()].copy_from_slice(&t.header);
    dev.write_at(t.lba * sector, &sec)
}

/// Set the type of partition entry `index` (1-based). If `expect_first_lba`
/// is given, the entry must start there (guards against index mix-ups).
/// Returns the entry as it was before the change.
pub fn set_type(
    dev: &dyn BlockDevice,
    index: u32,
    type_guid: &str,
    expect_first_lba: Option<u64>,
) -> io::Result<Entry> {
    let new_type = guid_to_bytes(type_guid)?;
    let sector = detect_sector(dev)?;
    let mut primary = read_table(dev, sector, 1)?;
    let backup_lba = le64(&primary.header, 32);
    let mut backup = read_table(dev, sector, backup_lba)?;
    if primary.entries[..primary.count * primary.entry_size]
        != backup.entries[..backup.count * backup.entry_size]
    {
        return Err(err(
            "the primary and backup GPT entry arrays differ; repair the table first",
        ));
    }
    let before = parse_entries(&primary)
        .into_iter()
        .find(|e| e.index == index)
        .ok_or_else(|| err(format!("partition {index} does not exist")))?;
    if let Some(first) = expect_first_lba {
        if before.first_lba != first {
            return Err(err(format!(
                "partition {index} starts at sector {}, not {first}; refusing to change it",
                before.first_lba
            )));
        }
    }
    let off = (index as usize - 1) * primary.entry_size;
    for t in [&mut backup, &mut primary] {
        t.entries[off..off + 16].copy_from_slice(&new_type);
    }
    // Backup first, and make it durable before the primary changes: one
    // copy is always valid if writing stops halfway.
    write_table(dev, sector, &mut backup)?;
    dev.flush()?;
    write_table(dev, sector, &mut primary)?;
    dev.flush()?;
    Ok(before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_round_trip_uses_mixed_endian_layout() {
        let b = guid_to_bytes(LINUX_FILESYSTEM).unwrap();
        assert_eq!(&b[..4], &[0xAF, 0x3D, 0xC6, 0x0F]);
        assert_eq!(guid_to_string(&b), LINUX_FILESYSTEM);
    }

    #[test]
    fn crc32_matches_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
