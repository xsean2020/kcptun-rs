<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-16 (tokio-only refactor) -->

# kcptun-client

## Purpose

kcptun client binary: local TCP listen → SMUX over KCP/UDP or KCP/raw-TCP to remote server. Both transports use `kcptun_common::KcptunSession`; the binary owns CLI, socket acquisition, stream forwarding, QPP, SNMP log, and pprof.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Features `tokio`/`qpp`/`pprof`; deps kcp/kcrypt/**common**(pipe/snmp/QPP)/smux/knet |
| `src/cli.rs` | Clap `Cli`, JSON `Config`, and deterministic CLI/config merge rules |
| `src/main.rs` | Runtime entry and orchestration: UDP/raw-TCP socket acquisition, `KcptunSession` pool, stream forwarding, logging, and pprof startup |

## Subdirectories

None (flat binary crate).

## For AI Agents

### Working In This Directory

- Stack: local TCP → (optional QPP) → SMUX stream → Snappy session → KCP → BlockCrypt → UDP.
- **Unified session path**: UDP and `--tcp` differ only in how they obtain a
  `knet::DatagramSocket`; both call `KcptunSession::connect` with a
  `KcptunConfig`. There is no binary-local session wrapper or dispatch trait.
  The connection pool stores `Vec<KcptunSession>` directly.
  A flaky tail-loss on large transfers was fixed via the SMUX EOF grace in `smux_rs::Stream::read`
  (see CHANGELOG / production migration plan §0.3).
- Shared: `kcptun_common::{KcptunConfig, KcptunSession, derive_key, pipe, snmp_logger, QPPPort?}`.
- Global allocator: `mimalloc`.
- Prefer `knet::*` for async.
- SNMP logger only meaningful when SNMP collection is enabled in kcp-rs.
- Crypto, FEC, KCP input/flush, and Snappy scheduling belong to the common/KCP
  layers; do not reintroduce them in this binary.

### Testing Requirements

- `cargo test -p kcptun-client`
- `make e2e` / `bash test_e2e.sh` after client path changes
- `make stress` (server-side) still validates client interop under load when used together

### Common Patterns

- Config: `cli.rs` owns CLI + optional JSON (`deny_unknown_fields`); keep flag defaults and merge precedence wire-compatible with Go behavior
- Multi-port remote parse: `host:min-max` / `host:port`

## Dependencies

### Internal

- `kcp-rs`, `kcrypt-rs`, `kcptun-common`, `smux-rs`, `knet-rs`

### External

- `clap`, `serde`/`serde_json`, `parking_lot`, `socket2`, `mimalloc`, default-enabled `pprof`

<!-- MANUAL: pprof is enabled in standard/default builds; minimal `--no-default-features` ARM builds may omit it. -->
