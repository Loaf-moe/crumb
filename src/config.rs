//! crumb's TOML config. Everything is checked at startup: a running crumb
//! always has a readable key and well-formed Apple identifiers for every
//! app it serves.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use http::HeaderValue;
use http::uri::{Authority, Scheme};
use p256::ecdsa::SigningKey;
use p256::pkcs8::DecodePrivateKey;
use serde::Deserialize;

pub struct Config {
    pub listen: SocketAddr,
    /// Keyed by the pusher `app_id`. Never empty.
    pub apps: HashMap<String, AppConfig>,
}

pub struct AppConfig {
    pub key: SigningKey,
    /// Where the key was read from: logged at startup, never its contents.
    pub key_path: PathBuf,
    pub key_id: KeyId,
    pub team_id: TeamId,
    pub topic: Topic,
    pub endpoint: Endpoint,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml(&text)
    }

    /// Parses the config, then reads and decodes every app's key.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(ConfigError::Parse)?;
        if raw.apps.is_empty() {
            return Err(ConfigError::NoApps);
        }
        let apps = raw
            .apps
            .into_iter()
            .map(|(app_id, app)| {
                let key = read_key(&app.key_path).map_err(|reason| ConfigError::Key {
                    app_id: app_id.clone(),
                    path: app.key_path.clone(),
                    reason,
                })?;
                let app = AppConfig {
                    key,
                    key_path: app.key_path,
                    key_id: app.key_id,
                    team_id: app.team_id,
                    topic: app.topic,
                    endpoint: app.endpoint,
                };
                Ok((app_id, app))
            })
            .collect::<Result<_, _>>()?;
        Ok(Config {
            listen: raw.listen,
            apps,
        })
    }
}

/// The `.p8` from developer.apple.com is a PKCS#8 PEM P-256 key.
fn read_key(path: &Path) -> Result<SigningKey, KeyError> {
    let pem = std::fs::read_to_string(path).map_err(KeyError::Read)?;
    SigningKey::from_pkcs8_pem(&pem).map_err(|_| KeyError::NotAP256Key)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_listen")]
    listen: SocketAddr,
    apps: HashMap<String, RawApp>,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    key_path: PathBuf,
    key_id: KeyId,
    team_id: TeamId,
    topic: Topic,
    endpoint: Endpoint,
}

/// Apple's identifiers are ten uppercase letters or digits.
fn is_apple_id(s: &str) -> bool {
    s.len() == 10
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The APNs auth key's id, from developer.apple.com → Keys.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct KeyId(String);

/// The Apple developer team that owns the key and the app.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct TeamId(String);

impl TryFrom<String> for KeyId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        is_apple_id(&s)
            .then_some(Self(s))
            .ok_or("key_id must be 10 uppercase letters or digits")
    }
}

impl TryFrom<String> for TeamId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        is_apple_id(&s)
            .then_some(Self(s))
            .ok_or("team_id must be 10 uppercase letters or digits")
    }
}

impl KeyId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TeamId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `apns-topic`: the app's bundle id. Stored as the header it becomes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Topic(HeaderValue);

impl TryFrom<String> for Topic {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let valid = !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
        if !valid {
            return Err("topic must be a bundle id");
        }
        HeaderValue::try_from(s)
            .map(Self)
            .map_err(|_| "topic must be a bundle id")
    }
}

impl From<Topic> for HeaderValue {
    fn from(topic: Topic) -> Self {
        topic.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endpoint {
    Production,
    Sandbox,
}

/// Where APNs requests go. Built from an [`Endpoint`], which is always
/// HTTPS to Apple; tests build a plain-HTTP one for a fake APNs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: Scheme,
    pub authority: Authority,
}

impl From<Endpoint> for Origin {
    fn from(endpoint: Endpoint) -> Self {
        let host = match endpoint {
            Endpoint::Production => "api.push.apple.com",
            Endpoint::Sandbox => "api.sandbox.push.apple.com",
        };
        Origin {
            scheme: Scheme::HTTPS,
            authority: Authority::from_static(host),
        }
    }
}

impl From<SocketAddr> for Origin {
    /// Plain HTTP/2 (h2c) to a local address: only a test's fake APNs.
    fn from(addr: SocketAddr) -> Self {
        Origin {
            scheme: Scheme::HTTP,
            authority: addr
                .to_string()
                .parse()
                .expect("a socket address is a valid authority"),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
    NoApps,
    /// Names the app and the path only: never the key's contents.
    Key {
        app_id: String,
        path: PathBuf,
        reason: KeyError,
    },
}

#[derive(Debug)]
pub enum KeyError {
    Read(std::io::Error),
    NotAP256Key,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "cannot read config {}: {source}", path.display())
            }
            ConfigError::Parse(e) => write!(f, "invalid config: {e}"),
            ConfigError::NoApps => f.write_str("invalid config: [apps] has no app"),
            ConfigError::Key {
                app_id,
                path,
                reason,
            } => {
                write!(f, "app {app_id}: key {}: {reason}", path.display())
            }
        }
    }
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Read(e) => write!(f, "cannot read: {e}"),
            KeyError::NotAP256Key => f.write_str("not a PKCS#8 P-256 key (.p8)"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } => Some(source),
            ConfigError::Parse(e) => Some(e),
            ConfigError::Key {
                reason: KeyError::Read(e),
                ..
            } => Some(e),
            ConfigError::NoApps | ConfigError::Key { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A file in the temp dir, removed on drop.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(contents: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "crumb-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn p8() -> TempFile {
        let key = SigningKey::from_slice(&[7; 32]).unwrap();
        TempFile::new(&key.to_pkcs8_pem(LineEnding::LF).unwrap())
    }

    fn toml(key: &Path, extra: &str) -> String {
        format!(
            r#"{extra}
[apps."moe.loaf.chat.ios"]
key_path = "{}"
key_id = "KEY1234567"
team_id = "6W2A5N37N3"
topic = "moe.loaf.chat.ios"
endpoint = "production"
"#,
            key.display()
        )
    }

    #[test]
    fn loads_an_app_and_its_key() {
        let key = p8();
        let config = Config::from_toml(&toml(&key.0, "")).unwrap();
        assert_eq!(config.listen, default_listen());
        let app = &config.apps["moe.loaf.chat.ios"];
        assert_eq!(app.key_id.as_str(), "KEY1234567");
        assert_eq!(app.team_id.as_str(), "6W2A5N37N3");
        assert_eq!(HeaderValue::from(app.topic.clone()), "moe.loaf.chat.ios");
        assert_eq!(app.endpoint, Endpoint::Production);
        assert_eq!(app.key, SigningKey::from_slice(&[7; 32]).unwrap());
        assert_eq!(
            app.key_path, key.0,
            "kept so startup can log where the key came from"
        );
    }

    #[test]
    fn a_key_saved_with_crlf_and_no_final_newline_loads() {
        let key = SigningKey::from_slice(&[7; 32]).unwrap();
        let pem = key.to_pkcs8_pem(LineEnding::CRLF).unwrap();
        let p8 = TempFile::new(pem.trim_end());
        assert!(Config::from_toml(&toml(&p8.0, "")).is_ok());
    }

    #[test]
    fn listen_can_be_set() {
        let key = p8();
        let config = Config::from_toml(&toml(&key.0, r#"listen = "127.0.0.1:9000""#)).unwrap();
        assert_eq!(config.listen, "127.0.0.1:9000".parse().unwrap());
    }

    #[test]
    fn a_missing_key_names_the_app_and_path() {
        let missing = Path::new("/nonexistent/crumb.p8");
        let e = Config::from_toml(&toml(missing, "")).err().unwrap();
        let msg = e.to_string();
        assert!(
            msg.contains("moe.loaf.chat.ios") && msg.contains("/nonexistent/crumb.p8"),
            "{msg}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_key_is_refused_without_echoing_it() {
        let junk = TempFile::new("SECRET-LOOKING-CONTENTS");
        let msg = Config::from_toml(&toml(&junk.0, ""))
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("not a PKCS#8 P-256 key"), "{msg}");
        assert!(!msg.contains("SECRET"), "{msg}");
    }

    #[test]
    fn no_apps_is_an_error() {
        assert!(matches!(
            Config::from_toml("[apps]"),
            Err(ConfigError::NoApps)
        ));
    }

    #[test]
    fn bad_fields_are_refused() {
        let key = p8();
        let good = toml(&key.0, "");
        for (from, to) in [
            (r#"key_id = "KEY1234567""#, r#"key_id = "short""#),
            (r#"team_id = "6W2A5N37N3""#, r#"team_id = "6w2a5n37n3""#),
            (r#"topic = "moe.loaf.chat.ios""#, r#"topic = "bad topic""#),
            (r#"endpoint = "production""#, r#"endpoint = "https://evil""#),
            (
                r#"endpoint = "production""#,
                "endpoint = \"production\"\nsurprise = 1",
            ),
        ] {
            let bad = good.replace(from, to);
            assert!(
                matches!(Config::from_toml(&bad), Err(ConfigError::Parse(_))),
                "{to}"
            );
        }
    }

    #[test]
    fn endpoints_are_https_to_apple() {
        assert_eq!(
            Origin::from(Endpoint::Production),
            Origin {
                scheme: Scheme::HTTPS,
                authority: Authority::from_static("api.push.apple.com")
            }
        );
        assert_eq!(
            Origin::from(Endpoint::Sandbox).authority,
            "api.sandbox.push.apple.com"
        );
    }
}
