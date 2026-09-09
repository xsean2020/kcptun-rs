<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-16 (tokio-only refactor: kio→knet) -->

# knet-rs

## Purpose

Tokio-based network I/O extensions for kcptun (`lib` name: `knet`). Provides `mmsg` (batch UDP), `tcpraw` (Linux TCP_REPAIR), `DatagramSocket`, `cpu_block` (blocking pool offload), and bidirectional copy utilities — all built on tokio. Business code calls `knet::sleep_ms`, `spawn_task`, `cpu_block`, sockets, etc.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Depends on `tokio`, `socket2` (feat `all`), `libc`, `async-channel`, `async-lock` |
| `src/lib.rs` | Facade: re-exports net/sync/task/time; `copy_bidirectional`, `copy_bidirectional_idle`, `ctrl_c`, `read_to_string`, `block_on` |
| `src/tests.rs` | Crate tests |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/net/` | TCP/UDP via socket2 + tokio wrappers (see `src/net/AGENTS.md`) |
| `src/sync/` | `Notify`, `CancellationToken`, `Mutex` (see `src/sync/AGENTS.md`) |
| `src/task/` | `spawn_task`, bounded `cpu_block`, `block_on`, `JoinHandle` (see `src/task/AGENTS.md`) |
| `src/time/` | `sleep`, `sleep_ms`, `timeout`, `Elapsed` (see `src/time/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- Crate name is `knet` (imported as `use knet::*` in business code).
- `cpu_block` offloads CPU work (crypto/snappy) to a blocking pool — used heavily on flush paths.
- `mmsg` provides Linux `sendmmsg`/`recvmmsg` for batch UDP I/O (no-op on non-Linux).
- `tcpraw` provides Linux `TCP_REPAIR` mode raw TCP transport.
- UDP socket buffers: 4 MB recv/send via the shared socket2 setup.

### Testing Requirements

- `src/tests.rs` and `cargo test -p knet-rs`
- Build: `cargo build -p knet-rs`

### Common Patterns

```rust
use knet::{spawn_task, cpu_block, sleep_ms, UdpSocket, TcpStream};
```

On Unix, `UnixListener` accepts local filesystem sockets while the unified
`TcpStream::connect` automatically falls back to a Unix socket path when the
input is not a valid TCP address.

## Dependencies

### Internal

None (consumed by smux + binaries).

### External

- `tokio` (full features: net, time, sync, rt, macros, process, io-util, fs)
- `socket2` (features=`["all"]` for tcpraw/mmsg)
- `async-lock`, `async-channel`, `log`, `libc`

<!-- MANUAL: -->
