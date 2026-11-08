//! Opaque secrets: device tokens and session tokens (spec 2.1 §2, §4).
//!
//! A token is 32 random bytes from the OS, encoded as unpadded base64url. The server stores only
//! the SHA-256 of the token *text* ([`hash`]), so a leaked database does not leak usable tokens
//! and revocation is a single row update. Pairing codes ([`pairing_code`]) are hashed the same
//! way.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use sultan_db::TokenHash;

/// Random bytes per token.
pub const TOKEN_BYTES: usize = 32;

/// A new random token: 32 bytes, base64url without padding (43 characters).
#[must_use]
pub fn generate() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Letters and digits of a pairing code: 32 symbols without the look-alikes `0 O 1 I`.
const PAIRING_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A new one-time pairing code (spec 2.1 §2), e.g. `K7Q2-M9XP`: 8 random symbols (40 bits) in
/// two groups, short enough to type into the register's setup wizard. It is valid for 10
/// minutes and single use, which is what keeps 40 bits enough.
#[must_use]
pub fn pairing_code() -> String {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    // 256 is a multiple of 32, so `% 32` is unbiased.
    let symbols: String = bytes
        .iter()
        .map(|b| char::from(PAIRING_ALPHABET[usize::from(*b) % 32]))
        .collect();
    format!("{}-{}", &symbols[..4], &symbols[4..])
}

/// SHA-256 of a token or pairing code as sent by the client. This is what the database stores.
#[must_use]
pub fn hash(secret: &str) -> TokenHash {
    Sha256::digest(secret.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_base64url_of_32_bytes() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).unwrap().len(), TOKEN_BYTES);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn pairing_codes_are_two_groups_of_four() {
        let a = pairing_code();
        assert_ne!(a, pairing_code());
        assert_eq!(a.len(), 9);
        assert_eq!(&a[4..5], "-");
        assert!(
            a.chars()
                .filter(|c| *c != '-')
                .all(|c| PAIRING_ALPHABET.contains(&(c as u8)))
        );
    }

    #[test]
    fn hash_is_sha256_of_the_text() {
        // SHA-256("abc"), FIPS 180-2 test vector.
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(hash("abc"), expected);
        assert_ne!(hash("abd"), expected);
    }
}
