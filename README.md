# vm-pool

A standalone service that manages a pool of isolated Linux VMs for running workloads. Built on [apple/container](https://github.com/apple/container) and Apple's Virtualization.framework.

## Features

- **VM Pool** — Dynamic allocation from a shared pool, health monitoring, automatic cleanup
- **Image Types** — Composable images (base, agent, automation) with versioning
- **Snapshots** — Save/restore VM state for fast reset between tasks
- **Event Streaming** — Append-only log with pub/sub for real-time updates

## Requirements

- macOS 26+ (Tahoe)
- Apple Silicon
- [apple/container](https://github.com/apple/container) CLI

## Usage

```sh
# Start the service
vm-pool serve

# The service listens on a Unix socket for commands from Tasks
```

## License

MIT
