//! SHA-256 verification helpers.

use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// SHA-256 of a local file, streamed in chunks.
pub async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}
