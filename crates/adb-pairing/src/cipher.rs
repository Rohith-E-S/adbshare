//! AES-128-GCM transport for `PeerInfo`, keyed by HKDF-SHA256 over the
//! SPAKE2 shared secret (AOSP `pairing_auth/aes_128_gcm.cpp`).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use sha2_011::Sha256;
use typenum::U12;

use crate::error::Error;

const HKDF_INFO: &[u8] = b"adb pairing_auth aes-128-gcm key";

/// Derive the 16-byte AES key from the 64-byte SPAKE2 shared secret:
/// HKDF-SHA256 with no salt and the fixed info string.
pub fn derive_key(shared_secret: &[u8; 64]) -> [u8; 16] {
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret);
    let mut key = [0u8; 16];
    hkdf.expand(HKDF_INFO, &mut key)
        .expect("16 bytes fits HKDF");
    key
}

/// Counter nonce: sequence number little-endian in the first 8 bytes, the
/// rest zero, per direction, each starting at 0.
fn nonce(sequence: u64) -> Nonce<U12> {
    let mut bytes = [0u8; 12];
    bytes[..8].copy_from_slice(&sequence.to_le_bytes());
    Nonce::from(bytes)
}

/// Both directions of one connection's ciphertexts.
pub struct PairingCipher {
    encrypt: Aes128Gcm,
    decrypt: Aes128Gcm,
    enc_sequence: u64,
    dec_sequence: u64,
}

impl PairingCipher {
    pub fn new(shared_secret: &[u8; 64]) -> Self {
        let key_bytes = derive_key(shared_secret);
        let key = aes_gcm::Key::<Aes128Gcm>::from(key_bytes);
        Self {
            encrypt: Aes128Gcm::new(&key),
            decrypt: Aes128Gcm::new(&key),
            enc_sequence: 0,
            dec_sequence: 0,
        }
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let ciphertext = self
            .encrypt
            .encrypt(&nonce(self.enc_sequence), plaintext)
            .expect("AES-GCM encrypt does not fail on memory");
        self.enc_sequence += 1;
        ciphertext
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        let plaintext = self
            .decrypt
            .decrypt(&nonce(self.dec_sequence), ciphertext)
            .map_err(|_| Error::CodeMismatch)?;
        self.dec_sequence += 1;
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_directions_round_trip_and_replay_fails() {
        let secret = [7u8; 64];
        let mut alice = PairingCipher::new(&secret);
        let mut bob = PairingCipher::new(&secret);
        let record = alice.encrypt(b"hello");
        assert_eq!(record.len(), 5 + 16);
        assert_eq!(bob.decrypt(&record).unwrap(), b"hello");
        // A replayed record reuses a nonce and fails authentication.
        assert!(bob.decrypt(&record).is_err());
        let back = bob.encrypt(b"world");
        assert_eq!(alice.decrypt(&back).unwrap(), b"world");
    }

    #[test]
    fn wrong_secret_fails() {
        let mut alice = PairingCipher::new(&[7u8; 64]);
        let mut bob = PairingCipher::new(&[8u8; 64]);
        let record = alice.encrypt(b"hello");
        assert!(bob.decrypt(&record).is_err());
    }
}
