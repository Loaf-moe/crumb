//! Pushkeys are APNs device tokens: crumb never logs one. It logs a short
//! hash instead, stable enough to follow one device through the logs.

use std::fmt;

use sha2::{Digest, Sha256};

/// Displays as the first 8 hex characters of SHA-256(pushkey). The hash is
/// only computed if the log line is actually written.
pub struct Pk<'a>(pub &'a str);

impl fmt::Display for Pk<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Sha256::digest(self.0.as_bytes())[..4]
            .iter()
            .try_for_each(|b| write!(f, "{b:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_the_sha256_prefix() {
        // SHA-256("abc") = ba7816bf8f01cfea...
        assert_eq!(Pk("abc").to_string(), "ba7816bf");
    }

    #[test]
    fn never_contains_the_pushkey() {
        let key = "0123456789abcdef0123456789abcdef";
        let hashed = Pk(key).to_string();
        assert_eq!(hashed.len(), 8);
        assert!(!key.contains(&hashed));
    }
}
