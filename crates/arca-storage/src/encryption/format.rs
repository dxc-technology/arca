//! Encrypted blob file format constants and header I/O.
//!
//! On-disk format:
//! ```text
//! [4-byte magic: "AENC"]
//! [1-byte version: 0x01]
//! [4-byte chunk_size: LE u32]
//! [chunk 0: 4-byte LE plaintext_len | ciphertext | 16-byte GCM tag]
//! [chunk 1: ...]
//! ...
//! ```

/// Magic bytes identifying an encrypted blob.
pub const MAGIC: &[u8; 4] = b"AENC";

/// Current format version.
pub const VERSION: u8 = 0x01;

/// Default plaintext chunk size (64 KiB).
pub const DEFAULT_CHUNK_SIZE: u32 = 64 * 1024;

/// GCM authentication tag length in bytes.
pub const TAG_LEN: usize = 16;

/// File header size: 4 (magic) + 1 (version) + 4 (chunk_size).
pub const HEADER_SIZE: usize = 9;

/// On-disk size of a single encrypted chunk given its plaintext length.
/// 4 (plaintext_len LE) + ciphertext (same as plaintext) + 16 (tag).
pub fn on_disk_chunk_size(plaintext_len: u32) -> u64 {
    4 + plaintext_len as u64 + TAG_LEN as u64
}

/// On-disk size of a full chunk (DEFAULT_CHUNK_SIZE plaintext).
pub fn full_on_disk_chunk_size() -> u64 {
    on_disk_chunk_size(DEFAULT_CHUNK_SIZE)
}

/// Writes the encrypted blob header into a buffer.
pub fn write_header(chunk_size: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE);
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.extend_from_slice(&chunk_size.to_le_bytes());
    buf
}

/// Parses the encrypted blob header.
/// Returns `chunk_size` on success.
pub fn parse_header(data: &[u8]) -> Result<u32, &'static str> {
    if data.len() < HEADER_SIZE {
        return Err("header too short");
    }
    if &data[0..4] != MAGIC {
        return Err("invalid magic bytes");
    }
    if data[4] != VERSION {
        return Err("unsupported format version");
    }
    let chunk_size = u32::from_le_bytes([data[5], data[6], data[7], data[8]]);
    if chunk_size == 0 {
        return Err("chunk_size must be > 0");
    }
    Ok(chunk_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let buf = write_header(DEFAULT_CHUNK_SIZE);
        assert_eq!(buf.len(), HEADER_SIZE);
        let chunk_size = parse_header(&buf).unwrap();
        assert_eq!(chunk_size, DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn parse_header_invalid_magic() {
        let mut buf = write_header(DEFAULT_CHUNK_SIZE);
        buf[0] = b'X';
        assert!(parse_header(&buf).is_err());
    }

    #[test]
    fn parse_header_unsupported_version() {
        let mut buf = write_header(DEFAULT_CHUNK_SIZE);
        buf[4] = 0xFF;
        assert!(parse_header(&buf).is_err());
    }

    #[test]
    fn parse_header_too_short() {
        assert!(parse_header(&[0u8; 5]).is_err());
    }

    #[test]
    fn on_disk_chunk_size_calculation() {
        // 4 (len prefix) + 65536 (ciphertext) + 16 (tag) = 65556
        assert_eq!(full_on_disk_chunk_size(), 65556);
    }
}
