//! An APNs device token: the pushkey of an iOS pusher, as hex.

use std::fmt;

/// A pushkey that is safe to put in an APNs request path: non-empty,
/// even-length ASCII hex. Borrowed, since it only lives for one send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceToken<'a>(&'a str);

#[derive(Debug, PartialEq, Eq)]
pub struct NotHex;

impl<'a> TryFrom<&'a str> for DeviceToken<'a> {
    type Error = NotHex;

    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        let valid =
            !s.is_empty() && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit());
        if valid { Ok(Self(s)) } else { Err(NotHex) }
    }
}

impl<'a> DeviceToken<'a> {
    pub fn as_str(self) -> &'a str {
        self.0
    }
}

impl fmt::Display for DeviceToken<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_even_length_hex_in_either_case() {
        for s in ["00", "abcdef0123456789", "ABCDEF", &"a1".repeat(32)] {
            assert_eq!(DeviceToken::try_from(s).map(|t| t.as_str()), Ok(s));
        }
    }

    #[test]
    fn refuses_anything_else() {
        for s in ["", "0", "abc", "zz", "00/../x", "aa bb", "éé"] {
            assert_eq!(DeviceToken::try_from(s), Err(NotHex), "{s:?}");
        }
    }
}
