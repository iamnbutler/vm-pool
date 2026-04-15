# vm-pool TODO

## Architecture direction

vm-pool is pure infrastructure. It manages VMs, pools, priorities, health, and provides a typed channel for passing commands/events through. It does not encode application business logic.

### Key principles

- The supervisor is PID 1, owned by vm-pool. Its children (agents, automation binaries, ssh servers, etc.) are the application's concern.
- The event/command framework should be generic — vm-pool owns pool-level commands (allocate, deallocate, status, snapshot, health), and the application defines its own command/event vocabulary that flows through as typed passthrough.
- Children inside the VM talk to the supervisor via a unix socket. The supervisor multiplexes child events onto the host transport (stdio JSON-line to the pool service).
- No mocks, ever. All tests use real processes.

## Immediate: crates.io publish

- [ ] Write README suitable for crates.io (short, what it is, status, requirements)
- [ ] Add `homepage`, `keywords`, `categories` to workspace Cargo.toml
- [ ] Add `repository.workspace = true` to all crate Cargo.tomls
- [ ] Version: `0.1.0-alpha.1`
- [ ] `cargo publish --dry-run` each crate
- [ ] Publish to crates.io

## Generic protocol

The current protocol hardcodes `VmCommand::Execute` and `VmEvent::Output` — this leaks assumptions about what runs inside VMs.

- [ ] Split protocol into pool-level commands (fixed) and VM-passthrough commands (generic)
  - Pool commands: `Allocate`, `Deallocate`, `Status`, `Snapshot`, `Restore`, `TailLogs`, `SubscribeLogs`
  - Passthrough: `Send { vm_id, payload: T }` where T is application-defined (`Serialize + DeserializeOwned`)
- [ ] Make `Pool<R, C, E>` generic over runtime `R`, command type `C`, and event type `E`
- [ ] Transport becomes generic over the message types it frames
- [ ] Supervisor protocol split: infrastructure messages (ping/pong, shutdown, health) vs application messages (opaque forwarding)

## Supervisor rework

The supervisor currently just runs shell commands. It needs to become a real process manager.

- [ ] Supervisor listens on a unix socket inside the VM (e.g. `/run/supervisor.sock`)
- [ ] Child process management: start, stop, restart, hot-patch binaries
- [ ] Children connect to the supervisor socket to emit events
- [ ] Supervisor multiplexes child events onto the host transport (stdout JSON-line)
- [ ] Infrastructure commands (ping, shutdown, process management) are supervisor-owned
- [ ] Application commands are forwarded to the appropriate child via the unix socket

## Container runtime

The `ContainerRuntime` is implemented but untested with a real container image.

- [ ] Build a container image with the supervisor baked in as entrypoint
- [ ] Test `ContainerRuntime` end-to-end: start container, send commands, receive events, stop
- [ ] Add DNS config to `VmConfig`
- [ ] Orphan detection: on startup, check for stale containers via `container list`

## Snapshots

Metadata persistence is done. Actual save/restore needs Virtualization.framework.

- [ ] Investigate apple/container snapshot support (may expose save/restore)
- [ ] If not exposed, look into direct Virtualization.framework Swift interop
- [ ] Wire snapshot save/restore into pool lifecycle (pause VM, save state, resume)
- [ ] Prewarm snapshots: boot + initialize, snapshot, restore for instant start

## Event system improvements

- [ ] Per-connection log subscription filtering (currently broadcasts everything)
- [ ] Event persistence to disk (currently in-memory only)
- [ ] Event compaction / retention policy for long-running services
