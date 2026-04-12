# vm-pool Specification

A standalone service that manages a pool of isolated Linux VMs for running workloads.

## Overview

vm-pool sits between Tasks (the orchestrator) and the raw VM infrastructure (apple/container). It provides:

1. **VM Pool** — Dynamic allocation from a shared pool
2. **Image Management** — Versioned, composable image types
3. **Snapshots** — Fast reset between tasks via save/restore
4. **Event Streaming** — Append-only log with real-time pub/sub
5. **Log Forwarding** — Stream VM logs to external consumers

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

## Components

### Protocol (`crates/protocol`)

Shared type definitions for:
- **VmCommand** — Commands sent to supervisor inside VM
- **VmEvent** — Events emitted by supervisor
- **ServiceCommand** — Commands sent by Tasks to service
- **ServiceEvent** — Events emitted by service to Tasks

### Supervisor (`crates/supervisor`)

PID 1 process running inside each VM:
- Receives commands over stdin (JSON lines)
- Emits events to stdout
- Manages workload execution
- Forwards logs to host

### Transport (`crates/transport`)

Host-side communication with VMs:
- Spawns VM process
- JSON-line message framing over stdio
- Async send/receive

### Events (`crates/events`)

Event infrastructure:
- Append-only event log
- Pub/sub for real-time streaming
- Per-VM log buffers for tailing
- Event types: lifecycle, logs, protocol events

### Images (`crates/images`)

Image management:
- Image types: base, agent, automation
- Version tags and content digests
- Local storage and caching
- Build via `container build`

### Pool (`crates/pool`)

VM pool management:
- Allocation/deallocation with limits
- Health monitoring
- Timeout enforcement
- Lifecycle state tracking

### Snapshot (`crates/snapshot`)

VM state persistence:
- Save via Virtualization.framework `saveMachineStateTo`
- Restore via `restoreMachineState`
- Snapshot storage and metadata

### Service (`crates/service`)

Main binary:
- Unix socket listener
- Command handling
- Event streaming to clients
- Health check loop

## API

### Service Commands (Tasks → vm-pool)

```json
{"type": "allocate", "image": "agent:v1.0.0", "config": {"cpus": 2, "memory_mb": 4096}}
{"type": "deallocate", "vm_id": "vm-abc123"}
{"type": "send", "vm_id": "vm-abc123", "command": {"type": "execute", "command": "ls -la"}}
{"type": "snapshot", "vm_id": "vm-abc123", "name": "clean-state"}
{"type": "restore", "vm_id": "vm-abc123", "snapshot": "clean-state"}
{"type": "status"}
{"type": "tail_logs", "vm_id": "vm-abc123", "lines": 100}
{"type": "subscribe_logs", "vm_id": "vm-abc123"}
```

### Service Events (vm-pool → Tasks)

```json
{"type": "vm_allocated", "vm_id": "vm-abc123", "image": "agent:v1.0.0"}
{"type": "vm_ready", "vm_id": "vm-abc123"}
{"type": "vm_event", "vm_id": "vm-abc123", "event": {"type": "output", "stream": "stdout", "data": "..."}}
{"type": "vm_stopped", "vm_id": "vm-abc123"}
{"type": "vm_crashed", "vm_id": "vm-abc123", "error": "..."}
{"type": "pool_status", "total": 6, "available": 4, "allocated": 2}
{"type": "vm_log", "vm_id": "vm-abc123", "stream": "stdout", "line": "..."}
```

## Log Forwarding

VMs emit logs via three streams:
- **stdout** — Workload output
- **stderr** — Workload errors
- **supervisor** — Supervisor internal logs

The service:
1. Captures all VM output
2. Stores in per-VM circular buffers (last N lines)
3. Forwards to event log for persistence
4. Streams to subscribed clients in real-time

Clients can:
- **Tail** — Get last N lines from a VM
- **Subscribe** — Stream logs in real-time
- **Query** — Get historical logs from event log

## Image Types

### Base
Ubuntu 24.04 with common tooling:
- Build essentials, Git, SSH
- Node.js, Bun, Rust
- GitHub CLI

### Agent
Base + interactive agent support:
- Claude Code
- Development tools
- Pre-configured for human-in-the-loop

### Automation
Base + headless execution:
- Minimal overhead
- No interactive tools
- Optimized for CI-style tasks

## Snapshots

Snapshots save the complete VM state (memory + disk) to disk:

```
~/.local/state/vm-pool/snapshots/
├── clean-agent-v1.0.0.vmstate
├── clean-automation-v1.0.0.vmstate
└── custom-checkpoint.vmstate
```

Use cases:
1. **Fast reset** — Restore to clean state between tasks (~5ms vs ~2s cold boot)
2. **Checkpointing** — Save progress during long-running tasks
3. **Prewarming** — Create snapshots after initialization for instant start

## Development Phases

### Phase 1: Foundation
- [x] Repo setup
- [x] Protocol crate with VmId newtype, encode/decode helpers, serde tests
- [x] Supervisor binary with real shell execution
- [x] Transport crate with MockTransport and integration tests

### Phase 2: Images
- [x] Base image Dockerfile
- [x] Image types (agent, automation)
- [x] Images crate with filesystem-backed metadata store

### Phase 3: Pool
- [x] Allocation/deallocation with limits
- [x] Health monitoring with timeout enforcement
- [ ] VM lifecycle with apple/container (ContainerRuntime trait)

### Phase 4: Snapshots
- [x] Snapshot metadata storage
- [ ] Virtualization.framework integration
- [ ] Pool integration for snapshot-based restore

### Phase 5: Service
- [x] Unix socket API with configurable ServiceConfig
- [x] Event streaming to clients
- [x] Error responses for all failure paths
- [x] Log tailing
- [ ] Per-connection log subscription filtering

### Phase 6: Integration
- [x] Tasks client library (vm-pool-client crate)
- [ ] Migration from tasks repo
