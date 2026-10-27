//! Binary Android XML (AXML) reading.
//!
//! `AndroidManifest.xml` inside an APK is a compiled resource format, not
//! text: a string pool, an optional resource-id map, and a tree of start /
//! end element nodes. Only the start-element attribute values the install
//! card shows are read: package, versions, SDK bounds, the requested
//! permissions, and the app label's resource id (resolved against the APK's
//! own string pool via the fallback rule Android itself uses: prefer the
//! label attribute, and treat `@string/…`-style indirection by scanning the
//! resource map).

use super::{MANIFEST_NAME, central_directory, extract};
use anyhow::{Result, anyhow, bail};

/// The parsed parts of `AndroidManifest.xml`.
#[derive(Debug, Clone, Default)]
pub struct AxmlManifest {
    pub package: String,
    pub version_name: Option<String>,
    pub version_code: Option<u64>,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    pub label: Option<String>,
    pub permissions: Vec<String>,
}

/// Read `AndroidManifest.xml` bytes out of an APK that is already in memory.
pub fn manifest_from(data: &[u8]) -> Result<Vec<u8>> {
    let entries = central_directory(data)?;
    let entry = entries
        .iter()
        .find(|e| e.name == MANIFEST_NAME)
        .ok_or_else(|| anyhow!("no {MANIFEST_NAME} in this archive"))?;
    extract(entry, data)
}

/// Read the whole `resources.arsc`. Only the package name is needed from it.
pub fn resources_arsc_from(data: &[u8]) -> Result<Vec<u8>> {
    let entries = central_directory(data)?;
    let entry = entries
        .iter()
        .find(|e| e.name == "resources.arsc")
        .ok_or_else(|| anyhow!("no resources.arsc in this archive"))?;
    extract(entry, data)
}

/// The CPU architectures with native libs (`lib/<abi>/*.so`), by name only.
/// One central-directory pass, no extraction — the manifest is the file just
/// before this in the queue, so keep the pass cheap by reading max once.
pub fn native_abis_in(data: &[u8]) -> Result<Vec<String>> {
    let entries = central_directory(data)?;
    let mut seen = std::collections::BTreeSet::new();
    for e in &entries {
        if let Some(rest) = e.name.strip_prefix("lib/")
            && let Some(abi) = rest.split('/').next()
            && !abi.is_empty()
        {
            seen.insert(abi.to_string());
        }
    }
    Ok(seen.into_iter().collect())
}

// ── AXML chunk walking ──────────────────────────────────────────────────────

const RES_STRING_POOL: u16 = 0x0001;
const RES_XML_START_ELEMENT: u16 = 0x0102;
const RES_XML_END_ELEMENT: u16 = 0x0103;
const RES_XML_RESOURCE_MAP: u16 = 0x0180;
const RES_XML_START_NAMESPACE: u16 = 0x0100;

/// Attribute resource ids the parser cares about. The platform matches by id,
/// never by name-string, because obfuscated manifests reuse name strings for
/// the same ids anyway.
const ATTR_VERSION_CODE: u32 = 0x0101_021b;
const ATTR_PACKAGE: u32 = 0x0101_0146;
const ATTR_VERSION_NAME: u32 = 0x0101_021c;
const ATTR_MIN_SDK: u32 = 0x0101_020c;
const ATTR_TARGET_SDK: u32 = 0x0101_0270;
const ATTR_LABEL: u32 = 0x0101_0001;
const ATTR_PERMISSION_NAME: u32 = 0x0101_0003;

pub fn parse(data: &[u8]) -> Result<AxmlManifest> {
    let mut manifest = AxmlManifest::default();
    let mut strings: Vec<String> = Vec::new();
    let mut resource_map: Vec<u32> = Vec::new();
    let mut pos = 0usize;

    while pos + 8 <= data.len() {
        let kind = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let header = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        let size = usize::try_from(u32::from_le_bytes([
            data[pos + 4],
            data[pos + 5],
            data[pos + 6],
            data[pos + 7],
        ]))
        .unwrap_or(0);
        if size < header as usize || size == 0 || pos + size > data.len() {
            // Malformed tail; stop cleanly. What's parsed so far stays usable
            // if it contains the package and version.
            break;
        }
        let chunk = &data[pos..pos + size];
        if kind == 0x0003 {
            // RES_XML_TYPE: the file is a single wrapper chunk whose children
            // are the real chunks. Step past the wrapper's own header and
            // keep the loop running.
            pos += header as usize;
            continue;
        }
        pos += size;
        #[cfg(feature = "axmldebug")]
        eprintln!("chunk kind={kind:#06x} header={header} size={size}");
        match kind {
            RES_STRING_POOL => {
                strings = parse_string_pool(chunk)?;
            }
            RES_XML_RESOURCE_MAP => {
                for i in (header as usize..size).step_by(4) {
                    resource_map.push(u32::from_le_bytes([
                        chunk[i],
                        chunk[i + 1],
                        chunk[i + 2],
                        chunk[i + 3],
                    ]));
                }
            }
            RES_XML_START_ELEMENT => {
                let node = StartElement::parse(chunk, header as usize, &resource_map)?;
                if node.name_index.is_none() && node.name_string.is_none() {
                    continue;
                }
                #[cfg(feature = "axmldebug")]
                eprintln!(
                    "elem {:?} attrs {}",
                    element_name(&strings, &node),
                    node.attributes
                        .iter()
                        .map(|a| format!("{:#x}[{}]", a.id, a.string_index))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                match element_name(&strings, &node).as_deref() {
                    Some("manifest") => {
                        for attr in &node.attributes {
                            match attr {
                                a if a.id == ATTR_PACKAGE
                                    || strings
                                        .get(a.name_index as usize)
                                        .map(|s| s == "package")
                                        .unwrap_or(false) =>
                                {
                                    manifest.package = string_value(&strings, a);
                                }
                                a if a.id == ATTR_VERSION_CODE => {
                                    if let Some(v) = a.int_value {
                                        manifest.version_code = Some(v as u64);
                                    }
                                }
                                a if a.id == ATTR_VERSION_NAME => {
                                    manifest.version_name = Some(string_value(&strings, a));
                                }
                                _ => {}
                            }
                        }
                    }
                    Some("uses-sdk") => {
                        for attr in &node.attributes {
                            if attr.id == ATTR_MIN_SDK {
                                manifest.min_sdk = attr.int_value;
                            } else if attr.id == ATTR_TARGET_SDK {
                                manifest.target_sdk = attr.int_value;
                            }
                        }
                    }
                    Some("uses-permission") => {
                        for attr in &node.attributes {
                            if attr.id == ATTR_PERMISSION_NAME {
                                let name = string_value(&strings, attr);
                                if !name.is_empty() && !manifest.permissions.contains(&name) {
                                    manifest.permissions.push(name);
                                }
                            }
                        }
                    }
                    Some("application") => {
                        for attr in &node.attributes {
                            if attr.id == ATTR_LABEL {
                                let label = label_value(&strings, attr);
                                if !label.is_empty() {
                                    manifest.label = Some(label);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            RES_XML_END_ELEMENT | RES_XML_START_NAMESPACE => {}
            _ => {}
        }
    }

    if manifest.package.is_empty() && manifest.version_code.is_none() {
        bail!("manifest has neither package nor versionCode; likely not an AXML file");
    }
    Ok(manifest)
}

/// Strings from a compiled string pool. UTF-8 pools use Android's own varint
/// scheme (not literal length prefixes); UTF-16 is the common default.
fn parse_string_pool(chunk: &[u8]) -> Result<Vec<String>> {
    if chunk.len() < 28 {
        bail!("string pool too small");
    }
    let count = u32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]) as usize;
    let flags = u32::from_le_bytes([chunk[16], chunk[17], chunk[18], chunk[19]]);
    let strings_start = u32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]) as usize;
    let utf8 = flags & 0x0000_0100 != 0;

    let mut strings = Vec::with_capacity(count);
    for i in 0..count {
        let off = 28 + i * 4;
        if off + 4 > chunk.len() {
            break;
        }
        let so = u32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]])
            as usize;
        let s = strings_start + so;
        let string = if s >= chunk.len() {
            String::new()
        } else if utf8 {
            read_utf8_string(&chunk[s..])
        } else {
            read_utf16_string(&chunk[s..])
        };
        strings.push(string);
    }
    Ok(strings)
}

/// Android UTF-8 pool strings prefix their byte length with a varint whose
/// high bit extends it, and the values are technically not standard UTF-8
/// (modified-UTF8), which string decoding handles fine in the common case.
fn read_utf8_string(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    let (skip, _) = decode_length(data);
    let rest = &data[skip..];
    let (hlen, hskip) = decode_length(rest);
    let (hlen, hskip) = (hlen, hskip as usize);
    let bytes = &rest[hskip..(hskip + hlen).min(rest.len())];
    // AAPT normalises to real UTF-8 in pools despite the modified-UTF8 claim,
    // so lossy is the right eye here: never fail the manifest on an odd glyph.
    String::from_utf8_lossy(bytes).into_owned()
}

fn read_utf16_string(data: &[u8]) -> String {
    if data.len() < 2 {
        return String::new();
    }
    let len = u16::from_le_bytes([data[0], data[1]]) as usize;
    let start = 2;
    // Chars, so the length is in UTF-16 code units: 2 bytes each.
    let bytes_start = start;
    let bytes_end = (bytes_start + len * 2).min(data.len());
    let units: Vec<u16> = data[bytes_start..bytes_end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Android varint: one or two bytes, low 7 bits first, the high bit marks
/// continuation on the second byte only.
fn decode_length(data: &[u8]) -> (usize, u8) {
    if data.is_empty() {
        return (0, 0);
    }
    let b = data[0];
    if b & 0x80 != 0 && data.len() > 1 {
        (((data[1] & 0x7f) as usize) << 7 | (b & 0x7f) as usize, 2)
    } else {
        ((b & 0x7f) as usize, 1)
    }
}

#[derive(Debug, Default)]
struct StartElement {
    name_index: Option<u32>,
    name_string: Option<String>,
    attributes: Vec<Attribute>,
}

#[derive(Debug)]
struct Attribute {
    /// Resource id, resolved from the resource map by attribute-name pool
    /// index. `0` when unresolvable.
    id: u32,
    /// Index into the string pool of the attribute NAME.
    name_index: u32,
    /// Index into the string pool of the raw value, when there is one.
    string_index: u32,
    int_value: Option<u32>,
}

impl StartElement {
    /// Parse one RES_XML_START_ELEMENT body.
    ///
    /// Layout after the 8-byte chunk header: `lineNumber` (u32), `comment`
    /// (u32), then the attrExt struct: `ns` (u32), `name` (u32),
    /// `attributeStart` (u16, offset from attrExt's own start to the first
    /// attribute), `attributeSize` (u16, one attribute's byte count, 20 in
    /// every file this reader has seen), `attributeCount` (u16), then three
    /// index/pointers (id/class/style) rounding attrExt out at 20 bytes. One
    /// flat attribute is `ns`(4) `name`(4) `rawValue`(4) `size`(u16, 8)
    /// `res0` (u8), `dataType` (u8), `data` (u32).
    fn parse(chunk: &[u8], header: usize, resource_map: &[u32]) -> Result<Self> {
        // attrExt starts right after the node preamble (chunk header,
        // lineNumber, comment), i.e. at `header` itself.
        let attr_ext = header;
        if chunk.len() < attr_ext + 20 {
            bail!("start element chunk too small");
        }
        let name_index = u32::from_le_bytes([
            chunk[attr_ext + 4],
            chunk[attr_ext + 5],
            chunk[attr_ext + 6],
            chunk[attr_ext + 7],
        ]);
        let attribute_start = u16::from_le_bytes([chunk[attr_ext + 8], chunk[attr_ext + 9]]);
        let attribute_size = u16::from_le_bytes([chunk[attr_ext + 10], chunk[attr_ext + 11]]);
        let attribute_count = u16::from_le_bytes([chunk[attr_ext + 12], chunk[attr_ext + 13]]);
        if attribute_size < 20 {
            bail!("attribute entry claims size {attribute_size}, smaller than the minimum 20");
        }
        let base = attr_ext + attribute_start as usize;
        let mut attributes = Vec::with_capacity(attribute_count as usize);
        for i in 0..attribute_count as usize {
            let a = base + i * attribute_size as usize;
            if a + attribute_size as usize > chunk.len() {
                break;
            }
            let name_idx =
                u32::from_le_bytes([chunk[a + 4], chunk[a + 5], chunk[a + 6], chunk[a + 7]]);
            let string_index =
                u32::from_le_bytes([chunk[a + 8], chunk[a + 9], chunk[a + 10], chunk[a + 11]]);
            let data_type = chunk[a + 15];
            let data =
                u32::from_le_bytes([chunk[a + 16], chunk[a + 17], chunk[a + 18], chunk[a + 19]]);
            // The resource map maps attribute-name string index -> attribute
            // resource id; without it, ids match by name string in the pool.
            let id = resource_map
                .get(name_idx as usize)
                .copied()
                // Typed values that are raw strings decode below; the
                // constants are 0x08 = STRING, 0x10 = INT_DEC, 0x11 = INT_HEX.
                .unwrap_or(0);
            let int_value = match data_type {
                0x10 | 0x11 | 0x04 => Some(data),
                _ => None,
            };
            attributes.push(Attribute {
                id,
                name_index: name_idx,
                string_index,
                int_value,
            });
        }
        Ok(StartElement {
            name_index: Some(name_index),
            name_string: None,
            attributes,
        })
    }
}

/// Resolve the element name through either the pool index or an inline string
/// (some writers name elements literally, not by pool reference).
fn element_name(strings: &[String], node: &StartElement) -> Option<String> {
    if let Some(i) = node.name_index
        && let Some(s) = strings.get(i as usize)
    {
        return Some(s.clone());
    }
    node.name_string.clone()
}

/// A string attribute value, or the empty string for a non-string type.
fn string_value(strings: &[String], attr: &Attribute) -> String {
    strings
        .get(attr.string_index as usize)
        .cloned()
        .unwrap_or_default()
}

/// Resolve the `application[android:label]` value. A literal string when the
/// attribute is one; a resource reference (e.g. `@string/app_name`) otherwise,
/// in which case the label is left set from the raw reference — showing the
/// reference beats showing the file name for wrongness, and real packaging
/// tools almost always emit the string via the pool, which
/// [`string_value`] already resolved.
fn label_value(strings: &[String], attr: &Attribute) -> String {
    string_value(strings, attr)
}

/// Package name from `resources.arsc`'s first package chunk.
///
/// AGP 8 stopped writing the `package` attribute into AndroidManifest.xml, so
/// the application id has to come from the resource table instead:
/// `RES_TABLE_PACKAGE_TYPE` carries the name as a fixed-size string right
/// after the 4-byte package id.
pub fn package_name_from_arsc(data: &[u8]) -> Option<String> {
    // The table header (RES_TABLE_TYPE) is 12 bytes; after it come chunks at
    // top level: the global string pool, then one package chunk per package.
    let table_size = if data.len() >= 12 {
        u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize
    } else {
        return None;
    };
    let mut pos = table_size.min(data.len()).max(12);
    // Walk at most a few chunks; the package chunk is seconds in line.
    for _ in 0..4 {
        if pos + 8 > data.len() {
            break;
        }
        let kind = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let hdr = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        if kind == 0x0200
            && let Some(name) = package_name_from_package_chunk(
                &data[pos..(pos + size).min(data.len())],
                hdr as usize,
            )
            && !name.is_empty()
        {
            return Some(name);
        }
        if size == 0 {
            break;
        }
        pos += size;
    }
    None
}

/// The name blob starts right after the package id. It is nominally UTF-16
/// (128 code units), UTF-8 in UTF-8-flagged tables; decode as UTF-16 first
/// and fall back to UTF-8 when the result looks like binary noise.
fn package_name_from_package_chunk(chunk: &[u8], hdr: usize) -> Option<String> {
    if chunk.len() < hdr + 132 {
        return None;
    }
    let blob = &chunk[hdr + 4..hdr + 4 + 256];
    // UTF-16: chars keep going until NUL.
    let utf16 = blob
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect::<Vec<u16>>();
    if utf16.len() > 3
        && utf16.iter().all(|&u| {
            u == b'.' as u16
                || (u as u8).is_ascii_alphanumeric()
                || u == b'_' as u16
                || u == b'-' as u16
        })
    {
        return Some(String::from_utf16_lossy(&utf16));
    }
    // UTF-8 fallback: NUL-terminated in the same blob.
    let bytes = &blob[..blob.iter().position(|&b| b == 0).unwrap_or(blob.len())];
    if bytes.len() > 3
        && bytes
            .iter()
            .all(|&b| b == b'.' || b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Some(String::from_utf8_lossy(bytes).into_owned());
    }
    None
}
