//! APNs provider tokens: ES256 JWTs signed with the app's `.p8` key.
//!
//! Apple rejects a token older than an hour and answers
//! `TooManyProviderTokenUpdates` to one refreshed more often than every 20
//! minutes, so a single token is shared until it is 40 minutes old.

use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::HeaderValue;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde::Serialize;

use crate::config::{KeyId, TeamId};

/// Renew once a token is this old (seconds): inside Apple's 20–60 minutes.
const MAX_AGE: u64 = 40 * 60;
/// A forced renewal never replaces a token younger than this (seconds), so a
/// burst of `ExpiredProviderToken` answers can't trip
/// `TooManyProviderTokenUpdates`.
const MIN_RENEW_GAP: u64 = 60;

/// Seconds since the Unix epoch. Injected so tests can move time.
pub trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

pub struct TokenSource {
    key: SigningKey,
    key_id: KeyId,
    team_id: TeamId,
    clock: Arc<dyn Clock>,
    cached: RwLock<Option<Cached>>,
}

struct Cached {
    /// `bearer <jwt>`, ready for the `authorization` header. Cloning a
    /// `HeaderValue` only bumps a reference count.
    bearer: HeaderValue,
    issued_at: u64,
}

impl TokenSource {
    pub fn new(key: SigningKey, key_id: KeyId, team_id: TeamId, clock: Arc<dyn Clock>) -> Self {
        Self {
            key,
            key_id,
            team_id,
            clock,
            cached: RwLock::new(None),
        }
    }

    /// The shared `authorization` value, renewed once it reaches [`MAX_AGE`].
    pub fn bearer(&self) -> HeaderValue {
        self.bearer_younger_than(MAX_AGE)
    }

    /// APNs called the token expired or invalid: replace it, unless it was
    /// issued within the last minute, in which case it is returned as is.
    pub fn renew(&self) -> HeaderValue {
        self.bearer_younger_than(MIN_RENEW_GAP)
    }

    fn bearer_younger_than(&self, age: u64) -> HeaderValue {
        let now = self.clock.now();
        let fresh = |cached: &Option<Cached>| {
            cached
                .as_ref()
                .filter(|c| now.saturating_sub(c.issued_at) < age)
                .map(|c| c.bearer.clone())
        };
        // Every push takes the read lock; only a renewal takes the write
        // lock. A poisoned lock still holds a valid Option, so keep going.
        if let Some(bearer) = fresh(&self.cached.read().unwrap_or_else(|e| e.into_inner())) {
            return bearer;
        }
        let mut cached = self.cached.write().unwrap_or_else(|e| e.into_inner());
        // Another request may have renewed while this one waited.
        if let Some(bearer) = fresh(&cached) {
            return bearer;
        }
        let bearer = self.sign(now);
        *cached = Some(Cached {
            bearer: bearer.clone(),
            issued_at: now,
        });
        bearer
    }

    fn sign(&self, iat: u64) -> HeaderValue {
        #[derive(Serialize)]
        struct Header<'a> {
            alg: &'static str,
            kid: &'a str,
        }
        #[derive(Serialize)]
        struct Claims<'a> {
            iss: &'a str,
            iat: u64,
        }
        let header = Header {
            alg: "ES256",
            kid: self.key_id.as_str(),
        };
        let claims = Claims {
            iss: self.team_id.as_str(),
            iat,
        };
        let mut jwt = String::with_capacity(256);
        jwt.push_str("bearer ");
        let start = jwt.len();
        encode_json(&header, &mut jwt);
        jwt.push('.');
        encode_json(&claims, &mut jwt);
        // JWS ES256 signs `header.claims` and wants the raw 64-byte r||s.
        let signature: Signature = self.key.sign(&jwt.as_bytes()[start..]);
        jwt.push('.');
        URL_SAFE_NO_PAD.encode_string(signature.to_bytes(), &mut jwt);

        let mut bearer = HeaderValue::try_from(jwt)
            .expect("bearer plus base64url is always a valid header value");
        bearer.set_sensitive(true);
        bearer
    }
}

fn encode_json(value: &impl Serialize, out: &mut String) {
    let json = serde_json::to_vec(value).expect("JWT header and claims always serialise");
    URL_SAFE_NO_PAD.encode_string(json, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeClock(AtomicU64);

    impl FakeClock {
        fn at(t: u64) -> Arc<Self> {
            Arc::new(Self(AtomicU64::new(t)))
        }
        fn advance(&self, secs: u64) {
            self.0.fetch_add(secs, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn key() -> SigningKey {
        SigningKey::from_slice(&[7; 32]).unwrap()
    }

    fn source(clock: Arc<FakeClock>) -> TokenSource {
        TokenSource::new(
            key(),
            KeyId::try_from("KEY1234567".to_owned()).unwrap(),
            TeamId::try_from("6W2A5N37N3".to_owned()).unwrap(),
            clock,
        )
    }

    fn jwt(bearer: &HeaderValue) -> &str {
        bearer.to_str().unwrap().strip_prefix("bearer ").unwrap()
    }

    fn decode(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    fn iat(bearer: &HeaderValue) -> Value {
        decode(jwt(bearer).split('.').nth(1).unwrap())["iat"].clone()
    }

    #[test]
    fn bearer_is_a_verifiable_es256_jwt() {
        let bearer = source(FakeClock::at(1_000)).bearer();
        assert!(bearer.is_sensitive());
        let parts: Vec<&str> = jwt(&bearer).split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            decode(parts[0]),
            json!({ "alg": "ES256", "kid": "KEY1234567" })
        );
        assert_eq!(
            decode(parts[1]),
            json!({ "iss": "6W2A5N37N3", "iat": 1_000 })
        );

        let raw = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(raw.len(), 64);
        let signature = Signature::from_slice(&raw).unwrap();
        let input = format!("{}.{}", parts[0], parts[1]);
        key()
            .verifying_key()
            .verify(input.as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn bearer_is_reused_until_forty_minutes_then_renewed() {
        let clock = FakeClock::at(1_000);
        let tokens = source(clock.clone());
        let first = tokens.bearer();
        clock.advance(MAX_AGE - 1);
        assert_eq!(tokens.bearer(), first);
        clock.advance(1);
        let second = tokens.bearer();
        assert_eq!(iat(&second), 1_000 + MAX_AGE);
    }

    #[test]
    fn renew_replaces_a_token_a_minute_old() {
        let clock = FakeClock::at(1_000);
        let tokens = source(clock.clone());
        tokens.bearer();
        clock.advance(MIN_RENEW_GAP);
        let renewed = tokens.renew();
        assert_eq!(iat(&renewed), 1_000 + MIN_RENEW_GAP);
        assert_eq!(tokens.bearer(), renewed);
    }

    #[test]
    fn renew_keeps_a_token_younger_than_a_minute() {
        let clock = FakeClock::at(1_000);
        let tokens = source(clock.clone());
        let first = tokens.bearer();
        clock.advance(MIN_RENEW_GAP - 1);
        assert_eq!(tokens.renew(), first);
    }

    #[test]
    fn renew_with_nothing_cached_issues_a_token() {
        let tokens = source(FakeClock::at(1_000));
        assert_eq!(iat(&tokens.renew()), 1_000);
    }
}
