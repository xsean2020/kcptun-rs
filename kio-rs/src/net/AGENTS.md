<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# net

## Purpose

TCP/UDP socket wrappers. All sockets created via **socket2** (2 MB buffers, SO_REUSEADDR, non-blocking), then handed to tokio or smol async wrappers.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Shared `raw_udp` / TCP setup; re-exports backend types |
| `tokio.rs` | Tokio `TcpListener` / `TcpStream` / `UdpSocket` |
| `smol.rs` | Smol backend equivalents |
| `mmsg.rs` | Optional multi-message / batch UDP helpers if present |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Keep socket options identical across backends (`SOCK_BUF = 2MB`).
- Bidirectional copy lives in **crate root** (`copy_bidirectional*`), not here.
- Client mode may `connect()` UDP when remote is known.

### Testing Requirements

- Covered by `kio-rs` tests and binary e2e

### Common Patterns

- `UdpSocket` / `TcpStream` types re-exported as `kio::{UdpSocket, TcpStream, TcpListener}`

## Dependencies

### Internal

- Parent `kio` facade

### External

- `socket2`, runtime crates

<!-- MANUAL: -->
