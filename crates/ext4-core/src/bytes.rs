//! Little-endian field access over on-disk byte buffers.
//!
//! Every on-disk structure in this crate is a view over raw bytes. Unknown
//! fields are preserved because writes only touch the bytes of known fields.

#[inline]
pub fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
pub fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
pub fn le64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

#[inline]
pub fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
pub fn be64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_be_bytes(a)
}

#[inline]
pub fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn put_be32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

#[inline]
pub fn put_be64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

/// Combine a `lo` 32-bit field with a `hi` 32-bit field.
#[inline]
pub fn lohi(lo: u32, hi: u32) -> u64 {
    (lo as u64) | ((hi as u64) << 32)
}

/// Bitmap helpers (ext bitmaps are little-endian bit order inside bytes).
#[inline]
pub fn test_bit(b: &[u8], bit: usize) -> bool {
    b[bit >> 3] & (1 << (bit & 7)) != 0
}

#[inline]
pub fn set_bit(b: &mut [u8], bit: usize) {
    b[bit >> 3] |= 1 << (bit & 7);
}

#[inline]
pub fn clear_bit(b: &mut [u8], bit: usize) {
    b[bit >> 3] &= !(1 << (bit & 7));
}

/// Find the first zero bit in `b[start..limit)`.
pub fn find_zero_bit(b: &[u8], start: usize, limit: usize) -> Option<usize> {
    let mut i = start;
    while i < limit {
        if i & 7 == 0 && i + 8 <= limit && b[i >> 3] == 0xFF {
            i += 8;
            continue;
        }
        if !test_bit(b, i) {
            return Some(i);
        }
        i += 1;
    }
    None
}
