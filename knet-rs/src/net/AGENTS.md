<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-16 (tokio-only refactor) -->

# net

## Purpose

TCP/UDP socket wrappers built on tokio. UDP sockets are created via
**socket2** (4 MB buffers, SO_REUSEADDR, non-blocking), then wrapped in
tokio's `UdpSocket`. Connected sockets use native `send`/`recv` rather
than carrying peer addresses.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Shared `raw_udp` / TCP setup; re-exports tokio types; `DatagramSocket` enum |
| `tokio.rs` | Tokio `TcpListener` / `TcpStream` / `UnixListener` / `UdpSocket` |
| `mmsg.rs` | Linux-only `sendmmsg`/`recvmmsg` batch helpers, **allocation-free** (per-thread scratch) |
| `tcpraw.rs` | Linux-only TCP raw transport. Inbound RST filter (`seg_ignored`) + `Takeover` enum (Repair → iptables TTL-DROP fallback + `KCPTCP_TAKEOVER` env) + graceful close (drain RX + repair-off + FIN) + accepted-conn deregistration |
| `tcpraw_stub.rs` | Non-Linux stub returning `Unsupported` |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Keep UDP socket options consistent (`SOCK_BUF = 4MB`).
- Bidirectional copy lives in **crate root** (`copy_bidirectional*`), not here.
- Client mode may `connect()` UDP when remote is known.
- On Unix, `TcpStream` can wrap TCP or Unix-domain streams; `UnixListener` owns and unlinks its
  filesystem path on drop, matching Go's default `UnixListener` cleanup.
- **tcpraw** is Linux-only. Seed seq/ack with `TCP_REPAIR` + `TCP_QUEUE_SEQ` (not `tcp_info.tcpi_snd_nxt` — those fields are absent from the `libc` crate). Do **not** use `TCP_TIMESTAMP` after repair for peer ts_ecr — it returns local `tsoffset`; start ts_ecr at 0 and fill from captured options. Use `socket2` feature `all` for `Type::RAW`.
- **Wire: TCP segments only** (`IPPROTO_TCP` raw, no `IP_HDRINCL`). Go `DialIP("ip:tcp")` same shape — kernel adds/strips IP. Sending full IP+TCP with HDRINCL breaks connectivity.
- Capture must update flow (seq←peer ACK, ack←peer seq+len, ts_ecr←peer TSval) and filter `dst_port`; only push PSH payloads (Go `captureFlow`).
- **TcpRawListener must stay blocking.** `accept()` runs in `cpu_block` (OS thread). Non-blocking accept returns EAGAIN when idle → server accept loop exits → client `--tcp` dial gets Connection refused.
- **mmsg IPv4 `s_addr`**: use `u32::from_ne_bytes(ip.octets())`, never `from_be_bytes` — LE hosts would send to `1.0.0.127` instead of `127.0.0.1` (silent blackhole / hang). Only affects Linux `sendmmsg_to` (server flush path).
- **All hot UDP sends route through `send_batch`/`send_batch_to`** → Linux `sendmmsg` (`mmsg.rs`). Confirmed callers: `KcpStream` data + ACK/urgent path, `KcpListener` `PeerTransport`, `CryptoTransport` `send_urgent*`. The single `send`/`send_to` methods are **fallback-only** (non-Linux, `WouldBlock`). Keep new send paths on the batch API — do not regress to per-datagram `send_to`.
- `mmsg.rs` uses a per-thread scratch (`MmsgScratch` via `thread_local!`) so `sendmmsg`/`recvmmsg` do **not** allocate per batch; `sendmmsg_*` are generic over `B: AsRef<[u8]>` so callers pass `&bufs[offset..]` directly (no `&[u8]` refs Vec).

### Testing Requirements

- Covered by `knet-rs` tests and binary e2e
- **tcpraw integration tests** (`net::tcpraw::integration_tests`) read the
  process-global `KCPTCP_TAKEOVER` env var — they must run **serially**:
  `cargo test -p knet-rs net::tcpraw::integration_tests -- --test-threads=1`
  (with `KCPTCP_ROOT_TEST=1` + root on Linux). Parallel runs race the env var.
- **✅ LINUX VERIFIED 2026-08-06**: `mmsg.rs` `#[cfg(test)]` round-trip tests pass in a
  `rustlang/rust:nightly` container: `cargo test -p knet-rs` → 25 passed, incl.
  `sendmmsg_to_roundtrip` + `recvmmsg_from_batch`. Container recipe: mount repo,
  `CARGO_TARGET_DIR=<repo>/target/linux-verify`,
  `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=cc` (overrides the macOS cross-linker
  in `.cargo/config.toml`).

### Common Patterns

- `UdpSocket` / `TcpStream` types re-exported as `knet::{UdpSocket, TcpStream, TcpListener}`;
  Unix platforms additionally re-export `UnixListener`.

## Dependencies

### Internal

- Parent `knet` facade

### External

- `socket2` (feature `all` — needed for RAW / IP_HDRINCL), `libc`, tokio

<!-- MANUAL: -->
