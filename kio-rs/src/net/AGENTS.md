<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# net

## Purpose

Runtime-agnostic TCP/UDP types: `TcpListener`, `TcpStream`, `UdpSocket`. Thin wrappers selecting tokio or smol backends.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Public type aliases / re-exports and shared helpers |
| `tokio.rs` | Tokio-backed sockets (`send_batch` / try_send; Linux sendmmsg) |
| `smol.rs` | Smol/async-io-backed sockets (`send_batch`; Linux sendmmsg) |
| `mmsg.rs` | Linux-only `sendmmsg` / `recvmmsg` helpers (cfg `target_os = "linux"`) |

## For AI Agents

### Working In This Directory

- Keep tokio and smol APIs **surface-identical** for callers in binaries.
- Prefer `socket2` for options (reuseaddr, buffers) when both backends need the same knob.
- UDP paths are latency-critical for KCP — avoid extra copies.

### Testing Requirements

Covered by `kio-rs` tests and e2e.

## Dependencies

### Internal
Parent `kio` crate features.

### External
`tokio` or `smol`/`async-io`/`socket2`

<!-- MANUAL: -->
