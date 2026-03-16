//! Streaming tar.gz archive download for the Admin API.
//!
//! `POST /admin/archive` accepts a JSON body with a bucket name and a list of
//! object keys. It returns a streaming `application/gzip` response containing a
//! tar archive of the requested objects, without creating any temporary files.
//!
//! The tar format is built manually (512-byte POSIX headers) and piped through
//! `async-compression` gzip for fully streaming output.

use std::io;
use std::pin::Pin;

use axum::extract::State;
use axum::response::Response;
use axum::Json;
use http::StatusCode;
use serde::Deserialize;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use crate::state::AppState;

use super::admin::AdminError;

// -- Request type --

#[derive(Deserialize)]
pub struct ArchiveRequest {
    pub bucket: String,
    pub keys: Vec<String>,
}

// -- Handler --

/// POST /admin/archive — stream a tar.gz archive of the requested objects.
pub async fn archive(
    State(state): State<AppState>,
    Json(body): Json<ArchiveRequest>,
) -> Result<Response, AdminError> {
    if body.keys.is_empty() {
        return Err(AdminError::bad_request("keys list must not be empty"));
    }

    // Verify the bucket exists.
    let bucket_exists = state
        .metadata
        .head_bucket(&body.bucket)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    if bucket_exists.is_none() {
        return Err(AdminError::not_found(format!(
            "Bucket '{}' not found",
            body.bucket
        )));
    }

    // Resolve all objects upfront so we can fail fast on missing keys.
    let mut entries = Vec::with_capacity(body.keys.len());
    for key in &body.keys {
        let record = state
            .metadata
            .get_object(&body.bucket, key)
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        match record {
            Some(r) => entries.push(r),
            None => {
                return Err(AdminError::not_found(format!(
                    "Object '{}' not found in bucket '{}'",
                    key, body.bucket
                )));
            }
        }
    }

    // Derive the download filename from the bucket name before moving data.
    let filename = format!("{}.tar.gz", sanitize_filename(&body.bucket));

    // Create a duplex stream: the writer side produces tar data, the reader
    // side is wrapped in gzip and streamed to the client.
    // 256 KiB buffer gives enough room for tar headers + data chunks without
    // blocking the writer too often.
    let (writer, reader) = tokio::io::duplex(256 * 1024);

    let blob_store = state.blob.clone();

    // Spawn a background task that writes the tar archive to the duplex writer.
    tokio::spawn(async move {
        if let Err(e) = write_tar(writer, blob_store, entries).await {
            tracing::error!(error = %e, "archive tar writer failed");
        }
    });

    // Wrap the reader in a gzip encoder.
    let gzip_reader = async_compression::tokio::bufread::GzipEncoder::new(
        tokio::io::BufReader::new(reader),
    );
    let stream = ReaderStream::new(gzip_reader);
    let resp_body = axum::body::Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/gzip")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(resp_body)
        .expect("build archive response"))
}

/// Writes a POSIX tar archive to the given writer.
async fn write_tar(
    mut writer: impl AsyncWrite + Unpin,
    blob_store: std::sync::Arc<dyn arca_core::store::BlobStore>,
    entries: Vec<arca_core::types::ObjectRecord>,
) -> io::Result<()> {
    for record in &entries {
        // Build tar header for this entry.
        let header = build_tar_header(&record.key, record.size)?;
        writer.write_all(&header).await?;

        // Stream the blob data.
        let get_result = blob_store
            .get(&record.blob_id, None)
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        let mut stream = std::pin::pin!(get_result.stream);
        let mut written: u64 = 0;

        loop {
            match poll_next(&mut stream).await {
                Some(Ok(chunk)) => {
                    writer.write_all(&chunk).await?;
                    written += chunk.len() as u64;
                }
                Some(Err(e)) => return Err(e),
                None => break,
            }
        }

        // Pad to 512-byte boundary.
        let remainder = (written % 512) as usize;
        if remainder > 0 {
            let padding = 512 - remainder;
            writer.write_all(&vec![0u8; padding]).await?;
        }
    }

    // End-of-archive marker: two 512-byte blocks of zeros.
    writer.write_all(&[0u8; 1024]).await?;
    writer.flush().await?;

    Ok(())
}

/// Polls the next item from a pinned stream.
async fn poll_next<S>(stream: &mut Pin<&mut S>) -> Option<S::Item>
where
    S: futures_core::Stream,
{
    std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await
}

/// Builds a 512-byte POSIX (UStar) tar header for a regular file.
fn build_tar_header(name: &str, size: u64) -> io::Result<[u8; 512]> {
    let mut header = [0u8; 512];

    // Name (bytes 0..100). If longer than 100 chars, use prefix field.
    let name_bytes = name.as_bytes();
    if name_bytes.len() <= 100 {
        header[..name_bytes.len()].copy_from_slice(name_bytes);
    } else if name_bytes.len() <= 255 {
        // Split into prefix (155 bytes max) + name (100 bytes max).
        // Find the last '/' within the prefix-eligible range.
        let search_end = name_bytes.len().min(155);
        let split_pos = name[..search_end].rfind('/').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("object key too long for tar header: {}", name),
            )
        })?;
        let (prefix, rest) = name_bytes.split_at(split_pos);
        let rest = &rest[1..]; // skip the '/'
        if rest.len() > 100 || prefix.len() > 155 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("object key too long for tar header: {}", name),
            ));
        }
        header[..rest.len()].copy_from_slice(rest);
        header[345..345 + prefix.len()].copy_from_slice(prefix);
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("object key too long for tar header: {}", name),
        ));
    }

    // Mode (bytes 100..108): 0644
    write_octal(&mut header[100..107], 0o644, 6);

    // UID (bytes 108..116): 0
    write_octal(&mut header[108..115], 0, 6);

    // GID (bytes 116..124): 0
    write_octal(&mut header[116..123], 0, 6);

    // Size (bytes 124..136): file size in octal
    write_octal(&mut header[124..135], size, 11);

    // Mtime (bytes 136..148): current time
    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    write_octal(&mut header[136..147], mtime, 11);

    // Typeflag (byte 156): '0' = regular file
    header[156] = b'0';

    // Magic (bytes 257..263): "ustar\0"
    header[257..263].copy_from_slice(b"ustar\0");

    // Version (bytes 263..265): "00"
    header[263..265].copy_from_slice(b"00");

    // Checksum (bytes 148..156): computed over header with checksum field as spaces.
    header[148..156].copy_from_slice(b"        ");
    let checksum: u32 = header.iter().map(|&b| b as u32).sum();
    write_octal(&mut header[148..154], checksum as u64, 6);
    header[154] = 0; // null terminator
    header[155] = b' '; // trailing space (traditional format)

    Ok(header)
}

/// Writes a value as zero-padded octal into the given slice.
fn write_octal(dest: &mut [u8], value: u64, width: usize) {
    let s = format!("{:0>width$o}", value, width = width);
    let bytes = s.as_bytes();
    let len = dest.len();
    if bytes.len() > len {
        // Truncate from the left if the octal string is wider than dest.
        let start = bytes.len() - len;
        dest.copy_from_slice(&bytes[start..]);
    } else {
        dest[..bytes.len()].copy_from_slice(bytes);
    }
}

/// Sanitizes a string for use as a filename (removes path separators, etc.).
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_header_size() {
        let header = build_tar_header("test.txt", 1024).unwrap();
        assert_eq!(header.len(), 512);
    }

    #[test]
    fn tar_header_name() {
        let header = build_tar_header("my/file.txt", 42).unwrap();
        let name = std::str::from_utf8(&header[..11]).unwrap();
        assert_eq!(name, "my/file.txt");
    }

    #[test]
    fn tar_header_magic() {
        let header = build_tar_header("test.txt", 0).unwrap();
        assert_eq!(&header[257..263], b"ustar\0");
    }

    #[test]
    fn tar_header_checksum_valid() {
        let header = build_tar_header("hello.txt", 5).unwrap();
        // Verify checksum: sum of all bytes with checksum field treated as spaces.
        let mut check_header = header;
        check_header[148..156].copy_from_slice(b"        ");
        let expected: u32 = check_header.iter().map(|&b| b as u32).sum();
        // Parse the stored checksum.
        let stored = std::str::from_utf8(&header[148..154]).unwrap().trim();
        let stored_val = u32::from_str_radix(stored, 8).unwrap();
        assert_eq!(stored_val, expected);
    }

    #[test]
    fn tar_header_long_name_with_prefix() {
        // Must be >100 chars to trigger prefix split. Build a path that's ~120 chars.
        let name = "very/long/directory/structure/that/definitely/needs/a/prefix/field/to/split/across/name/and/prefix/fields/in/tar/file.txt";
        assert!(name.len() > 100);
        let header = build_tar_header(name, 100).unwrap();
        // Verify the name is split across name and prefix fields.
        let stored_prefix = std::str::from_utf8(&header[345..345 + 155])
            .unwrap()
            .trim_end_matches('\0');
        let stored_name = std::str::from_utf8(&header[..100])
            .unwrap()
            .trim_end_matches('\0');
        let reconstructed = format!("{}/{}", stored_prefix, stored_name);
        assert_eq!(reconstructed, name);
    }

    #[test]
    fn tar_header_too_long() {
        let name = "a".repeat(256);
        assert!(build_tar_header(&name, 0).is_err());
    }

    #[test]
    fn sanitize_filename_basic() {
        assert_eq!(sanitize_filename("my-bucket"), "my-bucket");
        assert_eq!(sanitize_filename("has spaces!"), "has_spaces_");
        assert_eq!(sanitize_filename("path/to/thing"), "path_to_thing");
    }

    #[test]
    fn write_octal_basic() {
        let mut buf = [0u8; 8];
        write_octal(&mut buf[..7], 0o644, 6);
        assert_eq!(&buf[..6], b"000644");
    }
}
