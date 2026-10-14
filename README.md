# crumb

A [Matrix push gateway](https://spec.matrix.org/latest/push-gateway-api/)
for Loaf Chat. Homeservers POST notifications to it, and it forwards them to
Apple's APNs over one persistent HTTP/2 connection per app.

Loaf Chat's pushers point at
`https://push.matrix.loaf.moe/_matrix/push/v1/notify`.

## Configure

crumb reads a TOML file named by `CRUMB_CONFIG`. See
[`crumb.example.toml`](crumb.example.toml). At startup it checks every app
and reads every key, and exits with a message naming the app and the field if
anything is wrong.

The `.p8` key is a secret. Mount it read-only. crumb never logs it, never
logs a pushkey (only `pk=` plus 8 hex characters of its SHA-256), and never
logs message content.

## Run

```nu
CRUMB_CONFIG=crumb.toml cargo run --release
```

`RUST_LOG` overrides the log filter, which defaults to `crumb=info`.

## Container

```nu
podman build -t crumb .
```

Then run it as follows. The paths are examples.

```nu
(podman run --rm -p 8080:8080
  -v ./crumb.toml:/etc/crumb.toml:ro
  -v ./AuthKey.p8:/run/secrets/apns.p8:ro
  -e CRUMB_CONFIG=/etc/crumb.toml
  crumb)
```

The image is distroless and runs as a non-root user. It listens on 8080 and
speaks plain HTTP.

## Behind Traefik

Route `Host(push.matrix.loaf.moe) && PathPrefix(/_matrix/push/)` to the
container's port 8080, and terminate TLS there. The endpoint needs no
authentication and has no rate limit, as the spec says; add a Traefik
rate-limit middleware if that ever matters.

`GET /health` answers `200 {}`, for a Traefik health check. There is no shell
in the image, so check health from Traefik, not from inside the container.

## Smoke test

```nu
(http post --content-type application/json
  http://localhost:8080/_matrix/push/v1/notify
  { notification: { event_id: '$smoke', room_id: '!smoke:example.org',
    devices: [{ app_id: 'moe.loaf.chat.ios', pushkey: '00' }] } })
```

With a real device token as the `pushkey`, the phone shows a notification.
With `00`, APNs refuses the token, and the answer is `{"rejected": ["00"]}`.

## Develop

```nu
mise run test
mise run check
```

The tests run against a fake APNs: an in-process HTTP/2 server.

## Licence

AGPL-3.0-only. See `LICENSE`.
