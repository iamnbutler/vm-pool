# vm-pool

A standalone service that manages a pool of isolated Linux VMs for running workloads.

## Project Structure

- `crates/` — Rust crates
  - `protocol/` — Shared command/event type definitions (VmId, VmCommand, VmEvent, ServiceCommand, ServiceEvent)
  - `supervisor/` — PID 1 binary that runs inside VMs, executes shell commands
  - `transport/` — Host ↔ VM stdio communication (VmTransport + MockTransport)
  - `events/` — Append-only event log with pub/sub and per-VM circular log buffers
  - `images/` — Image types, versioning, filesystem-backed metadata store
  - `pool/` — VM allocation, limits, health monitoring with timeout enforcement
  - `snapshot/` — Save/restore VM state metadata (Virtualization.framework integration pending)
  - `service/` — Main binary + library, Unix socket API with configurable ServiceConfig
  - `client/` — High-level async client for communicating with the service
- `images/` — Dockerfiles for each image type
  - `base/` — Ubuntu + common tooling
  - `agent/` — Base + Claude Code + dev tools
  - `automation/` — Base + minimal tooling for headless tasks
- `spec/` — Design documents

## Architecture

```
┌─────────────┐     Unix socket      ┌─────────────┐
│    Tasks    │ ◀──────────────────▶ │  vm-pool   │
│  (client)   │   commands/events    │  (service)  │
└─────────────┘                      └──────┬──────┘
                                            │
                        ┌───────────────────┼───────────────────┐
                        │                   │                   │
                        ▼                   ▼                   ▼
                   ┌─────────┐         ┌─────────┐         ┌─────────┐
                   │  VM 1   │         │  VM 2   │         │  VM 3   │
                   │ (agent) │         │ (agent) │         │ (auto)  │
                   └─────────┘         └─────────┘         └─────────┘
```

## Building

```sh
cargo build                          # build all crates
cargo build -p vm-pool-supervisor   # build supervisor only
cargo test --workspace               # run all tests
```

## Key Decisions

- Host runtime: Rust (edition 2024)
- VM backend: apple/container (Virtualization.framework)
- Host ↔ service IPC: Unix socket with JSON-line protocol
- Host ↔ VM communication: JSON-line protocol over stdio
- Image format: OCI (built with `container build`)
- Snapshots: Virtualization.framework save/restore APIs
- Type safety: VmId newtype, strongly-typed protocol enums
- Testing: TDD with unit tests + integration tests (supervisor spawn)

## Development Phases

1. **Foundation** ✅ — Protocol types with VmId newtype, supervisor with real Execute, transport with MockTransport
2. **Images** ✅ — Image types, versioning, filesystem-backed ImageStore
3. **Pool** ✅ — VM lifecycle, allocation limits, health monitoring with timeout enforcement
4. **Snapshots** ✅ — Metadata persistence (Virtualization.framework integration pending)
5. **Service** ✅ — Unix socket API, configurable ServiceConfig, event streaming, error responses
6. **Integration** ✅ — Client crate with full async API and integration tests
