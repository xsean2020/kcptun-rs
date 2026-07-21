<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# kcp-rs

## Purpose

KCP ARQ (Automatic Repeat-reQuest) reliable UDP protocol state machine — a port of Go `github.com/xtaci/kcp-go/v5`. Provides ordered, reliable delivery over UDP with congestion control, FEC (Reed-Solomon), packet-level crypto wrappers (`CryptoBuf`), and atomic SNMP counters. Crypto implementations are re-exported from `kcrypt-rs` for backward compatibility.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Deps: `bytes`, `parking_lot`, `crossbeam`, `kcrypt-rs`, `reed-solomon-erasure`, `crc32fast` |
| `src/lib.rs` | Crate root; large `#![allow(clippy::…)]` list intentional — do not "fix" |
| `src/kcp.rs` | Core `KCP` state machine (~1430 LOC): windows, RTO, flush, input, NoDelay |
| `src/segment.rs` | 24-byte LE wire header, `Command` enum, `SegmentPool` (SegQueue) |
| `src/fec.rs` | `FecEncoder` / `FecDecoder`, header types 0x00f1/0x00f2/0x00f3 |
| `src/crypto_buf.rs` | Zero-alloc CFB encrypt: nonce counter + CRC32 + reusable buffer; `prepare_encrypt` for parallel path |
| `src/session.rs` | `UDPSession` helper around KCP + UDP |
| `src/snmp.rs` | Global `DEFAULT_SNMP` atomic counters |

## Subdirectories

None (flat `src/`).

## For AI Agents

### Working In This Directory

- **Wire compatibility with kcp-go v5 is the primary constraint.** Control flow deliberately mirrors Go; crate-level clippy allows exist for that reason — do not remove them or refactor for idiomatic Rust.
- `KCP::input()` must queue ACKs for **every** received Push segment.
- `snd_buf` cleanup: ACKed segments removed from the **front** in `flush()` (Go `k.snd_buf = k.snd_buf[1:]`).
- Constants (`IKCP_RTO_*`, `IKCP_PROBE_*`, `KCP_DEFAULT_WND=32`, cmds 81–84) must match Go.
- Crypto: prefer depending on `kcrypt-rs` in new code; re-exports here are legacy.
- `CryptoBuf` nonce is **not** the CFB IV (IV is fixed `GO_CFB_IV`); nonce is `[counter 8B][session_id 8B]`.

### Testing Requirements

- Unit tests in-module where present
- Interop: `bash test_e2e.sh` after any segment/KCP/FEC change
- Stress: `make stress` for flush/lock behavior under load

### Common Patterns

- Output callback: `Box<dyn FnMut(bytes::Bytes) + Send>` set on `KCP` (R2 ownership path)
- NoDelay modes applied by binaries via `nodelay/interval/resend/nc`
- FEC optional; encoder needs `data_shards > 0 && parity_shards > 0`

## Dependencies

### Internal
- `kcrypt-rs` — BlockCrypt / AeadCrypt

### External
- `bytes`, `crossbeam`, `parking_lot`, `reed-solomon-erasure`, `crc32fast`

<!-- MANUAL: -->
