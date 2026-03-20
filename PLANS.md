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
