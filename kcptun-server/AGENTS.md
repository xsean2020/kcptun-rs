<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# kcptun-server

## Purpose

Server binary: UDP KCP listener, SMUX accept, forward each stream to a TCP `--target`. Large `src/main.rs` (~2589 LOC) plus integration **stress_test**. Session map uses `dashmap`. Default listen `:29900`.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Same feature layout as client (`tokio`/`smol`, optional `pprof` off by default) + `dashmap` |
| `src/main.rs` | `KcpServerSession`, `get_or_create_session`, stream handler, flush loop, SNMP |
| `tests/stress_test.rs` | Multi-thread data-integrity stress (1 B–512 KB, many conns) |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `tests/` | Integration tests (see `tests/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **KCP update interval: 10 ms** (vs client 2 ms) — intentional.
- Session demux by conversation identity; `get_or_create_session` is the hot UDP path.
- Flush loop mirrors client 4-phase design; Snappy Phase 3 offloaded via `kio::cpu_block`.
- Pipe uses `copy_bidirectional_idle` with `closeWait`-style idle timeout.
- Shared constants/helpers must stay aligned with client (`SALT`, modes, crypto header rules).
- Log rotation helper `rotate_log` for long-running SNMP/log files.

### Testing Requirements

```bash
# Unit/integration in main
cargo test -p kcptun-server

# Stress (release binaries required)
make release
make stress
# or:
cargo test --release -p kcptun-server --test stress_test -- --nocapture --test-threads=1

# Interop
bash test_e2e.sh
```

### Common Patterns

- `SmuxStreamIo` = server-side async bridge (cfg dual impl like client)
- `handle_stream` → TCP dial target → pipe
- SNMP + optional pprof HTTP (requires `--features pprof`)

## Dependencies

### Internal
`kcp-rs`, `kcrypt-rs`, `smux-rs`, `qpp-rs`, `kio-rs`

### External
Same as client + `dashmap`

<!-- MANUAL: pprof feature is optional and off by default (keeps ARM release bins small). Enable with --features pprof. -->
