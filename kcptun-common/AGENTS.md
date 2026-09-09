<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-28 | Updated: 2026-08-16 (tokio-only refactor) -->

# kcptun-common

## Purpose

Shared helpers for `kcptun-client` and `kcptun-server` so wire-compatible logic is not duplicated. Hosts the encrypted KCP transport factories and the shared full session composition (`KcpStream → Snappy → SMUX`).

## Key Files

| File | Description |
|------|-------------|
| `src/lib.rs` | Crate root; feature-gated re-exports |
| `src/key.rs` | PBKDF2-HMAC-SHA1 key derive (`salt = b"kcp-go"`) |
| `src/mode.rs` | KCP mode profiles (`normal`/`fast`/`fast2`/`fast3`) applied to bare `KCP` |
| `src/kcp_config.rs` | `kcp_config_from` / `KcpCliParams` → `kcp_rs::KcpConfig` (runtime feature) |
| `src/kcptun_session.rs` | `KcptunConfig` + `KcptunSession`: complete client/server KCP + Snappy + SMUX scheduling, rate limiting, keepalive, and stream cleanup |
| `src/kcptun_listener.rs` | `KcptunListener`: encrypted shared-UDP listener with one recv demux and independent per-peer transports |
| `src/snappy_frame.rs` | Session-level Snappy framing stream decoder |
| `src/snappy_pipe.rs` | `SnappyPipe<T>` — `AsyncRead+AsyncWrite` Snappy session codec wrapping any transport (M0.4) |
| `src/pipe.rs` | Go-compatible per-direction closeWait pipe (`knet::copy_bidirectional_postwait`) |
| `src/snmp_log.rs` | Periodic SNMP CSV logger |
| `src/kcp_transport.rs` | Lower layer only: `CryptoTransport` plus internal client/per-peer-server `KcpStream` assembly |
| `src/qpp_port.rs` | QPP stream wrapper (feature `qpp`) |

## Features

| Feature | Effect |
|---------|--------|
| `tokio` (default) | `knet-rs` + `kcp-rs/async` — pipe / snmp / CryptoTransport / KcptunSession / kcp_config |
| `qpp` | `qpp-rs` + `QPPPort` |

Binaries must forward their runtime feature:

```toml
tokio = [..., "kcptun-common/tokio"]
qpp   = ["dep:qpp-rs", "kcptun-common/qpp"]
```

## KcptunSession production path

| Helper | Role |
|--------|------|
| `CryptoTransport` | `PacketTransport` that encrypt/decrypt wraps UDP (CFB/AEAD/null via `kcrypt_rs::wire` + `CryptEngine`) |
| `kcp_config_from` / `KcpCliParams` | Map CLI-shaped params → `kcp_rs::KcpConfig` |
| `KcptunConfig` | Complete KCP + SMUX + compression + rate-limit configuration |
| `KcptunSession::connect` | Complete client session over UDP or raw TCP |
| `KcptunSession::serve_transport` | Complete server session over an already per-peer transport |

**Status (Tasks 1–7):**

- **Done:** `kcp_transport` owns only encrypted KCP construction; it does not own Snappy or SMUX.
- **Done:** `KcptunSession::client/server` owns the shared Snappy+SMUX loops above an already-built `KcpStream`; both binaries and both transport modes use it.
- **Done:** `KcptunListener` keeps exactly one shared-socket receiver, demultiplexes by peer, then wraps each private peer transport with `CryptoTransport` before building `KcpStream`.
- UDP and raw TCP have no binary-local session or fallback path. They differ only in shared-socket demultiplexing requirements.
- **Snappy stays outside KcpStream** (session-level over KCP user data).

## For AI Agents

- Prefer extending this crate over copying helpers into binaries.
- `pipe` starts a grace period when either copy direction completes, matching Go `closeWait`.
- Do not change Snappy framing or PBKDF2 parameters (wire / key interop).
- QPP AsyncRead/Write uses tokio `ReadBuf`.
- When wiring binaries to `KcpStream`, map CLI via `KcpCliParams` / `kcp_config_from`;
  do **not** put Snappy or SMUX inside KcpStream.
- Prefer `KcptunSession::connect` / `serve_transport` for complete sessions;
  use `client/server` only when a role-specific `KcpStream` already exists.

## Dependencies

- Always: `kcp-rs`, `pbkdf2`, `sha1`, `snap`, `log`
- With runtime feature: `knet-rs`, `anyhow`, `bytes`, `parking_lot`
- Optional: `qpp-rs`

<!-- MANUAL: -->
