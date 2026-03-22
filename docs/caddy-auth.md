# Relay Browser Authentication

Relay browser access now uses relay-managed login sessions only:

- relay serves `GET /login` and `POST /auth/login`
- relay sets an HttpOnly session cookie after successful login
- unauthenticated browser access to `/` is redirected to `/login`
- unauthenticated `GET /ws/browser` is rejected with `401`

Proxy-injected identity-header auth is no longer used by relay.

## Relay Server Environment

Set these values on the relay server:

```bash
export RELAY_SERVER_BIND=127.0.0.1:8080
export RELAY_SERVER_CLIENT_TOKENS='token-v1,token-v2'
export RELAY_SERVER_CLIENT_TOKENS_REVOKED=''
export RELAY_SERVER_BROWSER_ALLOWED_ORIGINS='https://relay.example.com'

export RELAY_SERVER_LOGIN_USERNAME='admin'
export RELAY_SERVER_LOGIN_PASSWORD='replace-with-a-strong-password'
export RELAY_SERVER_LOGIN_SESSION_TTL_SECS=86400
export RELAY_SERVER_LOGIN_COOKIE_NAME='relay_session'
export RELAY_SERVER_LOGIN_COOKIE_SECURE=true
```

Notes:

- Keep the relay server bound to localhost when a reverse proxy is in front of it.
- In strict mode (default), client token allowlist and browser origin allowlist are required.
- `RELAY_SERVER_ALLOW_INSECURE_DEV=true` can relax auth checks only for loopback local development; it is rejected for non-loopback bind addresses.
- Token revocation takes precedence over allowlist when a token appears in both `RELAY_SERVER_CLIENT_TOKENS` and `RELAY_SERVER_CLIENT_TOKENS_REVOKED`.

## Home Client

Home client traffic is unchanged by browser login auth. Use the relay URL over TLS:

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

## Caddy Integration

Caddy only needs to terminate TLS and proxy to relay. Do not inject identity headers for relay auth.

Optional `basic_auth` in Caddy can still be used as an extra outer layer, but relay login remains required for browser session access.

## Browser Flow

When browser auth is enabled:

- `GET /`
- `GET /manifest.webmanifest`
- `GET /sw.js`
- `GET /favicon.svg`
- `GET /icons/*`
- `GET /ws/browser`

all require a valid relay login session cookie.

The relay server stores the authenticated login user id on newly created relay sessions so session lists and adoption are no longer globally shared across users.

Legacy persisted sessions without an owner remain visible until one authenticated user adopts them, at which point ownership is claimed.
