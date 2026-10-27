use anyhow::{Result, anyhow, bail};
use std::io::Read;
use std::path::Path;

pub mod manifest;

/// The name every APK keeps its manifest under.
pub const MANIFEST_NAME: &str = "AndroidManifest.xml";

/// Everything the pre-install card can show about one APK.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApkInfo {
    pub package: String,
    pub version_name: Option<String>,
    pub version_code: Option<u64>,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    pub label: Option<String>,
    pub permissions: Vec<String>,
    /// File size in bytes, straight from the filesystem.
    pub size: Option<u64>,
    /// CPU architectures with native libs, e.g. `arm64-v8a`.
    pub native_abis: Vec<String>,
}

/// True when a path names an installable APK, by name then by content magic.
///
/// The naive extension check wins for the common case, but a copy that lost or
/// never had its extension still routes correctly: the real check is the
/// `PK\x03\x04` + AndroidManifest membership, done on enough bytes to be
/// cheap.
pub fn is_apk(path: &Path) -> bool {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("apk"))
    {
        return true;
    }
    is_apk_magic(path)
}

/// The magic-number check behind [`is_apk`] for non-`.apk` files.
///
/// Two bounded reads, never the whole file: a dropped file can be a two
/// gigabyte video, and this runs on the UI thread for every path that lands
/// on the window. The first read rules out everything that is not a zip at
/// all, which is nearly every file; only a zip is worth a second look.
fn is_apk_magic(path: &Path) -> bool {
    let Ok(head) = read_head(path, 4096) else {
        return false;
    };
    if !head.starts_with(b"PK\x03\x04") {
        return false;
    }
    // Every APK built by aapt2 writes the manifest as the first entry, so the
    // local header usually names it outright.
    if local_header_name(&head).as_deref() == Some(MANIFEST_NAME) {
        return true;
    }
    // Otherwise the name is in the central directory, which lives at the end
    // of the file. The directory is plain text, so looking for the name in
    // the tail is enough.
    let Ok(tail) = read_tail(path, 1 << 20) else {
        return false;
    };
    tail.windows(MANIFEST_NAME.len())
        .any(|window| window == MANIFEST_NAME.as_bytes())
}

/// The name of the first entry in a zip, from its local file header.
fn local_header_name(head: &[u8]) -> Option<String> {
    if head.len() < 30 {
        return None;
    }
    let name_len = u16::from_le_bytes([head[26], head[27]]) as usize;
    let start: usize = 30;
    let end = start.checked_add(name_len)?;
    if end > head.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&head[start..end]).into_owned())
}

/// The first `n` bytes of a file.
fn read_head(path: &Path, n: usize) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let read = file.read(&mut buf[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// The last `n` bytes of a file.
fn read_tail(path: &Path, n: usize) -> Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(n as u64);
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (len - start) as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read the manifest of one APK and fold it into a full [`ApkInfo`].
pub fn apk_info(path: &Path) -> Result<ApkInfo> {
    let mut info = ApkInfo {
        size: std::fs::metadata(path).ok().map(|m| m.len()),
        ..Default::default()
    };
    // The APK is read once and the pieces are pulled out of that one copy.
    // Reading it three times over meant three hundred megabytes of I/O for
    // a hundred-megabyte game.
    let data = std::fs::read(path)?;
    let manifest_bytes = manifest::manifest_from(&data)?;
    let m = manifest::parse(&manifest_bytes)?;
    info.package = m.package;
    info.version_name = m.version_name;
    info.version_code = m.version_code;
    info.min_sdk = m.min_sdk;
    info.target_sdk = m.target_sdk;
    info.label = m.label;
    info.permissions = m.permissions;
    info.native_abis = manifest::native_abis_in(&data)?;
    if info.package.is_empty()
        && let Ok(arsc) = manifest::resources_arsc_from(&data)
        && let Some(name) = manifest::package_name_from_arsc(&arsc)
    {
        info.package = name;
    }
    Ok(info)
}

// ── Minimal ZIP reader ──────────────────────────────────────────────────────

/// One entry from a ZIP central directory. Only what APK walking needs: the
/// local header offset, the metadata lengths, and the compression method.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub name: String,
    method: u16,
    compressed_size: u64,
    local_offset: u64,
}

/// Read entries of a zip by finding its End Of Central Directory.
///
/// ZIP64 is oh-so-slightly supported: a >4GB APK is north of every store size
/// cap and missing from the routing path anyway, so the classic EOCD structure
/// is enough and failure means "not readable", not "wrong".
pub fn central_directory(data: &[u8]) -> Result<Vec<ZipEntry>> {
    // The EOCD is the last ~64 ka block, minimum 22 bytes.
    if data.len() < 22 {
        bail!("file too small to be a zip archive");
    }
    let scan = data.len().saturating_sub(22 + 65_536);
    // Scan backwards for the signature 0x06054b50.
    let mut eocd = None;
    let mut i = data.len() - 22;
    loop {
        if data[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]) {
            eocd = Some(i);
            break;
        }
        if i <= scan {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.ok_or_else(|| anyhow!("end of central directory not found"))?;
    let d = &data[eocd..];
    let count = u16::from_le_bytes([d[10], d[11]]) as usize;
    let cd_offset = u32::from_le_bytes([d[16], d[17], d[18], d[19]]) as usize;

    let mut entries = Vec::with_capacity(count.min(4096));
    let mut pos = cd_offset;
    for index in 0..count {
        // Every field is read through the checked cursor: a truncated or
        // hand-mangled archive must come back as an error, never as a panic.
        // A half-downloaded APK is exactly the shape that used to crash the
        // app when it was dropped on the window.
        let mut cur = Cursor::at(data, pos)?;
        if cur.u32()? != 0x02014b50 {
            bail!("central directory has a bad signature at entry {index}");
        }
        // The header after the signature: version made by, version needed,
        // flags, method, timestamps, crc, sizes, name, extra, comment, disk
        // numbers, attributes and the local header offset.
        cur.skip(6)?; // version made by, version needed, flags
        let method = cur.u16()?;
        cur.skip(4)?; // last-modified time and date
        let _crc = cur.u32()?;
        let compressed_size = cur.u32()? as u64;
        let _uncompressed_size = cur.u32()?;
        let name_len = cur.u16()? as usize;
        let extra_len = cur.u16()? as usize;
        let comment_len = cur.u16()? as usize;
        cur.skip(4)?; // disk number, internal attributes
        let _external_attributes = cur.u32()?;
        let local_offset = cur.u32()? as u64;
        let name = cur.string(name_len)?;
        entries.push(ZipEntry {
            name,
            method,
            compressed_size,
            local_offset,
        });
        pos = cur.pos + extra_len + comment_len;
    }
    Ok(entries)
}

/// A bounds-checked reader over a slice, for walking a binary structure.
///
/// Every read is checked, so a truncated archive produces an error instead of
/// an out-of-bounds panic. The zip reader is the one place in the app that
/// takes bytes straight from an untrusted file.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn at(data: &'a [u8], pos: usize) -> Result<Self> {
        if pos > data.len() {
            bail!("offset {pos} is past the end of the data");
        }
        Ok(Self { data, pos })
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| anyhow!("offset overflow"))?;
        if end > self.data.len() {
            bail!("the archive ends inside a field at offset {}", self.pos);
        }
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    /// A name of `n` bytes, decoded leniently: an archive with a mangled name
    /// is still worth listing, and the name is only ever shown to the user.
    fn string(&mut self, n: usize) -> Result<String> {
        let b = self.take(n)?;
        Ok(String::from_utf8_lossy(b).into_owned())
    }
}

/// Full bytes + entries of one small zip member, decompressed.
///
/// The whole file is mmap-like read in one pass: APKs are big, but the read is
/// sequential and the manifest walk happens once per interaction. The caller
/// has to be sure it starts near the header first.
pub fn extract(entry: &ZipEntry, data: &[u8]) -> Result<Vec<u8>> {
    let off = usize::try_from(entry.local_offset)
        .map_err(|_| anyhow!("local header offset does not fit in memory"))?;
    let mut cur = Cursor::at(data, off)?;
    let signature = cur.take(4)?;
    if signature != [0x50, 0x4b, 0x03, 0x04] {
        return Err(anyhow!("local header signature mismatch"));
    }
    cur.skip(22)?; // version, flags, method, time, date, crc
    let name_len = cur.u16()? as usize;
    let extra_len = cur.u16()? as usize;
    let data_start = cur.pos + name_len + extra_len;
    let data_end = data_start
        .checked_add(usize::try_from(entry.compressed_size).unwrap_or(usize::MAX))
        .ok_or_else(|| anyhow!("compressed size overflows"))?;
    if data_end > data.len() {
        return Err(anyhow!("compressed data beyond end of file"));
    }
    match entry.method {
        0 => Ok(data[data_start..data_end].to_vec()),
        8 => {
            let mut inflater = flate2::read::DeflateDecoder::new(&data[data_start..data_end]);
            let mut out = Vec::new();
            inflater.read_to_end(&mut out)?;
            Ok(out)
        }
        m => Err(anyhow!("unsupported compression method {m}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip whose central directory header is present but whose file name
    /// runs past the end of the data: what a half-downloaded APK looks like.
    ///
    /// The reader used to index the name without checking, and the app
    /// crashed when such a file was dropped on the window.
    fn truncated_zip() -> Vec<u8> {
        let mut data: Vec<u8> = Vec::new();
        // End of central directory, at offset 0.
        data.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(&[1, 0]); // entries on this disk
        data.extend_from_slice(&[1, 0]); // entries in total
        data.extend_from_slice(&[46, 0, 0, 0]); // directory size
        data.extend_from_slice(&[22, 0, 0, 0]); // directory offset
        data.extend_from_slice(&[0, 0]); // comment length
        // One central directory entry, at offset 22.
        data.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        data.extend_from_slice(&[0x14, 0x00, 0x0a, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&[0x08, 0x00]); // method: deflate
        data.extend_from_slice(&[0, 0, 0, 0]); // time and date
        data.extend_from_slice(&[0, 0, 0, 0]); // crc
        data.extend_from_slice(&[0, 0, 0, 0]); // compressed size
        data.extend_from_slice(&[0, 0, 0, 0]); // uncompressed size
        data.extend_from_slice(&[0xff, 0xff]); // name length: 65535
        data.extend_from_slice(&[0, 0]); // extra length
        data.extend_from_slice(&[0, 0]); // comment length
        data.extend_from_slice(&[0, 0, 0, 0]); // disk and internal attributes
        data.extend_from_slice(&[0, 0, 0, 0]); // external attributes
        data.extend_from_slice(&[0, 0, 0, 0]); // local header offset
        // No name bytes follow: the name runs off the end.
        data
    }

    #[test]
    fn a_truncated_archive_is_an_error_not_a_crash() {
        let data = truncated_zip();
        let outcome = std::panic::catch_unwind(|| central_directory(&data));
        assert!(
            outcome.is_ok(),
            "central_directory panicked on a truncated archive"
        );
        assert!(
            outcome.unwrap().is_err(),
            "a truncated archive is not readable"
        );
    }

    #[test]
    fn a_truncated_local_header_is_an_error_not_a_crash() {
        let data = truncated_zip();
        let entry = ZipEntry {
            name: "AndroidManifest.xml".into(),
            method: 0,
            compressed_size: 0,
            local_offset: u64::MAX,
        };
        let outcome = std::panic::catch_unwind(|| extract(&entry, &data));
        assert!(outcome.is_ok(), "extract panicked on a bad local header");
        assert!(outcome.unwrap().is_err());
    }

    /// Build the smallest valid zip holding one deflate entry, in memory.
    /// Enough for parser tests without a real APK.
    fn tiny_zip(name: &str, body: &[u8]) -> Vec<u8> {
        use crc32fast::Hasher as Crc;
        use std::io::Write as _;
        let body_deflated = {
            use flate2::Compression;
            use flate2::write::DeflateEncoder;
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
            enc.write_all(body).unwrap();
            enc.finish().unwrap()
        };
        let crc = {
            let mut crc = Crc::new();
            crc.update(body);
            crc.finalize()
        };
        let name_bytes = name.as_bytes();
        let mut out = Vec::new();
        // Local header
        out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
        out.extend_from_slice(&[0x0a, 0x00]); // version needed: 10
        out.extend_from_slice(&[0x00, 0x00]); // flags
        out.extend_from_slice(&[0x08, 0x00]); // method: deflate
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // mtime/date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(body_deflated.len() as u32).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&body_deflated);
        let local_offset = 0u32;
        let cd_start = out.len() as u32;
        // Central directory entry
        out.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        out.extend_from_slice(&[0x14, 0x00]); // version made by
        out.extend_from_slice(&[0x0a, 0x00]); // version needed
        out.extend_from_slice(&[0x00, 0x00]); // flags
        out.extend_from_slice(&[0x08, 0x00]); // method
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // time
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(body_deflated.len() as u32).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]); // extra
        out.extend_from_slice(&[0, 0]); // comment
        out.extend_from_slice(&[0, 0]); // disk number
        out.extend_from_slice(&[0, 0]); // internal attrs
        out.extend_from_slice(&[0, 0, 0, 0]); // external attrs
        out.extend_from_slice(&local_offset.to_le_bytes());
        out.extend_from_slice(name_bytes);
        // EOCD
        let cd_len = out.len() as u32 - cd_start;
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        out.extend_from_slice(&[0, 0]); // disk
        out.extend_from_slice(&[0, 0]); // cd disk
        out.extend_from_slice(&[1, 0]); // entries
        out.extend_from_slice(&[1, 0]); // entries total
        out.extend_from_slice(&cd_len.to_le_bytes());
        out.extend_from_slice(&cd_start.to_le_bytes());
        out.extend_from_slice(&[0, 0]); // comment len
        out
    }

    #[test]
    fn reads_a_tiny_zip_member() {
        let body = b"hello android manifest".to_vec();
        let data = tiny_zip("AndroidManifest.xml", &body);
        let entries = central_directory(&data).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "AndroidManifest.xml");
        let got = extract(&entries[0], &data).unwrap();
        assert_eq!(got, body);
    }
}

#[cfg(test)]
mod apk_tests {
    use super::*;

    #[test]
    fn parses_a_real_apk_if_present() {
        let cands = [
            std::path::PathBuf::from("/home/rohith/Downloads/EDGE-v4.0.1-full-apkvision.apk"),
            std::path::PathBuf::from("/home/rohith/Downloads/radiothing-release-unsigned.apk"),
        ];
        for apk in cands.iter().filter(|p| p.exists()) {
            let info = match apk_info(apk) {
                Ok(i) => i,
                Err(e) => {
                    println!("skip {}: {e}", apk.display());
                    continue;
                }
            };
            println!("info: {info:?}");
            assert!(!info.package.is_empty());
        }
    }
}
