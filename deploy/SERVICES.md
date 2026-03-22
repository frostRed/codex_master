# Service Templates

This directory contains starter templates for:

- `systemd`:
  - `deploy/systemd/codex-relay.service`
  - `deploy/systemd/codex-home-client.service`
- `launchd`:
  - `deploy/launchd/com.codex.relay-server.plist`
  - `deploy/launchd/com.codex.home-client.plist`
- environment files:
  - `deploy/env/relay.env.example`
  - `deploy/env/home-client.env.example`

## Notes

- Replace placeholder paths (`REPLACE_ME`, `/srv/codex/...`, `/usr/local/bin/...`) before enabling services.
- Relay server is designed to run on `127.0.0.1:8080` behind Caddy TLS termination.
- Keep strict auth enabled in production:
  - `RELAY_SERVER_ALLOW_INSECURE_DEV=false`
  - `HOME_CLIENT_ALLOW_INSECURE_WS=false`
