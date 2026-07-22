# FEC send path was a stub (fixed)

## Status

**Fixed** (session-layer FecEncoder/FecDecoder wired on client + server).

Follow-up (same feature):

- Recovered payload uses Go `r[2:sz]` via `fec_kcp_from_recovered` (not bare `r[2..]`).
- Decoder `reconstruct_data` present-flag polarity fixed (`true` = present).

## Symptom

CLI defaults `datashard=10` / `parityshard=3` matched Go, but Rust:

- Session-layer `FecEncoder` only used in unit tests (no send-path wiring)
- Client had no real `FecDecoder` recovery path
- (A dead `KCP::set_fec` no-op existed and was later removed; FEC is session-layer only)

So Rust did not emit parity shards; Go↔Rust FEC interop and loss recovery were incomplete.

## Fix

- Encode after KCP flush (and ACK drain): `fec_expand_packets` → encrypt → UDP
- Decode on client UDP path like server `feed_data`
- Encoder SIZE/parity/header_offset aligned with Go kcp-go v5
- Recovered KCP: `fec_kcp_from_recovered` (SIZE-aware); reconstruct present bool fixed

## Verify

```bash
cargo test -p kcp-rs fec
# default FEC on
kcptun-server ... --crypt null --mode fast
kcptun-client ... --crypt null --mode fast
# SNMP should show FECParityShards / FECFullShards > 0 with --snmplog

# explicit off
--datashard 0 --parityshard 0

# e2e section 6
bash test_e2e.sh   # or make e2e
```
