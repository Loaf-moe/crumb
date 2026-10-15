//! The APNs request for a notification: headers plus the JSON body.
//!
//! The body depends only on the notification and the device's sound, so a
//! request with many devices serialises it at most twice, and every device
//! shares those bytes.

use std::sync::OnceLock;

use bytes::Bytes;
use http::HeaderValue;
use serde::{Serialize, Serializer};

use crate::notification::{Alert, Badge, Notification, Priority, Push, Sound};

/// APNs's limit on `apns-collapse-id`.
const MAX_COLLAPSE_ID: usize = 64;

/// What one device's APNs request carries. Cheap to clone: the header and
/// body are reference-counted.
#[derive(Debug, Clone)]
pub struct ApnsRequest {
    pub priority: Priority,
    pub collapse_id: Option<CollapseId>,
    pub body: Bytes,
}

/// `apns-collapse-id`: the event id, so a homeserver's retry replaces the
/// notification on the phone instead of adding a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapseId(HeaderValue);

/// The event id is too long or not header-safe; the push goes without one.
#[derive(Debug, PartialEq, Eq)]
pub struct NoCollapseId;

impl TryFrom<&str> for CollapseId {
    type Error = NoCollapseId;

    /// Visible ASCII only: HTTP tolerates other bytes in a header, but APNs
    /// promises nothing about them. Real event ids are all ASCII. An empty
    /// header is refused outright, with a reason crumb treats as transient.
    fn try_from(event_id: &str) -> Result<Self, Self::Error> {
        let fits = (1..=MAX_COLLAPSE_ID).contains(&event_id.len())
            && event_id.bytes().all(|b| b.is_ascii_graphic());
        if !fits {
            return Err(NoCollapseId);
        }
        HeaderValue::from_str(event_id)
            .map(Self)
            .map_err(|_| NoCollapseId)
    }
}

impl From<CollapseId> for HeaderValue {
    fn from(id: CollapseId) -> Self {
        id.0
    }
}

impl From<Priority> for HeaderValue {
    /// `apns-priority`: 10 sends now, 5 lets the phone save power.
    fn from(priority: Priority) -> Self {
        match priority {
            Priority::High => HeaderValue::from_static("10"),
            Priority::Low => HeaderValue::from_static("5"),
        }
    }
}

/// Builds each device's request from one notification, serialising each
/// distinct body once, on first use.
pub struct Requests<'a> {
    push: &'a Push,
    priority: Priority,
    badge: Badge,
    collapse_id: Option<CollapseId>,
    silent: OnceLock<Bytes>,
    sounding: OnceLock<Bytes>,
}

impl<'a> Requests<'a> {
    pub fn new(push: &'a Push, priority: Priority, badge: Badge) -> Self {
        Self {
            push,
            priority,
            badge,
            collapse_id: push.event_id().and_then(|id| CollapseId::try_from(id).ok()),
            silent: OnceLock::new(),
            sounding: OnceLock::new(),
        }
    }

    pub fn for_device(&self, sound: Sound) -> ApnsRequest {
        // A counts-only update has no sound, so it only ever needs one body.
        let sound = match self.push {
            Push::Event(_) => sound,
            Push::Counts => Sound::Silent,
        };
        let cell = match sound {
            Sound::Silent => &self.silent,
            Sound::Default => &self.sounding,
        };
        ApnsRequest {
            priority: self.priority,
            collapse_id: self.collapse_id.clone(),
            body: cell.get_or_init(|| self.serialise(sound)).clone(),
        }
    }

    fn serialise(&self, sound: Sound) -> Bytes {
        let payload = match self.push {
            Push::Event(event) => Payload::Event {
                aps: EventAps {
                    alert: &event.alert,
                    mutable_content: MutableContent,
                    thread_id: event.room_id.as_deref(),
                    badge: self.badge,
                    sound: sound.apns_name(),
                },
                room_id: event.room_id.as_deref(),
                event_id: &event.event_id,
            },
            Push::Counts => Payload::Counts {
                aps: CountsAps { badge: self.badge },
            },
        };
        // Every field is a string, an integer or a struct of those, which
        // serde_json always serialises: an error here is a bug in this file.
        serde_json::to_vec(&payload)
            .expect("APNs payload types always serialise")
            .into()
    }
}

impl<'a> From<&'a Notification> for Requests<'a> {
    fn from(n: &'a Notification) -> Self {
        Requests::new(&n.push, n.priority, n.badge)
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum Payload<'a> {
    /// Carries `room_id` and `event_id` at the top level for the app's
    /// Notification Service Extension, which fetches and decrypts the event.
    Event {
        aps: EventAps<'a>,
        #[serde(skip_serializing_if = "Option::is_none")]
        room_id: Option<&'a str>,
        event_id: &'a str,
    },
    Counts {
        aps: CountsAps,
    },
}

#[derive(Serialize)]
struct EventAps<'a> {
    alert: &'a Alert,
    #[serde(rename = "mutable-content")]
    mutable_content: MutableContent,
    /// Groups a room's notifications together.
    #[serde(rename = "thread-id", skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a str>,
    badge: Badge,
    #[serde(skip_serializing_if = "Option::is_none")]
    sound: Option<&'static str>,
}

#[derive(Serialize)]
struct CountsAps {
    badge: Badge,
}

/// `"mutable-content": 1`, the only value APNs gives meaning to: it routes
/// the notification through the app's extension.
struct MutableContent;

impl Serialize for MutableContent {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn notification(json: &str) -> Notification {
        serde_json::from_str(json).unwrap()
    }

    fn body(r: &ApnsRequest) -> Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    const DEVICE: &str = r#""devices":[{"app_id":"a","pushkey":"00"}]"#;

    #[test]
    fn an_event_carries_what_the_extension_reads() {
        let n = notification(&format!(
            r#"{{"event_id":"$e","room_id":"!r","counts":{{"unread":2}},{DEVICE}}}"#
        ));
        let r = Requests::from(&n).for_device(Sound::Silent);
        assert_eq!(
            body(&r),
            json!({
                "aps": {
                    "alert": { "title": "Loaf Chat", "body": "new message" },
                    "mutable-content": 1,
                    "thread-id": "!r",
                    "badge": 2,
                },
                "room_id": "!r",
                "event_id": "$e",
            })
        );
        assert_eq!(r.priority, Priority::High);
        assert_eq!(r.collapse_id, CollapseId::try_from("$e").ok());
    }

    #[test]
    fn the_sound_only_appears_for_a_sounding_device() {
        let n = notification(&format!(r#"{{"event_id":"$e",{DEVICE}}}"#));
        let requests = Requests::from(&n);
        assert_eq!(
            body(&requests.for_device(Sound::Default))["aps"]["sound"],
            "default"
        );
        assert!(
            body(&requests.for_device(Sound::Silent))["aps"]
                .get("sound")
                .is_none()
        );
    }

    #[test]
    fn each_body_is_serialised_once_and_shared() {
        let n = notification(&format!(r#"{{"event_id":"$e",{DEVICE}}}"#));
        let requests = Requests::from(&n);
        let a = requests.for_device(Sound::Silent).body;
        let b = requests.for_device(Sound::Silent).body;
        assert_eq!(a.as_ptr(), b.as_ptr());
    }

    #[test]
    fn no_room_id_leaves_out_thread_id_and_room_id() {
        let n = notification(&format!(r#"{{"event_id":"$e",{DEVICE}}}"#));
        let b = body(&Requests::from(&n).for_device(Sound::Silent));
        assert!(b["aps"].get("thread-id").is_none());
        assert!(b.get("room_id").is_none());
        assert_eq!(b["event_id"], "$e");
    }

    #[test]
    fn counts_only_is_a_bare_badge_without_sound_or_collapse_id() {
        let n = notification(&format!(r#"{{"counts":{{"unread":4}},{DEVICE}}}"#));
        let r = Requests::from(&n).for_device(Sound::Default);
        assert_eq!(body(&r), json!({ "aps": { "badge": 4 } }));
        assert_eq!(r.collapse_id, None);
    }

    #[test]
    fn counts_only_with_no_counts_clears_the_badge() {
        let n = notification(&format!(r#"{{{DEVICE}}}"#));
        assert_eq!(
            body(&Requests::from(&n).for_device(Sound::Silent)),
            json!({ "aps": { "badge": 0 } })
        );
    }

    #[test]
    fn collapse_id_needs_1_to_64_visible_ascii_bytes() {
        assert!(CollapseId::try_from("$".repeat(64).as_str()).is_ok());
        assert!(CollapseId::try_from("$".repeat(65).as_str()).is_err());
        for unfit in ["", "$a\nb", "$a b", "$évènement"] {
            assert!(CollapseId::try_from(unfit).is_err(), "{unfit:?}");
        }
    }

    #[test]
    fn priority_maps_to_apns_priority() {
        assert_eq!(HeaderValue::from(Priority::High), "10");
        assert_eq!(HeaderValue::from(Priority::Low), "5");
    }

    #[test]
    fn the_largest_payload_fits_in_four_kilobytes() {
        let id = "x".repeat(254);
        let n = notification(&format!(
            r#"{{"event_id":"${id}","room_id":"!{id}","type":"m.room.message",
                "room_name":"{}","sender":"@{id}","content":{{"body":"{}"}},
                "counts":{{"unread":18446744073709551615}},{DEVICE}}}"#,
            "名".repeat(500),
            "\u{1F600}".repeat(2000),
        ));
        let r = Requests::from(&n).for_device(Sound::Default);
        assert!(r.body.len() <= 4096, "{} bytes", r.body.len());
    }

    #[test]
    fn the_largest_payload_fits_even_when_every_char_needs_escaping() {
        let id = "x".repeat(254);
        let n = notification(&format!(
            r#"{{"event_id":"${id}","room_id":"!{id}","type":"m.room.message",
                "room_name":"{}","sender":"@{id}","content":{{"body":"{}"}},
                "counts":{{"unread":18446744073709551615}},{DEVICE}}}"#,
            "\\\"".repeat(300),
            "\\u0001\\n".repeat(2000),
        ));
        let r = Requests::from(&n).for_device(Sound::Default);
        assert!(r.body.len() <= 4096, "{} bytes", r.body.len());
    }
}
