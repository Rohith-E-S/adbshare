//! SHA-256 verification helpers.

use std::path::Path;
use std::io::Read;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

pub async fn verify_checksum(path: &Path, expected_hex: &str) -> std::io::Result<bool> {
    let actual = sha256_file(path).await?;
    Ok(actual.eq_ignore_ascii_case(expected_hex))
}

/// SHA-256 of a local file, streamed in chunks.
pub async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn sha256_sync<R: Read>(mut reader: R) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}
