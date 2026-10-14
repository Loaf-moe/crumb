//! crumb end to end: HTTP in, APNs (a fake one) out.

mod support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::StatusCode;
use serde_json::json;
use support::{APP_ID, FakeApns, Reply, call, event, gateway, notify};

const DEADLINE: Duration = Duration::from_secs(5);

fn rejected(body: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = serde_json::from_value(body["rejected"].clone()).unwrap();
    keys.sort();
    keys
}

#[tokio::test]
async fn an_event_reaches_apns_with_the_app_contract() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);

    let (status, body) = notify(&app, &event("$ev", &["aa11"])).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "rejected": [] })));

    let seen = apns.seen();
    assert_eq!(seen.len(), 1);
    let push = &seen[0];
    assert_eq!(push.token, "aa11");
    assert_eq!(
        push.body,
        json!({
            "aps": {
                "alert": { "title": "Loaf Chat", "body": "new message" },
                "mutable-content": 1,
                "thread-id": "!r:loaf.moe",
                "badge": 1,
            },
            "room_id": "!r:loaf.moe",
            "event_id": "$ev",
        })
    );
    let h = &push.headers;
    assert!(
        h["authorization"]
            .to_str()
            .unwrap()
            .starts_with("bearer ey")
    );
    assert_eq!(h["apns-topic"], APP_ID);
    assert_eq!(h["apns-push-type"], "alert");
    assert_eq!(h["apns-priority"], "10");
    assert_eq!(h["apns-collapse-id"], "$ev");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires: u64 = h["apns-expiration"].to_str().unwrap().parse().unwrap();
    assert!((now + 86_000..=now + 86_500).contains(&expires));
}

#[tokio::test]
async fn dead_tokens_unknown_apps_and_non_hex_pushkeys_are_rejected() {
    let apns = FakeApns::start().await;
    apns.script("aa01", [Reply::Error(410, "Unregistered")]);
    apns.script("aa02", [Reply::Error(400, "BadDeviceToken")]);
    apns.script("aa03", [Reply::Error(400, "DeviceTokenNotForTopic")]);
    let app = gateway(&apns, DEADLINE);

    let body = r#"{"notification":{"event_id":"$e","devices":[
        {"app_id":"moe.loaf.chat.ios","pushkey":"aa01"},
        {"app_id":"moe.loaf.chat.ios","pushkey":"aa02"},
        {"app_id":"moe.loaf.chat.ios","pushkey":"aa03"},
        {"app_id":"moe.loaf.chat.ios","pushkey":"aa04"},
        {"app_id":"com.example.other","pushkey":"aa05"},
        {"app_id":"moe.loaf.chat.ios","pushkey":"not-hex"}]}}"#;
    let (status, body) = notify(&app, body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rejected(&body), ["aa01", "aa02", "aa03", "aa05", "not-hex"]);
    // The unknown app and the bad pushkey never reach APNs.
    assert_eq!(apns.seen().len(), 4);
}

#[tokio::test]
async fn a_transient_failure_asks_for_a_retry_that_skips_delivered_devices() {
    let apns = FakeApns::start().await;
    apns.script("bb02", [Reply::Error(500, "InternalServerError")]);
    let app = gateway(&apns, DEADLINE);
    let body = event("$ev", &["bb01", "bb02"]);

    let (status, error) = notify(&app, &body).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(error["errcode"], "M_UNKNOWN");

    let (status, body) = notify(&app, &body).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "rejected": [] })));
    assert_eq!(
        apns.seen_for("bb01").len(),
        1,
        "the retry must not ping bb01 twice"
    );
    assert_eq!(apns.seen_for("bb02").len(), 2);
}

#[tokio::test]
async fn too_many_requests_is_transient() {
    let apns = FakeApns::start().await;
    apns.script("cc01", [Reply::Error(429, "TooManyRequests")]);
    let app = gateway(&apns, DEADLINE);
    let (status, _) = notify(&app, &event("$ev", &["cc01"])).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_stalled_apns_misses_the_deadline_and_asks_for_a_retry() {
    let apns = FakeApns::start().await;
    apns.script("dd01", [Reply::Stall]);
    let app = gateway(&apns, Duration::from_millis(200));
    let (status, _) = notify(&app, &event("$ev", &["dd01"])).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn many_devices_share_one_connection_and_one_token() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let tokens: Vec<String> = (0..50).map(|i| format!("{i:04x}")).collect();
    let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();

    let (status, _) = notify(&app, &event("$ev1", &refs)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = notify(&app, &event("$ev2", &refs)).await;
    assert_eq!(status, StatusCode::OK);

    let seen = apns.seen();
    assert_eq!(seen.len(), 100);
    assert_eq!(apns.connections(), 1);
    let bearer = &seen[0].headers["authorization"];
    assert!(seen.iter().all(|s| s.headers["authorization"] == bearer));
}

#[tokio::test]
async fn an_expired_provider_token_is_renewed_and_retried_once() {
    let apns = FakeApns::start().await;
    apns.script("ee01", [Reply::Error(403, "ExpiredProviderToken")]);
    apns.script(
        "ee02",
        [
            Reply::Error(403, "InvalidProviderToken"),
            Reply::Error(403, "InvalidProviderToken"),
        ],
    );
    let app = gateway(&apns, DEADLINE);

    let (status, _) = notify(&app, &event("$a", &["ee01"])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(apns.seen_for("ee01").len(), 2);

    let (status, _) = notify(&app, &event("$b", &["ee02"])).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(apns.seen_for("ee02").len(), 2, "one retry, not a loop");
}

#[tokio::test]
async fn a_counts_only_update_is_a_bare_badge() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let body = r#"{"notification":{"counts":{"unread":3},"prio":"low",
        "devices":[{"app_id":"moe.loaf.chat.ios","pushkey":"ff01","tweaks":{"sound":"default"}}]}}"#;

    let (status, _) = notify(&app, body).await;
    assert_eq!(status, StatusCode::OK);
    let seen = apns.seen();
    assert_eq!(seen[0].body, json!({ "aps": { "badge": 3 } }));
    assert_eq!(seen[0].headers["apns-priority"], "5");
    assert!(seen[0].headers.get("apns-collapse-id").is_none());

    // Counts-only updates are idempotent: a repeat is sent, not suppressed.
    notify(&app, body).await;
    assert_eq!(apns.seen().len(), 2);
}

#[tokio::test]
async fn a_sound_tweak_plays_the_default_sound() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let body = r#"{"notification":{"event_id":"$e","devices":[
        {"app_id":"moe.loaf.chat.ios","pushkey":"ab01","tweaks":{"sound":"default"}},
        {"app_id":"moe.loaf.chat.ios","pushkey":"ab02"}]}}"#;
    notify(&app, body).await;
    assert_eq!(apns.seen_for("ab01")[0].body["aps"]["sound"], "default");
    assert!(apns.seen_for("ab02")[0].body["aps"].get("sound").is_none());
}

#[tokio::test]
async fn malformed_bodies_get_matrix_errors() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, Duration::from_secs(5));

    let (status, body) = notify(&app, "nope").await;
    assert_eq!(
        (status, &body["errcode"]),
        (StatusCode::BAD_REQUEST, &json!("M_NOT_JSON"))
    );

    let (status, body) = notify(&app, r#"{"notification":{}}"#).await;
    assert_eq!(
        (status, &body["errcode"]),
        (StatusCode::BAD_REQUEST, &json!("M_BAD_JSON"))
    );
    assert!(apns.seen().is_empty());
}

#[tokio::test]
async fn unknown_routes_and_methods_are_m_unrecognized() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, Duration::from_secs(5));

    let (status, body) = call(&app, "GET", "/_matrix/push/v1/nope", "").await;
    assert_eq!(
        (status, &body["errcode"]),
        (StatusCode::NOT_FOUND, &json!("M_UNRECOGNIZED"))
    );

    let (status, body) = call(&app, "GET", "/_matrix/push/v1/notify", "").await;
    assert_eq!(
        (status, &body["errcode"]),
        (StatusCode::METHOD_NOT_ALLOWED, &json!("M_UNRECOGNIZED"))
    );
}

#[tokio::test]
async fn health_is_ok() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, Duration::from_secs(5));
    let (status, body) = call(&app, "GET", "/health", "").await;
    assert_eq!((status, body), (StatusCode::OK, json!({})));
}

#[tokio::test]
async fn a_push_after_apns_drops_the_connection_reconnects() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let (status, _) = notify(&app, &event("$a", &["ac01"])).await;
    assert_eq!(status, StatusCode::OK);

    apns.drop_connections();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (status, _) = notify(&app, &event("$b", &["ac01"])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(apns.connections(), 2);
}

#[tokio::test]
async fn more_devices_than_apns_allows_streams_all_get_through() {
    let apns = FakeApns::start_with_streams(Some(8)).await;
    let app = gateway(&apns, DEADLINE);
    let tokens: Vec<String> = (0..100).map(|i| format!("{i:04x}")).collect();
    let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();

    let (status, body) = notify(&app, &event("$ev", &refs)).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "rejected": [] })));
    assert_eq!(apns.seen().len(), 100);
}

#[tokio::test]
async fn an_event_id_unfit_for_a_header_is_sent_without_a_collapse_id() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let (status, _) = notify(&app, &event("$évènement", &["ad01"])).await;
    assert_eq!(status, StatusCode::OK);
    let seen = apns.seen();
    assert!(seen[0].headers.get("apns-collapse-id").is_none());
    assert_eq!(seen[0].body["event_id"], "$évènement");
}

/// Real time on purpose: paused tokio time auto-advances while the runtime
/// waits on real sockets, firing the deadline and ping timers mid-request.
#[tokio::test]
#[ignore = "takes about three minutes; run with `mise run test:slow`"]
async fn an_idle_connection_outlives_the_pool_idle_timeout() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let (status, _) = notify(&app, &event("$a", &["ae01"])).await;
    assert_eq!(status, StatusCode::OK);

    // hyper-util's pool reaper ticks every 90 s and evicts what has idled
    // longer than that, so an unfixed client loses the connection by 180 s.
    tokio::time::sleep(Duration::from_secs(185)).await;

    let (status, _) = notify(&app, &event("$b", &["ae01"])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        apns.connections(),
        1,
        "the connection must persist while idle"
    );
}

#[tokio::test]
async fn a_payload_apns_calls_too_large_settles_instead_of_looping() {
    let apns = FakeApns::start().await;
    apns.script("af01", [Reply::Error(413, "PayloadTooLarge")]);
    let app = gateway(&apns, DEADLINE);
    let (status, body) = notify(&app, &event("$ev", &["af01"])).await;
    // Retrying can't shrink the payload, and the pushkey is still good.
    assert_eq!((status, body), (StatusCode::OK, json!({ "rejected": [] })));
}

#[tokio::test]
async fn a_device_named_twice_in_one_request_is_pushed_once() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let (status, _) = notify(&app, &event("$ev", &["ba01", "ba01"])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(apns.seen_for("ba01").len(), 1);
}

#[tokio::test]
async fn a_request_naming_too_many_devices_is_refused_before_any_push() {
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let tokens: Vec<String> = (0..101).map(|i| format!("{i:04x}")).collect();
    let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let (status, body) = notify(&app, &event("$ev", &refs)).await;
    assert_eq!(
        (status, &body["errcode"]),
        (StatusCode::BAD_REQUEST, &json!("M_BAD_JSON"))
    );
    assert!(apns.seen().is_empty());
}

#[tokio::test]
async fn a_hostile_app_id_cannot_forge_a_log_line() {
    let logs = support::capture_logs();
    let apns = FakeApns::start().await;
    let app = gateway(&apns, DEADLINE);
    let body = r#"{"notification":{"event_id":"$e","devices":[
        {"app_id":"x\nFORGED INFO crumb: all good","pushkey":"aa"}]}}"#;
    notify(&app, body).await;

    let text = logs.text();
    assert!(text.contains("unknown app_id"), "{text}");
    assert!(!text.lines().any(|l| l.starts_with("FORGED")), "{text}");
}
