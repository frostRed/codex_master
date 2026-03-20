# Caddy Browser Auth

This relay server can trust browser identity headers injected by Caddy.

## Relay Server Environment

Set these on the relay server when you want browser auth enabled:

```bash
export RELAY_SERVER_BIND=127.0.0.1:8080
export RELAY_SERVER_BROWSER_AUTH_REQUIRED=true
export RELAY_SERVER_BROWSER_AUTH_USER_HEADER=X-Relay-User
export RELAY_SERVER_BROWSER_AUTH_PROXY_SECRET_HEADER=X-Relay-Auth-Secret
export RELAY_SERVER_BROWSER_AUTH_PROXY_SECRET='replace-with-a-random-secret'
```

Notes:

- Keep the relay server bound to localhost when Caddy is in front of it.
- `RELAY_SERVER_BROWSER_AUTH_PROXY_SECRET` is optional, but recommended. When set, the relay server rejects browser requests that do not include the expected shared secret header from Caddy.
- Home client auth remains separate and still uses `RELAY_SERVER_CLIENT_TOKEN`.

## Home Client

Home client traffic is unchanged by browser auth. Keep using:

```bash
export HOME_CLIENT_TRANSPORT=relay
export HOME_CLIENT_RELAY_URL=ws://127.0.0.1:8080/ws/client
export HOME_CLIENT_AUTH_TOKEN='client-token'
```

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
