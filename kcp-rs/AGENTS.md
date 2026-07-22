<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 (inbound decrypt_cfb_in_place) -->

# kcp-rs

## Purpose

KCP ARQ (Automatic Repeat-reQuest) reliable UDP protocol state machine — port of Go `github.com/xtaci/kcp-go/v5`. Ordered, reliable delivery over UDP with congestion control, Reed-Solomon FEC, packet-level crypto framing (`CryptoBuf`), and atomic SNMP counters. Block/AEAD implementations live in `kcrypt-rs` and are **re-exported** here for backward compatibility.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Deps: `bytes`, `parking_lot`, `crossbeam`, `kcrypt-rs`, `reed-solomon-erasure`, `crc32fast` |
| `src/lib.rs` | Crate root; large intentional `#![allow(clippy::…)]` list — do not "fix" |
| `src/kcp.rs` | Core `KCP` state machine: windows, RTO, flush, input, NoDelay |
| `src/segment.rs` | 24-byte LE wire header, `Command` enum, `SegmentPool` (SegQueue) |
| `src/fec.rs` | `FecEncoder` / `FecDecoder` / `fec_expand_packets` / `fec_kcp_from_recovered`; header types `0x00f1` / `0x00f2` / `0x00f3` |
| `src/crypto_buf.rs` | CFB pack: nonce counter + CRC32; `prepare_encrypt`, `encrypt_batch`, `should_cpu_block_*`; inbound `decrypt_cfb_in_place` / `strip_cfb_header_if_present` / `inbound_null` |
| `src/session.rs` | `UDPSession` helper around KCP + UDP |
| `src/snmp.rs` | Global `DEFAULT_SNMP` atomic counters; `snmp_enable` / `snmp_add` / `snmp_store` |

## Subdirectories

None (flat `src/`).

## For AI Agents

### Working In This Directory

- **Wire compatibility with kcp-go v5 is the primary constraint.** Control flow mirrors Go; crate-level clippy allows exist for that reason.
- `KCP::input()` must queue ACKs for **every** received Push segment.
- `snd_buf` cleanup: ACKed segments removed from the **front** in `flush()` (Go `k.snd_buf = k.snd_buf[1:]`).
- Constants (`IKCP_RTO_*`, `IKCP_PROBE_*`, `KCP_DEFAULT_WND=32`, cmds 81–84) must match Go.
- Prefer depending on `kcrypt-rs` in new code; re-exports here are legacy.
- `CryptoBuf` nonce is **not** the CFB IV (IV is fixed `GO_CFB_IV`); nonce is `[counter 8B][session_id 8B]`.
- SNMP collection is **opt-in** (`snmp_enable`) so hot paths stay free when unused.

### Testing Requirements

- In-module unit tests where present
- Interop: `bash test_e2e.sh` after segment/KCP/FEC changes
- Stress: `make stress` for flush/lock behavior under load

### Common Patterns

- Output callback: `Box<dyn FnMut(bytes::Bytes) + Send>` on `KCP`
- NoDelay modes applied by binaries via `nodelay/interval/resend/nc`
- FEC optional at **session layer** only (`FecEncoder`/`FecDecoder`); no KCP-level FEC API
- Recovered FEC payload: `fec_kcp_from_recovered` (Go `r[2:sz]`); reconstruct present-flag is `true` = present
- Public re-exports: `KCP`, `CryptoBuf`, `encrypt_batch`, `decrypt_cfb_in_place`, `strip_cfb_header_if_present`, `inbound_null`, `InboundCryptError`, `CRYPT_HDR`/`NONCE_SZ`, `FecEncoder`/`FecDecoder`, `fec_expand_packets`, `fec_kcp_from_recovered`, `DEFAULT_SNMP`, `BlockCrypt`/`select_block_crypt`
- Inbound CFB hot path: use `decrypt_cfb_in_place` (no `enc_buf` copy); `CryptoBuf::decrypt_cfb` still copies into reusable buffer for callers that need owned `Bytes`

## Dependencies

### Internal

- `kcrypt-rs` — BlockCrypt / AeadCrypt

### External

- `bytes`, `crossbeam`, `parking_lot`, `reed-solomon-erasure`, `crc32fast`

<!-- MANUAL: -->
