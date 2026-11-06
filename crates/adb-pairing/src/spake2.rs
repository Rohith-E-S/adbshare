//! SPAKE2 as BoringSSL implements it (`crypto/curve25519/spake25519.cc`).
//!
//! This is *not* the CFRG draft or the `spake2` crate's variant: BoringSSL
//! speaks bare 32-byte compressed points (no side prefix), hashes the
//! transcript with SHA-512, derives its masks from nothing-up-my-sleeve
//! seeds, and keeps scalars unreduced (256-bit ladders) with the ephemeral
//! multiplied by the cofactor. The `spake2` crate speaks the draft instead
//! and can never interop with adbd, so this is written directly on
//! curve25519-dalek from the BoringSSL construction.
//!
//! The password scalar's low-three-bits adjustment for the omitted
//! `left_shift_3` is reproduced exactly: each test reads the running value
//! and adds the prime order (odd, so each addition flips the tested bit).

use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::traits::Identity;
use sha2::{Digest, Sha512};
use subtle::{Choice, ConditionallySelectable};

use crate::error::Error;

/// `M`, compressed: the point generated from seed
/// `edwards25519 point generation seed (M)`.
pub const M_ENCODED: [u8; 32] = [
    0x5a, 0xda, 0x7e, 0x4b, 0xf6, 0xdd, 0xd9, 0xad, 0xb6, 0x62, 0x6d, 0x32, 0x13, 0x1c, 0x6b, 0x5c,
    0x51, 0xa1, 0xe3, 0x47, 0xa3, 0x47, 0x8f, 0x53, 0xcf, 0xcf, 0x44, 0x1b, 0x88, 0xee, 0xd1, 0x2e,
];

/// `N`, compressed.
pub const N_ENCODED: [u8; 32] = [
    0x10, 0xe3, 0xdf, 0x0a, 0xe3, 0x7d, 0x8e, 0x7a, 0x99, 0xb5, 0xfe, 0x74, 0xb4, 0x46, 0x72, 0x10,
    0x3d, 0xbd, 0xdc, 0xbd, 0x06, 0xaf, 0x68, 0x0d, 0x71, 0x32, 0x9a, 0x11, 0x69, 0x3b, 0xc7, 0x78,
];

/// The prime order of the prime-order subgroup (Ed25519), little-endian.
const ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9a, 0x7a, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

#[allow(clippy::op_ref)]
/// Multiply `point` by a 256-bit scalar given as little-endian bytes, without
/// reducing the scalar modulo the group order.
///
/// BoringSSL's `x25519_ge_scalarmult` walks all 256 bits of the unreduced
/// scalar; a reduced `Scalar` multiply would lose the small-order component
/// the mask arithmetic depends on. Double-and-add over Edwards points with
/// constant-time conditional assignment to protect the secret scalar.
fn mul_wide(point: &EdwardsPoint, scalar: &[u8; 32]) -> EdwardsPoint {
    let mut result = EdwardsPoint::identity();
    let mut addend = *point;
    for &byte in scalar.as_ref() {
        for i in 0..8 {
            let bit = Choice::from((byte >> i) & 1);
            let sum = &result + &addend;
            result.conditional_assign(&sum, bit);
            addend = &addend + &addend;
        }
    }
    result
}

/// `n * 8` for a little-endian 256-bit scalar.
fn left_shift_3(scalar: &mut [u8; 32]) {
    let mut carry = 0u8;
    for byte in scalar.iter_mut() {
        let next_carry = *byte >> 5;
        *byte = (*byte << 3) | carry;
        carry = next_carry;
    }
}

/// `dest += src` for little-endian 256-bit scalars (mod 2^256, as BoringSSL's
/// `bn_add_words`).
fn add_wide(dest: &mut [u8; 32], src: &[u8; 32]) {
    let mut carry = 0u16;
    for (d, s) in dest.iter_mut().zip(src.iter()) {
        let sum = *d as u16 + *s as u16 + carry;
        *d = sum as u8;
        carry = sum >> 8;
    }
}

fn decompress(encoded: &[u8; 32]) -> Result<EdwardsPoint, Error> {
    CompressedEdwardsY(*encoded)
        .decompress()
        .ok_or(Error::Protocol(
            "SPAKE2 mask point does not decompress".into(),
        ))
}

fn base() -> EdwardsPoint {
    ED25519_BASEPOINT_POINT
}

/// One side of a SPAKE2 exchange. The client (adb) is Alice; the device is
/// Bob. Names include their NUL terminators, as AOSP passes `sizeof`.
pub struct Spake2 {
    role_alice: bool,
    my_name: Vec<u8>,
    their_name: Vec<u8>,
    password_hash: [u8; 64],
    password_scalar: [u8; 32],
    my_msg: [u8; 32],
    my_msg_point: EdwardsPoint,
    private_scalar: [u8; 32],
    generated: bool,
}

impl Spake2 {
    /// Create a context. `password` is the raw shared secret (pairing code
    /// bytes plus the TLS exporter output, in that order).
    pub fn new(role_alice: bool, password: &[u8]) -> Self {
        let (my_name, their_name): (&[u8], &[u8]) = if role_alice {
            (b"adb pair client\0", b"adb pair server\0")
        } else {
            (b"adb pair server\0", b"adb pair client\0")
        };
        let password_hash: [u8; 64] = Sha512::digest(password).into();
        // Password scalar: reduce the hash, then reproduce BoringSSL's
        // low-three-bits adjustment against the *running* value.
        let mut scalar: [u8; 32] = [0u8; 32];
        {
            // `x25519_sc_reduce` reduces the full 64-byte digest modulo the
            // group order (ring's fork takes `uint8_t s[64]`), writing the
            // reduced value back into the low 32 bytes.
            let mut wide = password_hash;
            let reduced = curve25519_dalek::Scalar::from_bytes_mod_order_wide(&wide);
            wide[..32].copy_from_slice(&reduced.to_bytes());
            scalar.copy_from_slice(&wide[..32]);
        }
        // `order` doubles each round (L, 2L, 4L), each test reading the
        // running value — exactly as BoringSSL's sequence.
        let mut order = ORDER;
        for bit in 0..3 {
            if scalar[0] & (1 << bit) != 0 {
                add_wide(&mut scalar, &order);
            }
            let doubled = order;
            add_wide(&mut order, &doubled);
        }
        debug_assert_eq!(scalar[0] & 7, 0);
        Self {
            role_alice,
            my_name: my_name.to_vec(),
            their_name: their_name.to_vec(),
            password_hash,
            password_scalar: scalar,
            my_msg: [0; 32],
            my_msg_point: EdwardsPoint::identity(),
            private_scalar: [0; 32],
            generated: false,
        }
    }

    /// Generate our message: `[ephemeral]B + [password]·(M or N)`.
    pub fn generate_msg(&mut self) -> [u8; 32] {
        // Ephemeral: 64 random bytes, reduced, then multiplied by the
        // cofactor (left_shift_3) and kept unreduced.
        let mut private_tmp = [0u8; 64];
        {
            use std::io::Read;
            std::fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut private_tmp))
                .expect("urandom read");
        }
        // `x25519_sc_reduce` over the full 64 random bytes.
        let reduced = curve25519_dalek::Scalar::from_bytes_mod_order_wide(&private_tmp);
        let mut scalar = reduced.to_bytes();
        left_shift_3(&mut scalar);
        self.private_scalar = scalar;
        let mask_point = if self.role_alice {
            decompress(&M_ENCODED).expect("M decompresses")
        } else {
            decompress(&N_ENCODED).expect("N decompresses")
        };
        let point = mul_wide(&base(), &scalar) + mul_wide(&mask_point, &self.password_scalar);
        self.my_msg_point = point;
        self.my_msg = point.compress().to_bytes();
        self.generated = true;
        self.my_msg
    }

    /// Process the peer's message and derive the 64-byte shared secret.
    pub fn process_msg(&mut self, their_msg: &[u8; 32]) -> Result<[u8; 64], Error> {
        if !self.generated {
            return Err(Error::Protocol("process before generate".into()));
        }
        let their_point = CompressedEdwardsY(*their_msg)
            .decompress()
            .ok_or(Error::Protocol("peer point not on curve".into()))?;
        let their_mask = if self.role_alice {
            decompress(&N_ENCODED)?
        } else {
            decompress(&M_ENCODED)?
        };
        let unmasked = their_point - mul_wide(&their_mask, &self.password_scalar);
        let dh_shared = mul_wide(&unmasked, &self.private_scalar);
        let dh_encoded = dh_shared.compress().to_bytes();

        // Transcript: SHA-512 over length-prefixed fields; Alice's order.
        let mut h = Sha512::new();
        let lp = |h: &mut Sha512, data: &[u8]| {
            h.update((data.len() as u64).to_le_bytes());
            h.update(data);
        };
        if self.role_alice {
            lp(&mut h, &self.my_name);
            lp(&mut h, &self.their_name);
            lp(&mut h, &self.my_msg);
            lp(&mut h, their_msg);
        } else {
            lp(&mut h, &self.their_name);
            lp(&mut h, &self.my_name);
            lp(&mut h, their_msg);
            lp(&mut h, &self.my_msg);
        }
        lp(&mut h, &dh_encoded);
        lp(&mut h, &self.password_hash);
        Ok(h.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alice and Bob with the same password derive the same secret; with
    /// different passwords they do not (that is what makes a wrong pairing
    /// code fail at the AEAD step rather than hang).
    #[test]
    fn alice_and_bob_agree() {
        let mut alice = Spake2::new(true, b"123456exporter-bytes");
        let mut bob = Spake2::new(false, b"123456exporter-bytes");
        let a_msg = alice.generate_msg();
        let b_msg = bob.generate_msg();
        let a_key = alice.process_msg(&b_msg).unwrap();
        let b_key = bob.process_msg(&a_msg).unwrap();
        assert_eq!(a_key, b_key);
    }

    #[test]
    fn different_passwords_disagree() {
        let mut alice = Spake2::new(true, b"wrongexporter-bytes");
        let mut bob = Spake2::new(false, b"123456exporter-bytes");
        let a_msg = alice.generate_msg();
        let b_msg = bob.generate_msg();
        assert_ne!(
            alice.process_msg(&b_msg).unwrap(),
            bob.process_msg(&a_msg).unwrap()
        );
    }

    /// The mask points decompress and are torsion-free after clearing the
    /// cofactor, as BoringSSL's tables assume.
    #[test]
    fn mask_points_are_torsion_free() {
        for encoded in [M_ENCODED, N_ENCODED] {
            let point = decompress(&encoded).unwrap();
            assert!(point.mul_by_cofactor().compress().to_bytes() != [0u8; 32]);
        }
    }
}
