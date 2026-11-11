//! Customer codes: the random short key printed on the member card (spec 2.5 §2).

use std::fmt;
use std::str::FromStr;

use super::CustomerError;

/// The 30 symbols of a customer code: digits and upper-case letters without `0 O 1 I L U`.
pub const CODE_ALPHABET: &[u8; 30] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
/// The length of a customer code, without the dash.
pub const CODE_LEN: usize = 8;

/// The largest multiple of the alphabet size that fits in a byte: random bytes at or above it
/// are skipped so that every symbol is equally likely.
#[allow(clippy::cast_possible_truncation)] // 30 * 8 = 240 fits in a u8.
const UNBIASED_LIMIT: u8 = (CODE_ALPHABET.len() * (256 / CODE_ALPHABET.len())) as u8;

/// A customer code: 8 symbols of [`CODE_ALPHABET`]. Stored without the dash (`K7QH29XM`,
/// [`CustomerCode::as_str`]) and shown with it (`K7QH-29XM`, `Display`).
///
/// Codes are drawn at random by whoever creates the customer ([`CustomerCode::generate`]), so
/// branches create them offline without coordinating. Uniqueness is the database's job: a clash
/// draws a new code (spec 2.5 §2 "Code clashes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CustomerCode([u8; CODE_LEN]);

impl CustomerCode {
    /// Draws a new code from `random_byte`, a source of uniformly random bytes (`core` has no
    /// random number generator; the caller injects one, e.g. `|| rand::random()`).
    ///
    /// Bytes `240..=255` are skipped so that each symbol is equally likely, so the source must
    /// not return only such bytes (a constant source of them never finishes).
    pub fn generate(mut random_byte: impl FnMut() -> u8) -> Self {
        let mut code = [0; CODE_LEN];
        let mut filled = 0;
        while filled < CODE_LEN {
            let b = random_byte();
            if b < UNBIASED_LIMIT {
                code[filled] = CODE_ALPHABET[usize::from(b) % CODE_ALPHABET.len()];
                filled += 1;
            }
        }
        Self(code)
    }

    /// Parses a typed or scanned code: surrounding spaces are ignored, letters are upper-cased
    /// and one dash after the 4th symbol is optional (`k7qh-29xm` = `K7QH29XM`).
    ///
    /// # Errors
    /// [`CustomerError::InvalidCode`] for the wrong length, a dash elsewhere or a symbol outside
    /// the alphabet (e.g. `K7QH-29X0`).
    pub fn parse(input: &str) -> Result<Self, CustomerError> {
        let invalid = || CustomerError::InvalidCode {
            input: input.to_owned(),
        };
        let upper = input.trim().to_ascii_uppercase();
        let bytes = upper.as_bytes();
        let compact: Vec<u8> = if bytes.len() == CODE_LEN + 1 && bytes[4] == b'-' {
            bytes[..4].iter().chain(&bytes[5..]).copied().collect()
        } else {
            bytes.to_vec()
        };
        let code: [u8; CODE_LEN] = compact.try_into().map_err(|_| invalid())?;
        if code.iter().all(|b| CODE_ALPHABET.contains(b)) {
            Ok(Self(code))
        } else {
            Err(invalid())
        }
    }

    /// The stored form without the dash, e.g. `K7QH29XM`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Every byte comes from the ASCII alphabet.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

/// The shown form with the dash, e.g. `K7QH-29XM`.
impl fmt::Display for CustomerCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.as_str();
        write!(f, "{}-{}", &s[..4], &s[4..])
    }
}

impl FromStr for CustomerCode {
    type Err = CustomerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic byte source cycling through `bytes`.
    fn cycle(bytes: &[u8]) -> impl FnMut() -> u8 + '_ {
        let mut i = 0;
        move || {
            let b = bytes[i % bytes.len()];
            i += 1;
            b
        }
    }

    /// Spec 2.5 §15 case 2: generated codes are 8 symbols of the alphabet.
    #[test]
    fn generate_uses_the_alphabet() {
        let all: Vec<u8> = (0..=255).collect();
        let mut source = cycle(&all);
        for _ in 0..200 {
            let code = CustomerCode::generate(&mut source);
            assert_eq!(code.as_str().len(), CODE_LEN);
            assert!(code.as_str().bytes().all(|b| CODE_ALPHABET.contains(&b)));
            assert_eq!(CustomerCode::parse(code.as_str()), Ok(code));
            assert_eq!(CustomerCode::parse(&code.to_string()), Ok(code));
        }
    }

    #[test]
    fn generate_maps_bytes_and_skips_biased_ones() {
        // 0 → '2', 29 → 'Z', 30 → '2', 239 → 'Z'; 240..=255 skipped.
        let code = CustomerCode::generate(cycle(&[255, 0, 240, 29, 30, 239, 1, 2, 3, 4]));
        assert_eq!(code.as_str(), "2Z2Z3456");
        assert_eq!(UNBIASED_LIMIT, 240);
    }

    #[test]
    fn parse_cases() {
        let cases: &[(&str, Option<&str>)] = &[
            ("K7QH29XM", Some("K7QH29XM")),
            ("k7qh-29xm", Some("K7QH29XM")),
            ("K7QH-29XM", Some("K7QH29XM")),
            ("  k7qh29xm ", Some("K7QH29XM")),
            ("23456789", Some("23456789")),
            // 0 is not in the alphabet.
            ("K7QH-29X0", None),
            ("K7QH-29XO", None),
            ("K7QH-29XI", None),
            ("K7QH-29XL", None),
            ("K7QH-29XU", None),
            ("K7QH-29X1", None),
            // Wrong length or dash position.
            ("K7QH29X", None),
            ("K7QH29XMM", None),
            ("K7Q-H29XM", None),
            ("K7QH--29XM", None),
            ("K7QH 29XM", None),
            ("", None),
            // Non-ASCII.
            ("K7QH29XÉ", None),
        ];
        for (input, want) in cases {
            let got = input.parse::<CustomerCode>();
            match want {
                Some(want) => assert_eq!(got.unwrap().as_str(), *want, "{input}"),
                None => assert_eq!(
                    got.unwrap_err(),
                    CustomerError::InvalidCode {
                        input: (*input).to_owned()
                    },
                    "{input}"
                ),
            }
        }
    }

    #[test]
    fn display_has_the_dash() {
        let code = CustomerCode::parse("K7QH29XM").unwrap();
        assert_eq!(code.to_string(), "K7QH-29XM");
        assert_eq!(code.as_str(), "K7QH29XM");
    }
}
