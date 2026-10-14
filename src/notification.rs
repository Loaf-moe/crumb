//! The notification as crumb understands it. The wire shape is turned into
//! these types during deserialisation, so the rest of crumb never sees a
//! half-valid notification: a counts-only update cannot carry an alert, a
//! missing count is already zero, and the alert text is already truncated.

use serde::de::{Deserializer, IgnoredAny};
use serde::{Deserialize, Serialize};

/// What the user sees if the app's Notification Service Extension can't
/// run. Lowercase body copy is the app's voice.
const FALLBACK_TITLE: &str = "Loaf Chat";
const FALLBACK_BODY: &str = "new message";

/// Caps that keep the payload under APNs's 4096-byte limit even with the
/// Matrix maximum of 255-byte room and event ids, each sent more than once.
const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 2000;

#[derive(Debug, Deserialize)]
#[serde(from = "RawNotification")]
pub struct Notification {
    pub push: Push,
    pub priority: Priority,
    pub badge: Badge,
    pub devices: Vec<Device>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    /// A notification about one event: shown, and rewritten on the phone.
    Event(Event),
    /// The spec's counts-only update: no `event_id`, just a badge refresh.
    Counts,
}

impl Push {
    pub fn event_id(&self) -> Option<&str> {
        match self {
            Push::Event(event) => Some(&event.event_id),
            Push::Counts => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Event {
    pub event_id: String,
    pub room_id: Option<String>,
    pub alert: Alert,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Alert {
    pub title: String,
    pub body: String,
}

/// The spec's `prio`. Absent means high.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    #[default]
    High,
    Low,
}

/// The app icon's number: unread messages plus missed calls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Badge(pub u64);

/// The spec omits zero counts, so absent is zero.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Counts {
    pub unread: u64,
    pub missed_calls: u64,
}

impl From<Counts> for Badge {
    fn from(counts: Counts) -> Self {
        Badge(counts.unread.saturating_add(counts.missed_calls))
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    #[default]
    Silent,
    Default,
}

impl Sound {
    /// `aps.sound`. The app ships no sounds named after server values and
    /// iOS plays the default for an unknown name anyway, so say so plainly.
    pub fn apns_name(self) -> Option<&'static str> {
        match self {
            Sound::Silent => None,
            Sound::Default => Some("default"),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Device {
    pub app_id: String,
    pub pushkey: String,
    #[serde(default, rename = "tweaks")]
    pub sound: Sound,
}

/// `tweaks` decides only the sound: any `sound` tweak means play one.
impl<'de> Deserialize<'de> for Sound {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Tweaks {
            sound: Option<IgnoredAny>,
        }
        Ok(match Tweaks::deserialize(d)?.sound {
            Some(_) => Sound::Default,
            None => Sound::Silent,
        })
    }
}

/// The spec's `notification`, as sent. Only `devices` is required. Fields
/// crumb has no use for (`pushkey_ts`, `data`) are skipped by serde.
#[derive(Deserialize)]
struct RawNotification {
    event_id: Option<String>,
    room_id: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    sender: Option<String>,
    sender_display_name: Option<String>,
    room_name: Option<String>,
    room_alias: Option<String>,
    #[serde(default)]
    user_is_target: bool,
    #[serde(default)]
    prio: Priority,
    content: Option<RawContent>,
    #[serde(default)]
    counts: Counts,
    devices: Vec<Device>,
}

/// Only the two content keys the alert reads. A key of an unexpected type
/// is treated as absent, not as a malformed request: content belongs to
/// the sender, not to the homeserver.
#[derive(Deserialize)]
struct RawContent {
    #[serde(default, deserialize_with = "string_or_none")]
    body: Option<String>,
    #[serde(default, deserialize_with = "string_or_none")]
    membership: Option<String>,
}

fn string_or_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Str(String),
        Other(IgnoredAny),
    }
    Ok(match Lenient::deserialize(d)? {
        Lenient::Str(s) => Some(s),
        Lenient::Other(_) => None,
    })
}

impl From<RawNotification> for Notification {
    fn from(raw: RawNotification) -> Self {
        let push = match raw.event_id {
            None => Push::Counts,
            Some(event_id) => {
                let kind = Kind::new(raw.event_type.as_deref(), raw.content, raw.user_is_target);
                Push::Event(Event {
                    event_id,
                    room_id: raw.room_id,
                    alert: Alert::new(
                        raw.room_name.or(raw.room_alias),
                        raw.sender_display_name.or(raw.sender),
                        kind,
                    ),
                })
            }
        };
        Notification {
            push,
            priority: raw.prio,
            badge: raw.counts.into(),
            devices: raw.devices,
        }
    }
}

/// The event kinds whose alert text crumb can say something useful about.
enum Kind {
    Message(Option<String>),
    Invite,
    Other,
}

impl Kind {
    fn new(event_type: Option<&str>, content: Option<RawContent>, user_is_target: bool) -> Self {
        match (event_type, content) {
            (Some("m.room.message"), content) => Kind::Message(content.and_then(|c| c.body)),
            (Some("m.room.member"), Some(c))
                if user_is_target && c.membership.as_deref() == Some("invite") =>
            {
                Kind::Invite
            }
            _ => Kind::Other,
        }
    }
}

impl Alert {
    /// `room` is the room's name or alias, `sender` the sender's display
    /// name or id. With `event_id_only` the homeserver sends none of these,
    /// which gives the generic fallback.
    fn new(room: Option<String>, sender: Option<String>, kind: Kind) -> Self {
        let body = match kind {
            Kind::Message(Some(text)) => match (&room, &sender) {
                (Some(_), Some(who)) => format!("{who}: {text}"),
                _ => text,
            },
            Kind::Invite => match &sender {
                Some(who) => format!("{who} invited you"),
                None => FALLBACK_BODY.to_owned(),
            },
            Kind::Message(None) | Kind::Other => FALLBACK_BODY.to_owned(),
        };
        let title = room.or(sender).unwrap_or_else(|| FALLBACK_TITLE.to_owned());
        Alert {
            title: truncated(title, MAX_TITLE),
            body: truncated(body, MAX_BODY),
        }
    }
}

/// Cuts `s` in place to at most `max` bytes on a char boundary, ending
/// with an ellipsis.
fn truncated(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max - '…'.len_utf8();
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Notification {
        serde_json::from_str(json).unwrap()
    }

    const DEVICE: &str = r#""devices":[{"app_id":"a","pushkey":"00"}]"#;

    fn event(fields: &str) -> Event {
        match parse(&format!(r#"{{"event_id":"$e",{fields},{DEVICE}}}"#)).push {
            Push::Event(e) => e,
            Push::Counts => panic!("expected an event"),
        }
    }

    #[test]
    fn event_id_only_gets_the_generic_alert() {
        let n = parse(&format!(
            r#"{{"event_id":"$e","room_id":"!r","counts":{{"unread":2}},{DEVICE}}}"#
        ));
        assert_eq!(
            n.push,
            Push::Event(Event {
                event_id: "$e".into(),
                room_id: Some("!r".into()),
                alert: Alert {
                    title: "Loaf Chat".into(),
                    body: "new message".into()
                },
            })
        );
        assert_eq!(n.badge, Badge(2));
        assert_eq!(n.priority, Priority::High);
    }

    #[test]
    fn no_event_id_is_a_counts_only_update() {
        let n = parse(&format!(r#"{{"room_name":"Den",{DEVICE}}}"#));
        assert_eq!(n.push, Push::Counts);
        assert_eq!(n.badge, Badge(0));
    }

    #[test]
    fn badge_adds_unread_and_missed_calls_saturating() {
        let n = parse(&format!(
            r#"{{"counts":{{"unread":2,"missed_calls":3}},{DEVICE}}}"#
        ));
        assert_eq!(n.badge, Badge(5));
        let n = parse(&format!(
            r#"{{"counts":{{"unread":18446744073709551615,"missed_calls":1}},{DEVICE}}}"#
        ));
        assert_eq!(n.badge, Badge(u64::MAX));
    }

    #[test]
    fn prio_low_is_low_and_anything_else_is_rejected() {
        assert_eq!(
            parse(&format!(r#"{{"prio":"low",{DEVICE}}}"#)).priority,
            Priority::Low
        );
        assert_eq!(
            parse(&format!(r#"{{"prio":"high",{DEVICE}}}"#)).priority,
            Priority::High
        );
        assert!(
            serde_json::from_str::<Notification>(&format!(r#"{{"prio":"urgent",{DEVICE}}}"#))
                .is_err()
        );
    }

    #[test]
    fn any_sound_tweak_means_the_default_sound() {
        let n = parse(
            r#"{"devices":[
                {"app_id":"a","pushkey":"00","tweaks":{"sound":"ping","highlight":true}},
                {"app_id":"a","pushkey":"01","tweaks":{"highlight":true}},
                {"app_id":"a","pushkey":"02"}]}"#,
        );
        let sounds: Vec<Sound> = n.devices.iter().map(|d| d.sound).collect();
        assert_eq!(sounds, [Sound::Default, Sound::Silent, Sound::Silent]);
    }

    #[test]
    fn devices_and_their_ids_are_required() {
        for json in [
            r#"{}"#,
            r#"{"devices":[{"app_id":"a"}]}"#,
            r#"{"devices":"x"}"#,
        ] {
            assert!(
                serde_json::from_str::<Notification>(json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn titles_and_bodies_follow_the_display_fields() {
        let cases = [
            (
                r#""type":"m.room.message","room_name":"Den","sender_display_name":"Alice","sender":"@a:x","content":{"body":"hi"}"#,
                "Den",
                "Alice: hi",
            ),
            (
                r##""type":"m.room.message","room_alias":"#den:x","sender":"@a:x","content":{"body":"hi"}"##,
                "#den:x",
                "@a:x: hi",
            ),
            (
                r#""type":"m.room.message","sender_display_name":"Alice","content":{"body":"hi"}"#,
                "Alice",
                "hi",
            ),
            (
                r#""type":"m.room.message","sender":"@a:x","content":{"body":"hi"}"#,
                "@a:x",
                "hi",
            ),
            (
                r#""type":"m.room.message","room_name":"Den","content":{"body":"hi"}"#,
                "Den",
                "hi",
            ),
            (
                r#""type":"m.room.message","content":{"body":7}"#,
                "Loaf Chat",
                "new message",
            ),
            (r#""type":"m.room.message""#, "Loaf Chat", "new message"),
            (
                r#""type":"m.room.encrypted","room_name":"Den","sender":"@a:x","content":{"ciphertext":"x"}"#,
                "Den",
                "new message",
            ),
            (
                r#""type":"m.room.member","user_is_target":true,"sender_display_name":"Alice","content":{"membership":"invite"}"#,
                "Alice",
                "Alice invited you",
            ),
            (
                r#""type":"m.room.member","user_is_target":false,"sender_display_name":"Alice","content":{"membership":"invite"}"#,
                "Alice",
                "new message",
            ),
            (
                r#""type":"m.room.member","user_is_target":true,"content":{"membership":"invite"}"#,
                "Loaf Chat",
                "new message",
            ),
            (
                r#""type":"m.room.member","user_is_target":true,"sender":"@a:x","content":{"membership":"join"}"#,
                "@a:x",
                "new message",
            ),
        ];
        for (fields, title, body) in cases {
            let alert = event(fields).alert;
            assert_eq!(
                (alert.title.as_str(), alert.body.as_str()),
                (title, body),
                "{fields}"
            );
        }
    }

    #[test]
    fn long_text_is_cut_on_a_char_boundary() {
        let fields = format!(
            r#""type":"m.room.message","room_name":"{}","content":{{"body":"{}"}}"#,
            "名".repeat(500),
            "é".repeat(5000),
        );
        let alert = event(&fields).alert;
        assert!(alert.title.len() <= MAX_TITLE && alert.title.ends_with('…'));
        assert!(alert.body.len() <= MAX_BODY && alert.body.ends_with('…'));
    }

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(truncated("hi".into(), 10), "hi");
        assert_eq!(truncated("x".repeat(10), 10), "x".repeat(10));
    }
}
