# Plans

## 2026-03-20 Remote Codex Client/Server Architecture

- Goal: document a production-oriented architecture where a home-network client hosts `codex-acp` / `codex cli`, a public VPS hosts the relay server, and users access the system through a PWA.
- Scope: design only; no runtime behavior or code changes to the existing Rust prototype.
- Invariants:
  - `codex-acp` and `codex cli` run only on the home/internal machine.
  - The public VPS server never directly executes local workspace commands.
  - The home client establishes the outbound long-lived connection to the VPS.
  - Permission prompts are user-mediated through the PWA by default.
- Likely files/modules to change:
  - `PLANS.md`
  - `docs/remote-architecture.md`
- Verification steps:
  - Review the current Rust prototype to anchor the design in the existing ACP relay flow.
  - Ensure the design covers topology, session lifecycle, message protocol, security boundaries, deployment, and phased rollout.
  - Confirm the saved document is readable and scoped for implementation planning.
- Main risks:
  - Over-designing before validating the minimum viable client/server protocol.
  - Leaving permission handling or reconnect semantics ambiguous.
  - Blurring responsibilities between the public server and the home client.

## 2026-03-20 Phase 1 Home Client Refactor

- Goal: turn the current single-file local relay into a reusable `home client` runtime with structured commands/events and a pluggable transport layer.
- Scope:
  - extract ACP/session orchestration from terminal I/O
  - define command/event types for inbound control and outbound streaming
  - keep a local debug transport so the prototype remains runnable
  - do not implement the real VPS relay transport yet
- Invariants:
  - `codex-acp` is still launched locally by the Rust process
  - prompts still stream chunked output as they arrive
  - the runtime owns ACP sessions and working directory resolution
  - permission requests must no longer be hard-wired to terminal-only behavior
- Likely files/modules to change:
  - `PLANS.md`
  - `src/main.rs`
  - `src/client.rs`
  - `src/runtime.rs`
  - `src/transport.rs`
  - `src/types.rs`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - verify the local debug transport still supports creating sessions and prompting
- Main risks:
  - borrowing/lifetime complexity when moving ACP callbacks into reusable runtime state
  - over-abstracting before the relay transport exists
  - accidentally regressing the current local interactive behavior

## 2026-03-20 Relay Transport Foundation

- Goal: add a real relay-facing transport to the home client so it can connect to a future VPS relay server over WebSocket while preserving the local debug mode.
- Scope:
  - define JSON protocol envelopes for client/server messages
  - add configuration for choosing `local` or `relay` transport
  - implement a minimal WebSocket relay transport for inbound commands and outbound events
  - leave full auth hardening, heartbeats, and reconnect policy as follow-up work
- Invariants:
  - ACP runtime remains transport-agnostic
  - local debug mode remains available
  - relay transport is outbound-only from the home client
  - permission requests still round-trip through the transport rather than being auto-approved
- Likely files/modules to change:
  - `PLANS.md`
  - `Cargo.toml`
  - `src/main.rs`
  - `src/transport.rs`
  - `src/types.rs`
  - `src/protocol.rs`
  - `src/config.rs`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - inspect the serialized protocol shapes and transport selection flow
- Main risks:
  - choosing protocol fields that are too rigid before the server exists
  - websocket integration introducing `Send`/`!Send` friction with the current single-thread local runtime
  - limited runtime verification if dependency downloads remain blocked

## 2026-03-20 Minimal Relay Server

- Goal: add a minimal in-memory relay server binary that accepts home-client and browser WebSocket connections and routes session, prompt, and permission messages between them.
- Scope:
  - convert shared client modules into a library crate
  - add browser-facing protocol messages
  - add a minimal `relay_server` binary with in-memory device/session state
  - keep auth, persistence, reconnect recovery, and horizontal scaling as follow-up work
- Invariants:
  - the server remains a routing/control plane and does not execute workspace commands
  - home client connections remain outbound-only
  - relay session ids are server-owned logical ids
  - browser and client connections communicate only through structured protocol messages
- Likely files/modules to change:
  - `PLANS.md`
  - `Cargo.toml`
  - `src/lib.rs`
  - `src/main.rs`
  - `src/protocol.rs`
  - `src/relay_server.rs`
  - `src/bin/relay_server.rs`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - inspect session routing and message mapping paths for browser -> server -> client -> server -> browser
- Main risks:
  - concurrency bugs in in-memory routing state
  - protocol ambiguity between browser-side and client-side messages
  - incomplete cleanup behavior on disconnect before persistence/recovery exists

## 2026-03-20 Relay Metadata Persistence

- Goal: persist relay-server device/session metadata and append-only event history so relay restarts no longer erase all logical state immediately.
- Scope:
  - add a persistence layer for relay devices, sessions, and session events
  - load persisted metadata on relay startup
  - snapshot state after device/session lifecycle changes and append key browser-visible events
  - keep persistence local-file based; do not introduce a database yet
- Invariants:
  - persistence stores relay control-plane metadata only, not workspace files
  - server-generated relay session ids remain the source of truth
  - browser-visible session events should be recoverable after a restart
  - persistence failures should surface as errors, not silently corrupt state
- Likely files/modules to change:
  - `PLANS.md`
  - `src/relay_server.rs`
  - `src/relay_state.rs`
  - `Cargo.toml`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - inspect the persisted JSON shapes and the save/load call sites
- Main risks:
  - over-serializing transient runtime state
  - writing snapshots too often without batching
  - leaving browser reconnect semantics only partially solved without a follow-up subscription API

## 2026-03-20 Relay Log Compaction And Checkpoint

- Goal: replace unbounded relay event-log growth with compaction into checkpointed history while preserving crash-tolerant recovery semantics.
- Scope:
  - add checkpoint metadata to the relay snapshot
  - add monotonic event sequencing to session history entries
  - compact older event-log entries into snapshot checkpointed history
  - keep browser history reads working against checkpointed history plus live log tail
- Invariants:
  - snapshot remains the recovery checkpoint for relay metadata
  - event log remains append-only between compactions
  - recovery should not duplicate already-checkpointed events after a restart
  - compaction should preserve browser-visible history, even if chunk events are merged
- Likely files/modules to change:
  - `PLANS.md`
  - `src/protocol.rs`
  - `src/relay_state.rs`
  - `src/relay_server.rs`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - inspect checkpoint/log load ordering and compaction trigger behavior
- Main risks:
  - missing an event creation path when introducing monotonic sequencing
  - incorrect de-duplication when loading checkpointed history plus log tail
  - over-compacting history in ways that make browser debugging harder

## 2026-03-20 Formal PWA For Relay Console

- Goal: turn the relay browser console into an installable PWA with explicit app metadata, app-shell caching, and mobile-friendly standalone behavior.
- Scope:
  - keep the existing single-file browser console architecture
  - serve PWA assets directly from the Rust relay server without adding a frontend build pipeline
  - add a web app manifest, service worker, and app icons
  - update the HTML shell to register the service worker and expose install/state cues
- Invariants:
  - the relay console remains reachable at `/`
  - WebSocket protocol and relay session behavior do not change
  - server-side changes are limited to static asset delivery for the browser shell
  - PWA assets should version cleanly enough to pick up future shell updates
- Likely files/modules to change:
  - `PLANS.md`
  - `src/relay_server.rs`
  - `src/relay_console.html`
  - `src/pwa_manifest.webmanifest`
  - `src/pwa_service_worker.js`
  - `src/pwa_icon.svg`
  - `src/pwa_icon_maskable.svg`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - confirm the relay server exposes the manifest, service worker, and icon routes
  - review the HTML for service worker registration and install-path behavior
- Main risks:
  - caching the shell too aggressively and making updates look stale
  - shipping icons or manifest metadata that install poorly on some platforms
  - adding PWA UX that assumes browser support the current environment may not have

## 2026-03-20 Workspace Allowlist For Session Creation

- Goal: replace browser-provided `cwd` session creation with a home-client-owned workspace allowlist keyed by `workspace_id`.
- Scope:
  - add structured workspace configuration to the home client
  - advertise allowed workspaces from the home client to the relay server
  - update relay/browser session creation to target `workspace_id` instead of raw paths
  - keep the existing single-device debug flow working with a sensible default workspace
- Invariants:
  - raw local workspace paths must stay on the home client
  - the relay server remains a control plane and must not invent filesystem paths
  - existing prompt streaming and permission round-trips must keep working
  - local debug mode should still be usable without extra setup
- Likely files/modules to change:
  - `PLANS.md`
  - `src/config.rs`
  - `src/main.rs`
  - `src/types.rs`
  - `src/protocol.rs`
  - `src/runtime.rs`
  - `src/transport.rs`
  - `src/relay_server.rs`
  - `src/relay_console.html`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - inspect the browser console flow for device -> workspace -> create session
  - verify the home client still has a default workspace when no explicit allowlist is configured
- Main risks:
  - introducing protocol drift between relay server, browser, and home client message shapes
  - making auto-created local debug sessions ambiguous when no default workspace is available
  - accidentally leaking local filesystem paths back into browser-visible metadata

## 2026-03-20 Reconnect Resume And Session Rebind

- Goal: allow relay sessions to survive home-client reconnects by re-binding persisted relay session ids to resumable ACP session ids after the device reconnects.
- Scope:
  - persist the opaque ACP/client session id alongside relay session metadata
  - extend the relay protocol with an explicit resume command and resume success/failure events
  - teach the home client runtime to resume ACP sessions and rebuild its in-memory relay->ACP session map
  - keep browser-facing session ids stable and history readable across disconnects
- Invariants:
  - relay session ids remain server-owned logical ids
  - local workspace paths stay on the home client
  - the relay server stores opaque client session ids only for routing/resume, not filesystem paths
  - reconnect failure must degrade to a readable but non-runnable session instead of silently misrouting prompts
- Likely files/modules to change:
  - `PLANS.md`
  - `src/protocol.rs`
  - `src/types.rs`
  - `src/runtime.rs`
  - `src/transport.rs`
  - `src/relay_server.rs`
  - `src/relay_state.rs`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - inspect the reconnect flow: create session -> persist opaque client session id -> simulate device disconnect -> resume command rebinds relay session
- Main risks:
  - assuming ACP resume semantics that differ from `codex-acp` runtime behavior
  - restoring stale client session ids and leaving relay sessions in a misleading status
  - introducing protocol changes that update one side of the relay without the other

### Follow-up: Browser Session Updated Broadcast

- Goal: push single-session summary changes to connected browsers so reconnect/resume and other state transitions update the session list without requiring a full `list_sessions` refresh.
- Scope:
  - add a browser-facing `session_updated` message carrying one `RelaySessionSummaryMessage`
  - track browser user identity in live websocket connections so updates respect session ownership visibility
  - broadcast summary changes for meaningful status transitions, not for every output chunk
- Invariants:
  - browser session visibility must continue to respect authenticated owner scoping
  - `session_list` remains the full-sync path; `session_updated` is incremental
  - output streaming remains on the existing `output_chunk` channel
- Likely files/modules to change:
  - `PLANS.md`
  - `src/protocol.rs`
  - `src/relay_server.rs`
  - `src/relay_console.html`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - inspect browser flow for create -> running -> awaiting_permission -> idle -> device_disconnected without manual refresh
- Main risks:
  - over-broadcasting updates for non-owner sessions
  - forgetting a summary mutation path and leaving the browser list stale
  - turning high-frequency events into unnecessary session-list churn

### Follow-up: Browser Session Removed Broadcast

- Goal: remove sessions from browser-side incremental state when session visibility shrinks, especially when a legacy ownerless session is claimed by one authenticated user.
- Scope:
  - add a browser-facing `session_removed` message carrying the logical session id
  - compare previous vs current visibility when ownership changes and notify browsers that lose access
  - keep full `session_list` as the recovery path, but stop relying on it for owner-claim cleanup
- Invariants:
  - browsers must never retain stale sessions they no longer have permission to access
  - visibility expansion should still use `session_updated`
  - no output-stream behavior should depend on `session_removed`
- Likely files/modules to change:
  - `PLANS.md`
  - `src/protocol.rs`
  - `src/relay_server.rs`
  - `src/relay_console.html`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - inspect ownerless-session adopt flow to confirm the claiming browser keeps the session while other browsers drop it without a full refresh
- Main risks:
  - missing one visibility-shrinking path and leaving stale UI state
  - over-generalizing removal logic before actual session deletion exists

### Follow-up: Session Closed And Delete Flow

- Goal: add an explicit logical close state for relay sessions plus a real relay-side delete flow that removes summary, bindings, and persisted history.
- Scope:
  - add browser commands for `close_session` and `delete_session`
  - add browser events for `session_closed` and reuse `session_removed` for delete
  - close should mark the relay session read-only and attempt ACP prompt cancellation when applicable
  - delete should require a closed session and remove relay metadata, routing state, pending permissions, and history events
- Invariants:
  - closed sessions remain readable via history but must reject new prompts
  - delete is relay-side deletion; it must stop future resume/rebind for that session
  - the implementation must not pretend it can hard-delete ACP-internal state that the current ACP API does not expose
- Likely files/modules to change:
  - `PLANS.md`
  - `src/protocol.rs`
  - `src/types.rs`
  - `src/runtime.rs`
  - `src/transport.rs`
  - `src/relay_server.rs`
  - `src/relay_console.html`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - inspect browser flow for close -> session becomes read-only -> delete removes it from list and history API returns unknown session
- Main risks:
  - close racing with in-flight prompt updates and accidentally reopening the session status
  - forgetting to scrub pending permission state on close/delete
  - making delete too permissive and removing sessions that are still active

### Follow-up: Delete Audit And Browser Confirmation

- Goal: preserve a relay-wide `session_deleted` audit trail while reducing accidental close/delete actions in the browser console.
- Scope:
  - keep a `session_deleted` event in the persisted relay event log even after per-session history is deleted
  - add browser confirmation dialogs before `close_session` and `delete_session`
  - keep delete semantics unchanged: session-local history is removed, audit event remains global
- Invariants:
  - deleting a session must still remove that session's own history from `get_session_history`
  - the audit event must not resurrect the deleted session in session history reads
  - browser confirmations should be client-side only and not change server auth semantics
- Likely files/modules to change:
  - `PLANS.md`
  - `src/relay_server.rs`
  - `src/relay_console.html`
- Verification steps:
  - run `cargo fmt`
  - run `cargo check`
  - run `cargo test`
  - run `cargo clippy --all-targets --all-features -- -D warnings`
  - inspect delete flow to confirm the session disappears from browser/history while the relay event log retains `session_deleted`
- Main risks:
  - accidentally retaining deleted session events in history pagination
  - adding an audit event shape that is too vague to be useful later

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
