//! Studio-format QR pairing payloads.
//!
//! Format (WPA3 piggyback, as in AOSP `docs/dev/adb_wifi.md`):
//! `WIFI:T:ADB;S:studio-<10>;P:<password>;;` — the phone's camera handler
//! starts a pairing server advertising the requested instance name, and the
//! password is the SPAKE2 shared secret.

pub const INSTANCE_PREFIX: &str = "studio-";
pub const INSTANCE_SUFFIX_LEN: usize = 10;
pub const PASSWORD_LEN: usize = 16;

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// A pairing session identity: the mDNS instance suffix and the 16-byte
/// shared secret, both random per scan.
#[derive(Debug, Clone)]
pub struct QrSession {
    pub instance: String,
    pub password: String,
}

fn random(len: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; len];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("urandom read");
    buf.iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

pub fn new_session() -> QrSession {
    QrSession {
        instance: format!("{INSTANCE_PREFIX}{}", random(INSTANCE_SUFFIX_LEN)),
        password: random(PASSWORD_LEN),
    }
}

/// The QR payload string.
pub fn payload(session: &QrSession) -> String {
    format!("WIFI:T:ADB;S:{};P:{};;", session.instance, session.password)
}

/// Human-readable form of the payload for display: instance + password.
pub fn describe(session: &QrSession) -> String {
    format!(
        "Instance {}  ·  Password {}",
        session.instance, session.password
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_shape() {
        let session = QrSession {
            instance: "studio-AbC123XyZ9".into(),
            password: "p4ssw0rdAbCdEfGh".into(),
        };
        assert_eq!(
            payload(&session),
            "WIFI:T:ADB;S:studio-AbC123XyZ9;P:p4ssw0rdAbCdEfGh;;"
        );
    }

    #[test]
    fn sessions_are_unique_and_shaped() {
        let a = new_session();
        let b = new_session();
        assert_ne!(a.instance, b.instance);
        assert!(a.instance.starts_with(INSTANCE_PREFIX));
        assert_eq!(
            a.instance.len(),
            INSTANCE_PREFIX.len() + INSTANCE_SUFFIX_LEN
        );
        assert_eq!(a.password.len(), PASSWORD_LEN);
    }
}
