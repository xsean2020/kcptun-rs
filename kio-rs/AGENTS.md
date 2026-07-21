<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# kio-rs

## Purpose

Async runtime + network I/O abstraction for kcptun. Business code calls `kio::*` without knowing whether **tokio** or **smol** is active. Features are mutually exclusive; exactly one must be enabled.

Lib name on crates.io path is package `kio-rs`; Rust crate name is `kio` (`[lib] name = "kio"`).

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Optional tokio / smol stacks; `default = ["tokio"]` |
| `src/lib.rs` | Feature guards, trait re-exports, `copy_bidirectional`, `copy_bidirectional_idle` (64KB buffers), `read_to_string` |
| `src/tests.rs` | Cross-backend tests |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/net/` | `TcpListener`, `TcpStream`, `UdpSocket` (see `src/net/AGENTS.md`) |
| `src/sync/` | `Notify` abstraction |
| `src/task/` | `spawn_task`, `block_on`, `cpu_block`, `JoinHandle` |
| `src/time/` | `sleep`, `sleep_ms`, `timeout`, `Elapsed` |

## For AI Agents

### Working In This Directory

- **Never enable both features.** `compile_error!` if both or neither.
- Tokio and smol expose **different** `AsyncRead`/`AsyncWrite` traits — re-export the right one; dual impls in binaries must be `cfg`-gated.
- `copy_bidirectional` uses **64 KB** buffers (Go parity). Do not switch to tokio's 8 KB helper.
- `copy_bidirectional_idle` is an **idle** timeout (reset on data), matching Go `closeWait` — not a total duration limit. Smol path uses `poll_fn` + timer (not total-timeout wrapper).
- `cpu_block`: offload CPU work (Snappy, crypto batches). Smol backend uses a **persistent** thread pool (not short-lived `smol::unblock`).
- New runtime APIs: add to both `tokio.rs` and `smol.rs` modules under the relevant subdirectory, then re-export from `mod.rs`.

### Testing Requirements

```bash
cargo test -p kio-rs
# and with smol:
cargo test -p kio-rs --no-default-features --features smol --target-dir target/smol
make test-both
```

### Common Patterns

```rust
kio::spawn_task(async move { ... });
kio::sleep_ms(10).await;
kio::cpu_block(|| expensive()).await;
kio::copy_bidirectional_idle(&mut a, &mut b, idle_secs).await?;
```

## Dependencies

### Internal
None (foundation crate for async).

### External
Shared: `async-lock`, `async-channel`, `socket2`, `log`, `libc`  
Tokio feature: `tokio`  
Smol feature: `smol`, `async-io`, `async-executor`, `futures-lite`, `num_cpus`, `event-listener`

<!-- MANUAL: -->
