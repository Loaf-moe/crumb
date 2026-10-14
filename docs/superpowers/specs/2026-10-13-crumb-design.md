# crumb: a Matrix push gateway for Loaf Chat

**Date:** 2026-10-13
**Status:** design, awaiting review

## Purpose

crumb is a small Rust service that implements the
[Matrix Push Gateway API](https://spec.matrix.org/latest/push-gateway-api/)
and forwards notifications to Apple's APNs for the Loaf Chat iOS app. It
replaces Sygnal, which is too heavy for one app.

Every homeserver posts to one public URL:
`https://push.matrix.loaf.moe/_matrix/push/v1/notify`. The app has this URL
as a constant. Only the holder of the app's APNs key can push to the app,
which is why the gateway is central rather than per-homeserver.

**Success** means a release (TestFlight) build of Loaf Chat receives a push
through crumb from a real homeserver, and the extension rewrites it.

**Rule:** crumb implements everything the Push Gateway API defines. Anything
not in the spec is listed under "Beyond the spec" with its reason.

## Decisions

| Topic | Decision |
|---|---|
| Repo | `~/code/crumb`, one binary crate `crumb` |
| Server | `axum` on `hyper` |
| APNs client | Hand-rolled on `hyper-util` + `hyper-rustls` (ring, webpki roots), HTTP/2 only. `a2` was rejected: no release since 2024-05 and OpenSSL by default |
| JWT | Hand-rolled ES256 with `p256`. No JWT crate |
| Deploy | Podman container behind Traefik. Traefik terminates TLS; crumb speaks plain HTTP |
| Config | Keyed by `app_id`, like Sygnal |
| Observability | `GET /health` and structured `tracing` logs on stdout. No metrics |
| Delivery model | Synchronous, with no queue. The `200` response must name rejected pushkeys, so crumb waits for APNs. Retrying is the homeserver's job (the spec says it SHOULD retry HTTP errors with exponential backoff) |

**Dependencies** (complete list): `tokio`, `axum`, `hyper`, `hyper-util`,
`hyper-rustls`, `serde`, `serde_json`, `toml`, `p256`, `base64`, `sha2`,
`tracing`, `tracing-subscriber`. Dev-only: none beyond these; the fake APNs
uses `hyper`'s server.

## Configuration

The config is TOML, and its path is in `CRUMB_CONFIG`.

```toml
listen = "0.0.0.0:8080"          # optional, this is the default

[apps."moe.loaf.chat.ios"]
key_path = "/run/secrets/apns.p8" # the .p8 auth key, mounted read-only
key_id = "ABC123DEFG"             # the key's id from developer.apple.com
team_id = "6W2A5N37N3"
topic = "moe.loaf.chat.ios"
endpoint = "production"           # "production" | "sandbox"
```

- At startup crumb reads and parses every key. A missing, unreadable or
  malformed key, an empty `apps` table, or an unknown `endpoint` stops
  startup with an error that names the app and the field, never the key's
  contents.
- `production` is `https://api.push.apple.com`; `sandbox` is
  `https://api.sandbox.push.apple.com`. There is no free-form endpoint in the
  config.

## Modules

Each module has one job and can be tested on its own.

| Module | Job | Depends on |
|---|---|---|
| `config` | Load and validate the TOML; load each `.p8` into a `p256::SigningKey` | `toml`, `p256` |
| `matrix` | Serde types for the notify request and response; the Matrix error body | `serde` |
| `payload` | Pure function: a notification + device → APNs headers and JSON body | `matrix` |
| `jwt` | Holds one provider token per app. Issues a cached token, renews it after 40 minutes, and renews on demand after `ExpiredProviderToken`. The clock is injected | `p256`, `base64` |
| `apns` | One client per app over a persistent, multiplexed HTTP/2 connection. `send(device_token, request) -> Outcome` | `hyper*`, `jwt` |
| `dedupe` | Remembers delivered `(event_id, pushkey)` pairs for 15 minutes | — |
| `dispatch` | For one notify request: fans out to devices, applies dedupe, merges the outcomes into a response | `apns`, `payload`, `dedupe` |
| `server` | axum routes, the error mapping and the fallback | `dispatch` |
| `log` | `pk=<8 hex>`: the first 8 hex characters of SHA-256(pushkey) | `sha2` |

## Request handling (spec coverage)

### Routes

| Route | Behaviour |
|---|---|
| `POST /_matrix/push/v1/notify` | The gateway |
| `GET /health` | `200 {}` |
| Unknown path | `404 {"errcode":"M_UNRECOGNIZED","error":"Unrecognized request"}` |
| Known path, wrong method | `405` with the same body |

The endpoint needs no authentication and has no rate limit, as the spec says.

### Parsing

- The body must be JSON (`400 M_NOT_JSON` otherwise). It must contain
  `notification.devices[]`, and each device must have `app_id` and `pushkey`
  (`400 M_BAD_JSON` otherwise). The spec defines only `200` for this
  endpoint. The `400` follows the general Matrix error convention, and a
  homeserver that retries it does no harm.
- Every other field the spec defines is parsed: `event_id`, `room_id`,
  `type`, `sender`, `sender_display_name`, `room_name`, `room_alias`,
  `user_is_target`, `prio`, `content`, `counts.unread`, `counts.missed_calls`,
  and on each device `data` (including `format`), `pushkey_ts` and `tweaks`.
  Unknown fields are ignored.

### Per device

In order:

1. `app_id` is not in the config → the pushkey goes in `rejected`.
2. The pushkey is not an even-length hex string → `rejected`, without
   calling APNs.
3. The notification has an `event_id` and `dedupe` already holds
   `(event_id, pushkey)` → it counts as delivered and nothing is sent.
4. Otherwise, build the payload and send it.

Devices are sent concurrently over the app's shared connection.

### The APNs request

**Headers:**

| Header | Value |
|---|---|
| `authorization` | `bearer <provider JWT>` |
| `apns-topic` | the app's `topic` |
| `apns-push-type` | `alert` |
| `apns-priority` | `10` for `prio: high` or absent; `5` for `prio: low` |
| `apns-expiration` | now + 24 hours (Unix seconds) |
| `apns-collapse-id` | the `event_id`, when present and at most 64 bytes |

**Event notification** (`event_id` present):

```json
{
  "aps": {
    "alert": { "title": "<title>", "body": "<body>" },
    "mutable-content": 1,
    "thread-id": "<room_id>",
    "badge": 3,
    "sound": "default"
  },
  "room_id": "<room_id>",
  "event_id": "<event_id>"
}
```

- `room_id`, `event_id` and `aps.mutable-content: 1` are always set when the
  `event_id` is present. The app's Notification Service Extension reads the
  two top-level keys, fetches and decrypts the event, and rewrites the alert.
- When `room_id` is absent, `thread-id` and the top-level `room_id` are left
  out.
- `aps.badge` is `counts.unread + counts.missed_calls`. A missing count is 0,
  since the spec says zero counts are omitted.
- `aps.sound` is `"default"` only when the device's `tweaks.sound` is present.
  Otherwise there's no sound key.
- **Title and body**, the fallback the user sees if the extension doesn't
  run:
  - With `event_id_only` (Loaf Chat's format), nothing else is sent, so the
    alert is `Loaf Chat` / `new message`.
  - For other formats:
    - **Title:** `room_name`, else `room_alias`, else `sender_display_name`,
      else `sender`, else `Loaf Chat`.
    - **Body for `m.room.message`** with a string `content.body`: when the
      title is a room name or alias, the body is
      `<sender_display_name or sender>: <body>`. Otherwise it is `<body>`.
    - **Body for `m.room.member`** with `content.membership == "invite"` and
      `user_is_target == true`: `<sender_display_name or sender> invited you`.
    - **Any other case**, including `m.room.encrypted`: `new message`.
  - Bodies are truncated so the payload stays within APNs's 4 KB limit.

**Counts-only update** (no `event_id`):

```json
{ "aps": { "badge": 0 } }
```

The badge is the sum above. It has no alert and no collapse id, and it skips
dedupe, because count updates are idempotent (the spec only requires dedupe
for event notifications).

### APNs outcomes

| APNs answer | Outcome |
|---|---|
| `200` | Delivered. Record `(event_id, pushkey)` in dedupe if there's an `event_id` |
| `410` (any reason), `400 BadDeviceToken`, `400 DeviceTokenNotForTopic` | Rejected |
| `403 ExpiredProviderToken` or `InvalidProviderToken` | Renew the JWT and retry once. A second failure is Transient |
| Anything else: other 4xx/5xx, `429`, connection error, the deadline | Transient |

### The response

- The whole fan-out runs under a 10-second deadline.
- If **any** device is Transient → `502 {"errcode":"M_UNKNOWN","error":"..."}`.
  The homeserver retries. Dedupe means devices that already succeeded aren't
  pushed twice, and the collapse id is a second line of defence on the
  phone.
- Otherwise → `200 {"rejected": [...]}` with every Rejected pushkey (empty
  if none).

### Duplicate suppression

- The spec requires dedupe on `event_id` for event notifications.
  `dedupe` is an in-memory map from `(event_id, pushkey)` to the time it was
  delivered.
- Entries expire after 15 minutes. That's longer than a sensible homeserver
  backoff window, so retries are covered.
- The map is capped at 100,000 entries, evicting the oldest first, and is
  pruned as a side effect of inserts.
- A restart clears it. The only consequence is that a retry spanning a
  restart may push once more, and the collapse id still replaces the banner.

## APNs connection and JWT

- **One HTTP/2 connection per app.** It's built with `hyper-util`'s pooled
  client, HTTP/2 only, with ALPN `h2` over rustls. Concurrent requests share
  it as streams. HTTP/2 keep-alive pings run every 60 seconds so idle
  connections are noticed, and a dropped connection is reopened on the next
  request.
- **The JWT** has header `{"alg":"ES256","kid":<key_id>}` and claims
  `{"iss":<team_id>,"iat":<now>}`. The signature is the raw 64-byte `r||s`.
  - The token is reused until it is 40 minutes old, which is inside Apple's
    20–60 minute window and avoids `TooManyProviderTokenUpdates`.
  - It is renewed early on `ExpiredProviderToken` / `InvalidProviderToken`,
    at most once per minute.
  - Renewal is serialised behind a mutex, so concurrent requests share one
    renewal.

## Logging and secrets

- **Logged:** each request's device count; each device as `pk=<8 hex of
  SHA-256(pushkey)>` with its `app_id` and outcome; the APNs status, `reason`
  and `apns-id`; JWT renewals; connection errors.
- **Never logged:** raw pushkeys, JWTs, key material, `content`, or any
  display field (names, aliases, bodies).
- The `.p8` path is logged at startup. The key's contents are never logged.

## Testing

All tests are written before the code they cover (TDD).

**Unit tests:**
- `matrix`: valid bodies, missing `notification` or `devices`, non-JSON,
  unknown fields.
- `payload`: a table test over the format, title and body cases, badge
  arithmetic, sound tweak, `prio`, collapse-id length guard, absent
  `room_id`, counts-only, truncation.
- Outcome mapping: a table test from status plus `reason` to Outcome.
- `jwt`:
  - A key generated in the test signs a token; `p256` verifies it.
  - A fake clock checks reuse before 40 minutes, renewal after, and forced
    renewal with a one-per-minute floor.
- `dedupe`: expiry, the cap and eviction.
- `log`: the hash is stable and a raw pushkey never appears in it.

**Integration tests with a fake APNs:**
- The fake is an in-process hyper HTTP/2 server (h2c). The `apns` client has
  a crate-internal constructor that takes a base URI, used only by tests.
  The config can still only name `production` or `sandbox`.
- The fake records headers and bodies, and can be scripted per pushkey to
  answer `200`, `410`, `400 BadDeviceToken`, `403 ExpiredProviderToken`,
  `500` or a stall.
- The axum router is driven end to end over it, covering:
  - The `rejected` list.
  - A `502` on Transient, then a retry that skips already-delivered devices.
  - The deadline.
  - Many devices multiplexed over one connection.
  - One JWT reused across requests, and renewal after
    `ExpiredProviderToken`.
  - `404`/`405` `M_UNRECOGNIZED`.
  - `/health`.

**Smoke test:** the README includes a sample body and a Nushell
`http post` command.

**End to end:** a real push to a TestFlight build once the app side and the
deployment are in place, run with Chris.

## Deployment (crumb's side)

- **`Containerfile`:** a multi-stage build that produces a static
  `x86_64-unknown-linux-musl` binary on a `scratch` base, plus CA roots,
  exposing 8080.
- **The README documents:**
  - `CRUMB_CONFIG`, the config format, and mounting the `.p8` read-only.
  - `/health` for Podman/Traefik health checks.
  - That Traefik must route `push.matrix.loaf.moe` `PathPrefix(/_matrix/push/)`
    to crumb and terminate TLS.
- crumb ships no Traefik or quadlet config. Chris wires that up on uwutah.

## CI

If crumb gets a GitHub remote, one Linux runner runs `cargo fmt --check`,
`cargo clippy -- -D warnings` and `cargo test`, finishing in under 5 minutes.
Nothing else gets added without asking.

## Beyond the spec

- **APNs only.** No Firebase or other providers.
- **No metrics.** Logs and `/health` only.
- **No queue or persistence.** Retrying is the homeserver's job, as the
  spec says.

## Note for the app session

The handoff said "don't set a sound." Following the spec's `tweaks`, crumb
now sets `aps.sound: "default"` when the push rule asks for a sound. If the
extension fails, the fallback alert plays the default sound. When the
extension runs, its chime replaces it. It also sets `aps.badge` from
`counts`, so the app no longer owns the badge number.
