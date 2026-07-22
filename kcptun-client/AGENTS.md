<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 (inbound CFB/null less-copy) -->

# kcptun-client

## Purpose

kcptun client binary: local TCP listen → SMUX over KCP/UDP to remote server. Single-file `src/main.rs` owns CLI, key derive, `KcpConn` flush loop, Snappy session codec, optional QPP, SNMP log, optional pprof.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Features `tokio` (default) / `smol`; optional `pprof`; deps kcp/kcrypt/smux/qpp/kio, clap, snap, mimalloc |
| `build.rs` | Build-time glue |
| `src/main.rs` | Entire binary: `Cli`, `KcpConn`, `SmuxStreamAsync`, `QPPPort`, `handle_client`, `snmp_logger`, `run_pprof` |

## Subdirectories

None (flat binary crate).

## For AI Agents

### Working In This Directory

- Stack: local TCP → (optional QPP) → SMUX stream → Snappy session → KCP → BlockCrypt → UDP.
- Flush loop is **4-phase** to minimize KCP mutex hold; keep crypto/snappy outside the lock.
- PBKDF2 salt `b"kcp-go"`, 4096 iters, 32-byte key — must match server.
- Modes (`fast3` etc.) map to KCP nodelay/interval/resend/nc via `apply_mode`.
- Global allocator: `mimalloc`.
- Prefer `kio::*` for async; dual impl blocks for tokio/smol on AsyncRead/Write wrappers.
- SNMP logger only meaningful when SNMP collection is enabled in kcp-rs.
- UDP reader: CFB decrypts **in place** on the recv buffer (`decrypt_cfb_in_place`, no inbound `CryptoBuf` lock); null uses the recv slice; FEC/KCP `input` takes `&[u8]` without intermediate `Bytes` copies.

### Testing Requirements

- `cargo test -p kcptun-client`
- `make e2e` / `bash test_e2e.sh` after client path changes
- `make stress` (server-side) still validates client interop under load when used together

### Common Patterns

- Config: CLI + optional JSON (`deny_unknown_fields`)
- Multi-port remote parse: `host:min-max` / `host:port`

## Dependencies

### Internal

- `kcp-rs`, `kcrypt-rs`, `smux-rs`, `qpp-rs`, `kio-rs`

### External

- `clap`, `serde`/`serde_json`, `snap`, `pbkdf2`/`sha1`, `parking_lot`, `socket2`, `mimalloc`, optional `pprof`

<!-- MANUAL: pprof feature is optional and off by default (keeps ARM release bins small). Enable with --features pprof. -->
