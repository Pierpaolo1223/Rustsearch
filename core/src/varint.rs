//! Unsigned LEB128, the only integer encoding in the postings.
//!
//! Term frequencies and the escape values of the block postings are small, so one
//! or two bytes per value is the normal case. The decoder is bounds-checked and
//! never shifts by more than 28 bits; a corrupt byte run stops the caller instead
//! of panicking.

/// The largest value a `u32` varint may encode.
pub const MAX_U32: u32 = u32::MAX;

/// Decode one `u32` from `bytes` starting at `*pos`, advancing the cursor.
///
/// Returns `None` on truncation, overflow or a ten-byte run. The caller decides
/// whether a missing value is a corrupt index (postings) or a clean end (a loop).
#[inline]
pub fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut value: u32 = 0;
    let mut shift: u32 = 0;
    for _ in 0..5 {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

/// Append one `u32` to `out` in the same encoding.
#[inline]
pub fn write_u32(out: &mut alloc::vec::Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Number of bytes [`write_u32`] would append.
#[inline]
pub fn encoded_len(mut value: u32) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn round_trips_the_boundaries() {
        for value in [0, 1, 127, 128, 16_383, 16_384, 2_097_151, u32::MAX] {
            let mut bytes = Vec::new();
            write_u32(&mut bytes, value);
            assert_eq!(bytes.len(), encoded_len(value));
            let mut pos = 0;
            assert_eq!(read_u32(&bytes, &mut pos), Some(value));
            assert_eq!(pos, bytes.len());
        }
    }

    #[test]
    fn truncation_is_none_not_a_panic() {
        for bytes in [&[][..], &[0x80][..], &[0x80, 0x80, 0x80, 0x80][..]] {
            let mut pos = 0;
            assert_eq!(read_u32(bytes, &mut pos), None);
        }
    }
}
