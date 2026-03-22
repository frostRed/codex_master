# Upstream Browser Auth

This relay server can trust browser identity headers injected by a reverse proxy or upstream gateway (Caddy, Nginx, app gateway, etc.).

## Relay Server Environment

Set these on the relay server:

```bash
export RELAY_SERVER_BIND=127.0.0.1:8080
export RELAY_SERVER_CLIENT_TOKENS='token-v1,token-v2'
export RELAY_SERVER_CLIENT_TOKENS_REVOKED=''
export RELAY_SERVER_BROWSER_AUTH_REQUIRED=true
export RELAY_SERVER_BROWSER_AUTH_USER_HEADER=X-Relay-User
export RELAY_SERVER_BROWSER_AUTH_PROXY_SECRET_HEADER=X-Relay-Auth-Secret
export RELAY_SERVER_BROWSER_AUTH_PROXY_SECRET='replace-with-a-random-secret'
export RELAY_SERVER_BROWSER_ALLOWED_ORIGINS='https://relay.example.com'
export RELAY_SERVER_BROWSER_ALLOW_ANONYMOUS=false
```

Notes:

- Keep the relay server bound to localhost when a reverse proxy is in front of it.
- In strict mode (default), client tokens, browser auth, proxy secret, and browser origin allowlist are required.
- `RELAY_SERVER_ALLOW_INSECURE_DEV=true` can relax auth checks only for loopback local development; it is rejected for non-loopback bind addresses.
- Token revocation takes precedence over allowlist when a token appears in both `RELAY_SERVER_CLIENT_TOKENS` and `RELAY_SERVER_CLIENT_TOKENS_REVOKED`.

## Home Client

Home client traffic is unchanged by browser auth. Use the relay URL over TLS:

```bash
export HOME_CLIENT_TRANSPORT=relay
export HOME_CLIENT_RELAY_URL=wss://relay.example.com/ws/client
export HOME_CLIENT_AUTH_TOKEN='token-v2'
```

Optional local-development override (loopback only):

```bash
export HOME_CLIENT_ALLOW_INSECURE_WS=true
export HOME_CLIENT_RELAY_URL=ws://127.0.0.1:8080/ws/client
```

Optional reconnect tuning:

```bash
export HOME_CLIENT_RECONNECT_DELAY_SECS=3
export HOME_CLIENT_RECONNECT_MAX_DELAY_SECS=60
export HOME_CLIENT_RECONNECT_JITTER_MILLIS=750
export HOME_CLIENT_RECONNECT_RESET_AFTER_SECS=30
```

## No Caddy Basic Auth

If your own application gateway already authenticates users, you can remove Caddy `basic_auth`.

Requirements still remain:

- gateway must inject trusted user header (`RELAY_SERVER_BROWSER_AUTH_USER_HEADER`)
- gateway must inject proxy secret header (`RELAY_SERVER_BROWSER_AUTH_PROXY_SECRET_HEADER`)
- browser websocket `Origin` must match `RELAY_SERVER_BROWSER_ALLOWED_ORIGINS`
- relay server should stay on loopback and only be reachable through the proxy

## Browser Flow

When browser auth is enabled:

- `GET /`
- `GET /manifest.webmanifest`
- `GET /sw.js`
- `GET /favicon.svg`
- `GET /icons/*`
- `GET /ws/browser`

all require the trusted browser auth headers.

The relay server stores the authenticated browser user id on newly created relay sessions so session lists and adoption are no longer globally shared across authenticated users.

Legacy persisted sessions without an owner remain visible until one authenticated user adopts them, at which point ownership is claimed.
