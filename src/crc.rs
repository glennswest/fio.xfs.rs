//! CRC32C (Castagnoli), the checksum on every v5 metadata structure.

const fn table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0x82f6_3b78 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u32; 256] = table();

/// CRC32C of `data`, as XFS computes it.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// Whether the little-endian checksum stored at `off` in `buf` matches the
/// rest of it: XFS checksums a structure with its own checksum field zeroed.
pub fn verify(buf: &[u8], off: usize) -> bool {
    if buf.len() < off + 4 {
        return false;
    }
    let stored = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
    let mut c = !0u32;
    for (i, &b) in buf.iter().enumerate() {
        let b = if (off..off + 4).contains(&i) { 0 } else { b };
        c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c == stored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_value() {
        // The standard CRC-32C check value.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn verify_zeroes_its_own_field() {
        let mut buf = vec![7u8; 64];
        buf[8..12].fill(0);
        let c = crc32c(&buf);
        buf[8..12].copy_from_slice(&c.to_le_bytes());
        assert!(verify(&buf, 8));
        buf[20] ^= 1;
        assert!(!verify(&buf, 8));
    }
}
