# Plans

## 2026-03-20 Caddy Browser Authentication

- Goal: protect browser/PWA access behind Caddy-authenticated requests while teaching the relay server to trust and enforce authenticated browser identity.
- Scope:
  - add relay-server browser auth configuration for trusted user headers and optional proxy shared secret
  - require authenticated browser identity on HTTP shell routes and browser websocket upgrades
  - scope relay sessions to authenticated browser users so session lists and adoption are not globally shared
  - add a Caddy deployment example for the authenticated reverse-proxy setup
- Invariants:
  - home client auth remains separate and still uses the existing client token
  - browser auth should be optional for local development when Caddy is not in front
  - the relay server must not trust arbitrary spoofed browser identity when a proxy secret is configured
  - health checks and client websocket connectivity should keep working
- Likely files/modules to change:
  - `PLANS.md`
  - `src/protocol.rs`
  - `src/relay_server.rs`
  - `src/relay_state.rs`
  - `docs/caddy-auth.md`
  - `deploy/Caddyfile.example`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - inspect the relay browser routes for unauthorized vs authenticated flow
  - confirm the Caddy example forwards both browser identity and proxy secret headers
- Main risks:
  - locking out the PWA shell if auth is enforced too aggressively on static assets
  - leaving legacy persisted sessions inaccessible or globally visible after user scoping is introduced
  - documenting a Caddy pattern that drifts from the relay server's actual header expectations

## 2026-03-22 Auth Hardening, TLS Enforcement, Reconnect Backoff, And XSS Fixes

Status: completed

- Goal: harden production readiness by closing obvious auth gaps, enforcing secure relay transport defaults, improving reconnect behavior, and removing DOM XSS sinks in the relay console.
- Scope:
  - enforce secure relay URL defaults (`wss://`) for the home client while keeping an explicit local-development override
  - harden relay-server auth defaults so browser auth and client token checks are required unless explicitly disabled for local development
  - add exponential backoff with jitter for home-client reconnect and browser websocket auto-reconnect
  - replace `innerHTML` render paths that interpolate runtime data with safe DOM text-node rendering
  - refresh docs/examples for new auth and transport environment knobs
- Invariants:
  - Caddy remains the TLS terminator and relay server can stay bound to localhost
  - home-client and browser auth checks remain separate concerns
  - relay runtime/session protocol behavior remains backward-compatible beyond transport/auth validation
  - reconnect loops must not create tight retry storms when network is unstable
- Likely files/modules to change:
  - `PLANS.md`
  - `src/config.rs`
  - `src/main.rs`
  - `src/relay_server.rs`
  - `src/relay_console.html`
  - `docs/caddy-auth.md`
  - `deploy/Caddyfile.example`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - inspect browser websocket reconnect behavior and confirm dynamic list rendering no longer uses `innerHTML` for untrusted data
- Main risks:
  - making secure defaults too strict and surprising existing local/dev usage
  - introducing reconnect jitter bugs that delay recovery more than expected
  - missing a remaining `innerHTML` sink in the UI code

## 2026-03-22 Service Templates And Env Files

Status: completed

- Goal: provide ready-to-customize deployment templates for `systemd`, `launchd`, and environment files for both relay server and home client.
- Scope:
  - add `systemd` unit templates for `relay_server` and `codex_master` (home client)
  - add `launchd` plist templates for relay server and home client
  - add `.env` example files containing strict-mode auth and reconnect knobs
  - keep all artifacts under `deploy/` without changing runtime Rust logic
- Invariants:
  - templates must default to secure relay deployment assumptions (Caddy TLS termination + strict auth)
  - env examples should align with currently implemented runtime variables
  - placeholders should be explicit and easy to replace per host
- Likely files/modules to change:
  - `PLANS.md`
  - `deploy/systemd/codex-relay.service`
  - `deploy/systemd/codex-home-client.service`
  - `deploy/launchd/com.codex.relay-server.plist`
  - `deploy/launchd/com.codex.home-client.plist`
  - `deploy/env/relay.env.example`
  - `deploy/env/home-client.env.example`
- Verification steps:
  - inspect templates for variable and path consistency
  - ensure service/unit syntax is structurally valid plain text (no placeholders breaking parsers)
- Main risks:
  - ambiguous placeholder paths causing copy-paste deployment mistakes
  - mismatch between env templates and current strict-mode auth requirements

## 2026-03-22 Production Auth Hardening Follow-up

Status: completed

- Goal: harden public-production auth by adding upstream identity passthrough validation, browser origin allowlist checks, and client token rotation/revocation support.
- Scope:
  - extend relay-server client auth from single token to token set + revoked token set
  - remove legacy single-token fallback compatibility so production config has one unambiguous token source
  - add relay-native local login mode (`/login` + cookie session) for deployments that do not want proxy-injected identity headers
  - require explicit browser origin allowlist in strict mode and enforce it on browser HTTP/WS requests
  - remove unauthenticated/anonymous browser mode from production paths
  - update deployment env examples/docs to reflect the new production auth knobs
- Invariants:
  - strict mode remains default for production
  - loopback-only insecure mode remains possible for local debugging
  - browser/session ownership checks continue to apply after authentication
  - no protocol shape changes required between relay server and home client/browser
- Likely files/modules to change:
  - `PLANS.md`
  - `src/relay_server.rs`
  - `deploy/env/relay.env.example`
  - `docs/caddy-auth.md`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - inspect auth flow for valid token, rotated token, and revoked token behavior
  - inspect origin allowlist rejections for missing/invalid browser `Origin`
- Main risks:
  - over-restricting origin validation and blocking legitimate production domains
  - misconfigured token lists causing home-client lockout during rotation
  - introducing auth regressions for local loopback debug mode

## 2026-03-22 Built-in Relay Login Only

Status: completed

- Goal: make relay browser auth rely on built-in login sessions only, so `/` and `/ws/browser` always require relay-managed cookie auth.
- Scope:
  - remove browser proxy-header auth mode and related runtime configuration dependencies
  - keep single-account username/password login flow as the current production path
  - ensure unauthenticated HTTP access redirects to `/login` and unauthenticated browser websocket upgrades are rejected
  - update deployment env examples and docs to remove proxy-header instructions
- Invariants:
  - home client authentication remains token-based and unchanged
  - browser origin allowlist enforcement remains intact in strict mode
  - existing relay session ownership checks continue to use authenticated user id from login session
- Likely files/modules to change:
  - `PLANS.md`
  - `src/relay_server.rs`
  - `deploy/env/relay.env.example`
  - `deploy/Caddyfile.example`
  - `docs/caddy-auth.md`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - manually inspect browser auth code paths for `/`, `/login`, and `/ws/browser`
- Main risks:
  - accidentally breaking existing deployments that still set proxy-header env vars
  - introducing login regressions around cookie handling or session expiry

## 2026-03-23 Relay/Home Client Connection Diagnostics

Status: completed

- Goal: improve observability for relay/home-client websocket lifecycle so disconnect causes can be identified from logs without guesswork.
- Scope:
  - add explicit lifecycle logs on home-client relay transport: connect attempt, connect success, hello send, close frame, read/write failures, and reader/writer task exit reasons
  - add explicit lifecycle logs on relay-server client websocket handling: hello failures, auth decision context, malformed client messages, close frames, and disconnect summary
  - keep protocol behavior unchanged; this is diagnostics-only
- Invariants:
  - do not log sensitive token values in plain text
  - do not change existing auth/session semantics
  - preserve current reconnect/backoff behavior
- Likely files/modules to change:
  - `PLANS.md`
  - `src/transport.rs`
  - `src/main.rs`
  - `src/relay_server.rs`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - inspect new logs for connect/close/error paths to ensure they include enough context (device_id/url/reason) and no token leakage
- Main risks:
  - adding noisy logs that obscure important signals
  - accidentally logging secrets while adding auth-related context

## 2026-03-23 Relay Event Slice Panic Guard

Status: completed

- Goal: prevent relay-server crashes when websocket disconnect/reconnect flows mutate persisted event history and previously captured indexes become stale.
- Scope:
  - harden `update_store` event extraction against shrinking `store.events`
  - harden `RelayStore` snapshot/tail slicing to avoid out-of-bounds panics when checkpoint metadata exceeds event length
  - add regression tests for index-clamping behavior
- Invariants:
  - keep persistence semantics unchanged (append when not compacting, rewrite when compacting)
  - preserve existing event ordering and compaction output
  - avoid introducing silent data loss beyond dropping impossible out-of-range prefixes
- Likely files/modules to change:
  - `PLANS.md`
  - `src/relay_server.rs`
  - `src/relay_state.rs`
- Verification steps:
  - run `cargo fmt`
  - run `cargo test`
  - run `cargo check`
- Main risks:
  - masking deeper bookkeeping bugs if clamping is overused without targeted tests
  - accidentally changing persisted tail content during compaction

## 2026-03-24 Session Resume Stability + Keepalive

Status: completed

- Goal: stop repeated session resume failures after reconnect, reduce websocket idle disconnect churn, and make transport-mode status messaging accurate.
- Scope:
  - send transport-specific startup info so relay mode no longer claims local debug transport
  - add lightweight keepalive heartbeats for browser websocket and home-client relay websocket
  - restrict server auto-resume to recoverable session states and skip sessions already marked `resume_failed`
  - standardize relay-initiated websocket close reasons so browser UI can show explicit server close causes
- Invariants:
  - no protocol breaking changes for existing browser/client message shapes
  - session ownership and auth checks remain unchanged
  - reconnect loops remain bounded by existing backoff behavior
- Likely files/modules to change:
  - `PLANS.md`
  - `src/runtime.rs`
  - `src/main.rs`
  - `src/transport.rs`
  - `src/relay_server.rs`
  - `src/relay_console.html`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run targeted tests around reconnect/resume filtering behavior
  - manually inspect heartbeat setup/cleanup paths for both browser and home-client
- Main risks:
  - heartbeat interval too aggressive causing unnecessary traffic
  - over-filtering resume candidates and preventing legitimate resume attempts
  - leaving stale timers running after websocket close

## 2026-03-24 Relay Server Structural Refactor

Status: completed

- Goal: turn the oversized relay server implementation into a clearer, more maintainable module layout without changing relay/browser/client behavior.
- Scope:
  - split `src/relay_server.rs` by stable responsibility so auth/login handling, HTTP asset handlers, and relay session routing are no longer interleaved in one file
  - centralize repeated relay-session status/event mutation helpers where it meaningfully reduces duplication
  - split client-facing and browser-facing message handlers into direction-specific modules
  - extract the shared connection/broadcast layer so socket send/broadcast/cleanup helpers no longer live in the main module
  - keep protocol/message shapes, persistence format, and websocket route behavior backward-compatible
- Invariants:
  - `relay_server::run` and `RelayServerConfig::from_env` remain the public entry points
  - browser login/cookie/origin enforcement behavior stays unchanged
  - client/browser websocket message routing semantics stay unchanged
  - persisted relay snapshots and event log files remain compatible with existing deployments
- Likely files/modules to change:
  - `PLANS.md`
  - `src/lib.rs`
  - `src/relay_server.rs` or `src/relay_server/mod.rs`
  - new `src/relay_server/*.rs` helper modules
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - sanity-check that login routes, websocket routes, and session broadcast flows still compile against unchanged protocol types
- Main risks:
  - moving private types/functions across modules and accidentally widening or breaking visibility boundaries
  - introducing subtle behavior drift in auth redirects or websocket cleanup paths during extraction
  - over-fragmenting the relay server into too many tiny files instead of a few coherent modules
