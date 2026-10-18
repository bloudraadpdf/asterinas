// SPDX-License-Identifier: MPL-2.0

//! Checksums for device data and on-disk metadata.

const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = (value >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(value & 1));
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
};

/// Updates an uncomplemented CRC-32C value with the supplied bytes.
pub fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc = (crc >> 8) ^ CRC32C_TABLE[((crc ^ u32::from(*byte)) & 255) as usize];
    }
    crc
}

/// Updates an uncomplemented, most-significant-bit-first CRC-32 value.
pub fn crc32_be(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ (0x04c1_1db7 & 0u32.wrapping_sub(crc >> 31));
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::{crc32_be, crc32c};

    #[test]
    fn most_significant_bit_first_check_value() {
        assert_eq!(crc32_be(!0, b"123456789"), 0x0376_e6e7);
    }

    #[test]
    fn castagnoli_check_value() {
        assert_eq!(!crc32c(!0, b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(0x1234_5678, b""), 0x1234_5678);
    }

    #[test]
    fn incremental_update_matches_one_pass() {
        let first = crc32c(!0, b"1234");
        assert_eq!(crc32c(first, b"56789"), crc32c(!0, b"123456789"));
    }
}
