<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 (feed_data_mut in-place inbound) -->

# kcptun-server

## Purpose

kcptun server binary: UDP/KCP accept → SMUX → target TCP. `KcpServerSession` per peer, DashMap session table, Snappy session codec, optional QPP, SNMP log, optional pprof. Stress integration tests live in `tests/stress_test.rs` (no AGENTS under `tests/`).

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Features `tokio` (default) / `smol`; optional `pprof`; + `dashmap` vs client |
| `build.rs` | Build-time glue |
| `src/main.rs` | Entire binary: `Cli`, `KcpServerSession`, `SmuxStreamIo`, `handle_stream`, `pipe` idle timeout, pprof |
| `tests/stress_test.rs` | Multi-connection stress / data integrity (run via `make stress`) |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `tests/` | Integration stress tests only — **no AGENTS.md** |

## For AI Agents

### Working In This Directory

- Stack: UDP → decrypt/FEC → KCP → Snappy → SMUX → (optional QPP) → target TCP.
- `pipe` uses **idle** timeout (`closewait`), not total duration — matches Go; do not convert to hard total timeout.
- Session demux by peer; inbound via `feed_data_mut(&mut [u8])` — CFB/null decrypt in place (`decrypt_cfb_in_place` / `inbound_null`), then FEC + `KCP::input` + inline SMUX.
- Flush loop 4-phase like client; keep lock short.
- Known open issue: proxy SMUX stream leak → RSS growth (`bugs/BUGREPORT_PROXY_MEMORY_GROWTH.md`).
- PBKDF2 / crypt / mode / nocomp must match client.

### Testing Requirements

```bash
make stress
cargo test --release --package kcptun-server --test stress_test -- --nocapture --test-threads=1
make e2e
```

### Common Patterns

- `DashMap` for concurrent session lookup
- Log rotation helper for file logs

## Dependencies

### Internal

- `kcp-rs`, `kcrypt-rs`, `smux-rs`, `qpp-rs`, `kio-rs`

### External

- Same family as client + `dashmap`; optional `pprof`

<!-- MANUAL: pprof feature is optional and off by default (keeps ARM release bins small). Enable with --features pprof. -->
