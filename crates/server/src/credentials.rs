//! PINs and passwords (spec 2.1 §4): format rules and argon2id hashing.
//!
//! PIN: 4–6 digits. Password: at least 8 characters. Both are stored as argon2id PHC strings
//! (`users.pin_hash`, `users.password_hash`). Verification is CPU-heavy by design, so the async
//! path runs it on the blocking thread pool.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::http::StatusCode;

use crate::error::ApiError;

/// Shortest PIN (spec 2.1 §4).
pub const PIN_MIN_DIGITS: usize = 4;
/// Longest PIN (spec 2.1 §4).
pub const PIN_MAX_DIGITS: usize = 6;
/// Shortest password, in characters (spec 2.1 §4).
pub const PASSWORD_MIN_CHARS: usize = 8;

fn argon2id() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default())
}

/// Checks the PIN format: 4–6 ASCII digits.
///
/// # Errors
/// `422 invalid_pin`.
pub fn validate_pin(pin: &str) -> Result<(), ApiError> {
    let ok = (PIN_MIN_DIGITS..=PIN_MAX_DIGITS).contains(&pin.len())
        && pin.bytes().all(|b| b.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_pin",
            "a PIN is 4 to 6 digits",
        ))
    }
}

/// Checks the password length: at least 8 characters.
///
/// # Errors
/// `422 weak_password`.
pub fn validate_password(password: &str) -> Result<(), ApiError> {
    if password.chars().count() >= PASSWORD_MIN_CHARS {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "weak_password",
            "a password has at least 8 characters",
        ))
    }
}

/// Hashes a secret with argon2id and a fresh random salt; returns the PHC string.
///
/// # Panics
/// Never with the fixed default parameters used here.
#[must_use]
pub fn hash_secret(secret: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    argon2id()
        .hash_password(secret.as_bytes(), &salt)
        .expect("argon2id with default parameters accepts any input")
        .to_string()
}

/// Validates and hashes a PIN.
///
/// # Errors
/// `422 invalid_pin`.
pub fn hash_pin(pin: &str) -> Result<String, ApiError> {
    validate_pin(pin)?;
    Ok(hash_secret(pin))
}

/// Validates and hashes a password.
///
/// # Errors
/// `422 weak_password`.
pub fn hash_password(password: &str) -> Result<String, ApiError> {
    validate_password(password)?;
    Ok(hash_secret(password))
}

/// Whether `secret` matches the stored PHC string. A malformed stored hash never matches.
#[must_use]
pub fn verify_secret(phc: &str, secret: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|parsed| {
        argon2id()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok()
    })
}

/// [`verify_secret`] on the blocking thread pool, so a login does not stall the async runtime.
///
/// # Errors
/// `500 internal` if the blocking task panicked.
pub async fn verify_blocking(phc: String, secret: String) -> Result<bool, ApiError> {
    tokio::task::spawn_blocking(move || verify_secret(&phc, &secret))
        .await
        .map_err(|_| ApiError::internal())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_format() {
        let cases = [
            ("1234", true),
            ("123456", true),
            ("123", false),
            ("1234567", false),
            ("12a4", false),
            ("", false),
            ("١٢٣٤", false), // non-ASCII digits
        ];
        for (pin, ok) in cases {
            assert_eq!(validate_pin(pin).is_ok(), ok, "{pin:?}");
        }
        assert_eq!(validate_pin("1").unwrap_err().code(), "invalid_pin");
    }

    #[test]
    fn password_length_counts_characters() {
        let cases = [("12345678", true), ("1234567", false), ("ééééééé", false)];
        for (password, ok) in cases {
            assert_eq!(validate_password(password).is_ok(), ok, "{password:?}");
        }
        assert_eq!(validate_password("").unwrap_err().code(), "weak_password");
    }

    #[tokio::test]
    async fn hash_and_verify() {
        let phc = hash_pin("1234").unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_secret(&phc, "1234"));
        assert!(!verify_secret(&phc, "1235"));
        assert!(!verify_secret("not a phc string", "1234"));
        assert!(verify_blocking(phc.clone(), "1234".into()).await.unwrap());
        assert!(!verify_blocking(phc, "0000".into()).await.unwrap());

        assert!(hash_pin("12").is_err());
        assert!(hash_password("short").is_err());
        let password = hash_password("long enough").unwrap();
        assert!(verify_secret(&password, "long enough"));
        assert_ne!(hash_secret("same"), hash_secret("same"), "fresh salt");
    }
}
