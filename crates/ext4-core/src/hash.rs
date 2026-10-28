//! Directory index (htree) name hashes: legacy, half-MD4, and TEA, each in
//! signed and unsigned `char` variants.

pub const LEGACY: u8 = 0;
pub const HALF_MD4: u8 = 1;
pub const TEA: u8 = 2;
pub const LEGACY_UNSIGNED: u8 = 3;
pub const HALF_MD4_UNSIGNED: u8 = 4;
pub const TEA_UNSIGNED: u8 = 5;
pub const SIPHASH: u8 = 6;

const DEFAULT_SEED: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
const EOF_32: u32 = 0x7FFF_FFFF;

/// `(major, minor)` hash of `name`. Returns `None` for unknown versions.
pub fn dirhash(version: u8, name: &[u8], seed: [u32; 4]) -> Option<(u32, u32)> {
    let mut buf = if seed.iter().any(|&w| w != 0) {
        seed
    } else {
        DEFAULT_SEED
    };
    let (hash, minor) = match version {
        LEGACY | LEGACY_UNSIGNED => (legacy(name, version == LEGACY_UNSIGNED), 0),
        HALF_MD4 | HALF_MD4_UNSIGNED => {
            let unsigned = version == HALF_MD4_UNSIGNED;
            let mut rest = name;
            loop {
                let mut input = [0u32; 8];
                str2hashbuf(rest, &mut input, unsigned);
                half_md4(&mut buf, &input);
                if rest.len() <= 32 {
                    break;
                }
                rest = &rest[32..];
            }
            (buf[1], buf[2])
        }
        TEA | TEA_UNSIGNED => {
            let unsigned = version == TEA_UNSIGNED;
            let mut rest = name;
            loop {
                let mut input = [0u32; 4];
                str2hashbuf(rest, &mut input, unsigned);
                tea(&mut buf, &input);
                if rest.len() <= 16 {
                    break;
                }
                rest = &rest[16..];
            }
            (buf[0], buf[1])
        }
        _ => return None,
    };
    let mut hash = hash & !1;
    if hash == EOF_32 << 1 {
        hash = (EOF_32 - 1) << 1;
    }
    Some((hash, minor))
}

fn char_value(b: u8, unsigned: bool) -> u32 {
    if unsigned {
        b as u32
    } else {
        b as i8 as i32 as u32
    }
}

/// Pack up to `out.len() * 4` name bytes into words, padding with a
/// length-derived pattern.
fn str2hashbuf(msg: &[u8], out: &mut [u32], unsigned: bool) {
    let len = msg.len() as u32;
    let mut pad = len | (len << 8);
    pad |= pad << 16;
    let take = msg.len().min(out.len() * 4);
    let mut val = pad;
    let mut idx = 0;
    for (i, &b) in msg[..take].iter().enumerate() {
        val = char_value(b, unsigned).wrapping_add(val << 8);
        if i % 4 == 3 {
            out[idx] = val;
            idx += 1;
            val = pad;
        }
    }
    if idx < out.len() {
        out[idx] = val;
        idx += 1;
    }
    for w in out.iter_mut().skip(idx) {
        *w = pad;
    }
}

fn legacy(name: &[u8], unsigned: bool) -> u32 {
    let (mut h0, mut h1): (u32, u32) = (0x12a3_fe2d, 0x37ab_e8f9);
    for &b in name {
        let mut h = h1.wrapping_add(h0 ^ char_value(b, unsigned).wrapping_mul(7_152_373));
        if h & 0x8000_0000 != 0 {
            h = h.wrapping_sub(0x7fff_ffff);
        }
        h1 = h0;
        h0 = h;
    }
    h0 << 1
}

fn half_md4(buf: &mut [u32; 4], x: &[u32; 8]) {
    const K2: u32 = 0x5A82_7999;
    const K3: u32 = 0x6ED9_EBA1;
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! round {
        ($fun:expr, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($fun($b, $c, $d)).wrapping_add($x);
            $a = $a.rotate_left($s);
        };
    }
    round!(f, a, b, c, d, x[0], 3);
    round!(f, d, a, b, c, x[1], 7);
    round!(f, c, d, a, b, x[2], 11);
    round!(f, b, c, d, a, x[3], 19);
    round!(f, a, b, c, d, x[4], 3);
    round!(f, d, a, b, c, x[5], 7);
    round!(f, c, d, a, b, x[6], 11);
    round!(f, b, c, d, a, x[7], 19);

    round!(g, a, b, c, d, x[1].wrapping_add(K2), 3);
    round!(g, d, a, b, c, x[3].wrapping_add(K2), 5);
    round!(g, c, d, a, b, x[5].wrapping_add(K2), 9);
    round!(g, b, c, d, a, x[7].wrapping_add(K2), 13);
    round!(g, a, b, c, d, x[0].wrapping_add(K2), 3);
    round!(g, d, a, b, c, x[2].wrapping_add(K2), 5);
    round!(g, c, d, a, b, x[4].wrapping_add(K2), 9);
    round!(g, b, c, d, a, x[6].wrapping_add(K2), 13);

    round!(h, a, b, c, d, x[3].wrapping_add(K3), 3);
    round!(h, d, a, b, c, x[7].wrapping_add(K3), 9);
    round!(h, c, d, a, b, x[2].wrapping_add(K3), 11);
    round!(h, b, c, d, a, x[6].wrapping_add(K3), 15);
    round!(h, a, b, c, d, x[1].wrapping_add(K3), 3);
    round!(h, d, a, b, c, x[5].wrapping_add(K3), 9);
    round!(h, c, d, a, b, x[0].wrapping_add(K3), 11);
    round!(h, b, c, d, a, x[4].wrapping_add(K3), 15);

    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

fn tea(buf: &mut [u32; 4], x: &[u32; 4]) {
    const DELTA: u32 = 0x9E37_79B9;
    let (mut sum, mut b0, mut b1) = (0u32, buf[0], buf[1]);
    let (a, b, c, d) = (x[0], x[1], x[2], x[3]);
    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(
            (b1 << 4).wrapping_add(a) ^ b1.wrapping_add(sum) ^ (b1 >> 5).wrapping_add(b),
        );
        b1 = b1.wrapping_add(
            (b0 << 4).wrapping_add(c) ^ b0.wrapping_add(sum) ^ (b0 >> 5).wrapping_add(d),
        );
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}
