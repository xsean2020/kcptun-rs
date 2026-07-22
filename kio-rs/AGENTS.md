<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# kio-rs

## Purpose

Async runtime + network I/O abstraction for kcptun (`lib` name: `kio`). Unified API that compiles under either **tokio** (default) or **smol**. Business code calls `kio::sleep_ms`, `spawn_task`, `cpu_block`, sockets, etc. without knowing the backend.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Features `tokio` / `smol` (mutex exclusive); shared `async-lock`, `async-channel`, `socket2`, `libc` |
| `build.rs` | Feature checks / glue |
| `src/lib.rs` | Facade: re-exports net/sync/task/time; `copy_bidirectional`, `copy_bidirectional_idle`, `ctrl_c`, `read_to_string` |
| `src/tests.rs` | Crate tests |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/net/` | TCP/UDP via socket2 + runtime wrappers (see `src/net/AGENTS.md`) |
| `src/sync/` | `Notify`, `Mutex` (see `src/sync/AGENTS.md`) |
| `src/task/` | `spawn_task`, `cpu_block`, `block_on`, `JoinHandle` (see `src/task/AGENTS.md`) |
| `src/time/` | `sleep`, `sleep_ms`, `timeout`, `Elapsed` (see `src/time/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **Never enable both `tokio` and `smol`.** Compile error if both.
- Prefer this crate over raw tokio/smol in client/server/smux new code.
- `cpu_block` offloads CPU work (crypto/snappy) to a blocking pool — used heavily on flush paths.
- Smol `JoinHandle` detaches on drop (tokio-like); do not assume cancel-on-drop.
- Socket buffers: 2 MB recv/send via socket2 in `net`.

### Testing Requirements

- `src/tests.rs` and `cargo test -p kio-rs`
- Build both features: default tokio and `--no-default-features --features smol`

### Common Patterns

```rust
use kio::{spawn_task, cpu_block, sleep_ms, UdpSocket, TcpStream};
```

## Dependencies

### Internal

None (consumed by smux + binaries).

### External

- Shared: `async-lock`, `async-channel`, `socket2`, `log`, `libc`
- Tokio feature: `tokio`
- Smol feature: `smol`, `async-io`, `async-executor`, `futures-lite`, `num_cpus`, `event-listener`

<!-- MANUAL: -->
