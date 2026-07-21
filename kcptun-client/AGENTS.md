<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# kcptun-client

## Purpose

Client binary: listens for local TCP, tunnels over KCP/UDP to a remote kcptun-server. Single large `src/main.rs` (~2040 LOC) owns CLI, KCP connection pool (`KcpConn`), Snappy session framing, SMUX open, optional QPP, and bidirectional pipe.

Default listen `:12948`; requires `--remoteaddr`.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Features tokio/smol; optional `pprof` (off by default); `mimalloc`; path deps |
| `src/main.rs` | Entire client: `Cli`, `KcpConn`, flush loop, `handle_client`, SNMP, pprof helper |

## Subdirectories

None (binary crate).

## For AI Agents

### Working In This Directory

- Keep wire/behavior parity with `kcptun-server` and Go client for shared helpers: `SALT = b"kcp-go"`, `derive_key`, `apply_mode`, Snappy CRC32C framing, `PIPE_BUF_SIZE=65536`.
- **KCP update interval: 2 ms** (`KCP_UPDATE_INTERVAL_MS`) — tighter than server's 10 ms.
- `KcpConn` flush loop: 4-phase design (Snappy/`cpu_block`/parallel encrypt outside lock; KCP under lock briefly).
- Multi-port remote: `IP:min-max` → `parse_multi_port`.
- Cipher storage: `Arc<dyn BlockCrypt>` (no Mutex).
- `null` vs `none` header behavior must match Go.
- When fixing a bug also present in server, update **both** mains unless intentional asymmetry.

### Testing Requirements

```bash
cargo build -p kcptun-client --release
bash test_e2e.sh
make stress   # client is started by stress_test
make bench
```

### Common Patterns

- CLI via clap + optional JSON `-c`
- `SmuxStreamAsync` bridges smux `Stream` to kio AsyncRead/Write (cfg dual impl)
- `QPPPort` optional wrapper when `--QPP`
- `snmp_logger` background task

## Dependencies

### Internal
`kcp-rs`, `kcrypt-rs`, `smux-rs`, `qpp-rs`, `kio-rs`

### External
`clap`, `serde`/`serde_json`, `snap`, `pbkdf2`, `sha1`, `parking_lot`, `crc32fast`, `socket2`, `mimalloc`, `anyhow`, `log`/`env_logger`, `bytes`; optional `pprof`

<!-- MANUAL: pprof feature is optional and off by default (keeps ARM release bins small). Enable with --features pprof. -->
