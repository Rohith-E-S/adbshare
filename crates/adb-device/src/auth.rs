//! ADB authentication: RSA key generation, persistence, and the AUTH handshake.
//!
//! The key is stored under `$XDG_DATA_HOME/adbshare/adb_key.pkcs8` (default
//! `~/.local/share/adbshare/adb_key.pkcs8`) in PKCS#8 PEM format. If the user
//! has an existing OpenSSH-format `~/.android/adbkey` from the `adb` tool, we
//! import it on first run and copy it to our location.
//!
//! Note: `adbd` (the device side) only accepts RSA SHA-1 signatures, so the
//! algorithm choice is fixed by the protocol.

use std::{fs, path::{Path, PathBuf}};

use pkcs1::DecodeRsaPrivateKey;
use rsa::{
    pkcs1v15::{Signature, SigningKey},
    pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding},
    signature::{RandomizedSigner, SignatureEncoding},
    RsaPrivateKey, RsaPublicKey,
};
use sha1::Sha1;
use ssh_key::private::PrivateKey as SshPrivateKey;
use zeroize::Zeroize;

use crate::error::{AdbError, Result};

/// 2048-bit RSA key used for ADB authentication.
#[derive(Debug)]
pub struct AdbKey {
    pkcs8_der: Vec<u8>,
    ssh_pub: Vec<u8>,
    path: PathBuf,
}

impl AdbKey {
    pub fn from_pkcs8_der(pkcs8_der: Vec<u8>, path: PathBuf) -> Result<Self> {
        let ssh_pub = ssh_pub_from_pkcs8(&pkcs8_der)?;
        Ok(Self { pkcs8_der, ssh_pub, path })
    }

    pub fn ssh_public(&self) -> &[u8] { &self.ssh_pub }
    pub fn pkcs8_der(&self) -> &[u8] { &self.pkcs8_der }
    pub fn path(&self) -> &Path { &self.path }

    /// Sign a 20-byte SHA-1 token sent by the device during AUTH.
    pub fn sign(&self, token: &[u8]) -> Result<Vec<u8>> {
        let key = RsaPrivateKey::from_pkcs8_der(&self.pkcs8_der)
            .map_err(|e| AdbError::Other(format!("parse pkcs8: {e}")))?;
        let signing_key = SigningKey::<Sha1>::new(key);
        let mut rng = rand::thread_rng();
        let sig: Signature = signing_key.sign_with_rng(&mut rng, token);
        Ok(sig.to_bytes().to_vec())
    }
}

impl Drop for AdbKey {
    fn drop(&mut self) {
        self.pkcs8_der.zeroize();
    }
}

pub fn generate_key(path: &Path) -> Result<AdbKey> {
    let mut rng = rand::thread_rng();
    let key = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|e| AdbError::Other(format!("rsa keygen: {e}")))?;
    let pkcs8_doc = key
        .to_pkcs8_der()
        .map_err(|e| AdbError::Other(format!("encode pkcs8: {e}")))?;
    let pkcs8_der = pkcs8_doc.as_bytes().to_vec();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pem = pkcs8_doc.to_pem("PRIVATE KEY", LineEnding::LF)?;
    fs::write(path, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }

    let ssh_pub = ssh_pub_from_pkcs8(&pkcs8_der)?;
    Ok(AdbKey { pkcs8_der, ssh_pub, path: path.to_path_buf() })
}

pub fn load_or_create_key() -> Result<AdbKey> {
    let our_path = default_key_path();

    if our_path.exists() {
        return load_pkcs8_pem(&our_path, &our_path);
    }

    if let Some(android_path) = android_adb_key_path() {
        if android_path.exists() {
            // Try OpenSSH first (old adb), then PKCS#8 (new adb since 2017).
            if let Ok(k) = import_openssh(&android_path, &our_path) { return Ok(k); }
            return import_pkcs8_pem(&android_path, &our_path);
        }
    }

    generate_key(&our_path)
}

fn load_pkcs8_pem(pem_path: &Path, store_path: &Path) -> Result<AdbKey> {
    let pem = fs::read(pem_path)?;
    let key = RsaPrivateKey::from_pkcs8_pem(&String::from_utf8_lossy(&pem))
        .map_err(|e| AdbError::Other(format!("parse pkcs8 pem: {e}")))?;
    let pkcs8_der = key
        .to_pkcs8_der()
        .map_err(|e| AdbError::Other(format!("re-encode pkcs8: {e}")))?
        .as_bytes()
        .to_vec();
    let ssh_pub = ssh_pub_from_pkcs8(&pkcs8_der)?;
    Ok(AdbKey { pkcs8_der, ssh_pub, path: store_path.to_path_buf() })
}

fn import_pkcs8_pem(pem_path: &Path, store_path: &Path) -> Result<AdbKey> {
    let pem = fs::read(pem_path)?;
    let key = RsaPrivateKey::from_pkcs8_pem(&String::from_utf8_lossy(&pem))
        .map_err(|e| AdbError::Other(format!("parse pkcs8 pem: {e}")))?;
    if let Some(parent) = store_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let out = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| AdbError::Other(format!("encode pkcs8 pem: {e}")))?;
    fs::write(store_path, out.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(store_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(store_path, perms)?;
    }
    let pkcs8_der = key
        .to_pkcs8_der()
        .map_err(|e| AdbError::Other(format!("re-encode pkcs8: {e}")))?
        .as_bytes()
        .to_vec();
    let ssh_pub = ssh_pub_from_pkcs8(&pkcs8_der)?;
    Ok(AdbKey { pkcs8_der, ssh_pub, path: store_path.to_path_buf() })
}

fn import_openssh(openssh_path: &Path, store_path: &Path) -> Result<AdbKey> {
    let pem = fs::read(openssh_path)?;
    let priv_key = SshPrivateKey::from_openssh(&pem)
        .map_err(|e| AdbError::Other(format!("parse openssh: {e}")))?;
    let rsa_data = match priv_key.key_data() {
        ssh_key::private::KeypairData::Rsa(rsa) => rsa,
        _ => return Err(AdbError::Other("not an RSA key".into())),
    };
    let key: RsaPrivateKey = rsa_data
        .try_into()
        .map_err(|e: ssh_key::Error| AdbError::Other(format!("openssh -> rsa: {e}")))?;

    if let Some(parent) = store_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| AdbError::Other(format!("encode pkcs8 pem: {e}")))?;
    fs::write(store_path, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(store_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(store_path, perms)?;
    }

    let pkcs8_der = key
        .to_pkcs8_der()
        .map_err(|e| AdbError::Other(format!("re-encode pkcs8: {e}")))?
        .as_bytes()
        .to_vec();
    let ssh_pub = ssh_pub_from_pkcs8(&pkcs8_der)?;
    Ok(AdbKey { pkcs8_der, ssh_pub, path: store_path.to_path_buf() })
}

pub fn default_key_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("adbshare").join("adb_key.pkcs8")
}

fn android_adb_key_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".android").join("adbkey"))
}

fn ssh_pub_from_pkcs8(pkcs8_der: &[u8]) -> Result<Vec<u8>> {
    use rsa::traits::PublicKeyParts;
    let key = RsaPrivateKey::from_pkcs8_der(pkcs8_der)
        .map_err(|e| AdbError::Other(format!("parse pkcs8: {e}")))?;
    let n = key.n().to_bytes_be();
    let e = key.e().to_bytes_be();

    // SSH wire format: string "ssh-rsa", mpint e, mpint n.
    let mut wire = Vec::new();
    push_ssh_string(&mut wire, b"ssh-rsa");
    push_ssh_mpint(&mut wire, &e);
    push_ssh_mpint(&mut wire, &n);

    // Android adbd expects the "Android public key" text format:
    //   base64(ssh-rsa-blob) + " " + user@host + '\0'
    // The payload sent as AUTH RSAPUBLICKEY must include the trailing NUL.
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let host = current_hostname();
    let mut out = base64_encode(&wire).into_bytes();
    out.extend_from_slice(format!(" {user}@{host}\0").as_bytes());
    Ok(out)
}

fn current_hostname() -> String {
    // HOSTNAME is set by most shells; fall back to the kernel hostname file
    // (nix's gethostname() is gated behind a feature we can't enable here
    // without adding a dependency feature).
    std::env::var("HOSTNAME").ok()
        .or_else(|| {
            fs::read_to_string("/proc/sys/kernel/hostname")
                .or_else(|_| fs::read_to_string("/etc/hostname"))
                .ok()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string())
}

/// Minimal standard-alphabet base64 encoder (no padding-skipping shortcuts).
/// Hand-rolled because this crate has no base64 dependency.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

fn push_ssh_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

fn push_ssh_mpint(out: &mut Vec<u8>, bytes: &[u8]) {
    // Per RFC 4251 the stored value must be minimal: strip redundant leading
    // zero bytes, then prepend a single zero byte if the high bit is set
    // (to mark the number as positive).
    let mut v = bytes;
    while v.len() > 1 && v[0] == 0 {
        v = &v[1..];
    }
    if v.first().map_or(false, |b| *b & 0x80 != 0) {
        out.extend_from_slice(&((v.len() + 1) as u32).to_be_bytes());
        out.push(0);
        out.extend_from_slice(v);
    } else {
        push_ssh_string(out, v);
    }
}
