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
