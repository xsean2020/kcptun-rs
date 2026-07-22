//! Reed-Solomon Forward Error Correction (FEC).
//!
//! Port of Go's `github.com/xtaci/kcp-go/v5/fec.go` with full wire-level
//! compatibility.

use crate::snmp::{self as snmp, DEFAULT_SNMP};
use reed_solomon_erasure::galois_8::Field;
use reed_solomon_erasure::ReedSolomon;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// FEC header size (seqid + type = 6 bytes).
pub const FEC_HEADER_SIZE: usize = 6;
/// FEC header + 2B data size.
pub const FEC_HEADER_SIZE_PLUS_2: usize = 8;
/// FEC type: data packet.
pub const FEC_TYPE_DATA: u16 = 0x00f1;
/// FEC type: parity packet.
pub const FEC_TYPE_PARITY: u16 = 0x00f2;
/// FEC type: out-of-band data.
pub const FEC_TYPE_OOB: u16 = 0x00f3;
const MAX_SHARD_SETS: u32 = 3;

// ─── Utilities ───────────────────────────────────────────────────────────

fn current_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─── FEC Encoder ─────────────────────────────────────────────────────────

/// Reed-Solomon FEC encoder matching Go's fecEncoder.
pub struct FecEncoder {
    data_shards: usize,
    parity_shards: usize,
    shard_size: usize,
    paws: u32,
    next: u32,
    shard_count: usize,
    max_size: usize,
    header_offset: usize,
    payload_offset: usize,
    shard_cache: Vec<Vec<u8>>,
    ts_latest_packet: i64,
    codec: ReedSolomon<Field>,
}

impl FecEncoder {
    /// Create a new FEC encoder.
    /// `offset` = header_size (from encryption layer), typically 0 or 20.
    pub fn new(data_shards: usize, parity_shards: usize, offset: usize) -> Option<Self> {
        if data_shards == 0 || parity_shards == 0 {
            return None;
        }
        if data_shards + parity_shards > 256 {
            return None;
        }
        let shard_size = data_shards + parity_shards;
        let paws = 0xffffffffu32 / shard_size as u32 * shard_size as u32;
        let codec = ReedSolomon::<Field>::new(data_shards, parity_shards).ok()?;

        let mut shard_cache = Vec::with_capacity(shard_size);
        for _ in 0..shard_size {
            shard_cache.push(vec![0u8; 1500]);
        }

        Some(FecEncoder {
            data_shards,
            parity_shards,
            shard_size,
            paws,
            next: 0,
            shard_count: 0,
            max_size: 0,
            header_offset: offset,
            payload_offset: offset + FEC_HEADER_SIZE,
            shard_cache,
            ts_latest_packet: 0,
            codec,
        })
    }

    /// Feed a data packet. Returns parity packets when a full group is
    /// collected and the data is continuous (within `rto` ms since last packet).
    ///
    /// Buffer layout (matching Go):
    /// `[header_offset zeros][seq 4][type 2][size 2][kcp payload…]`
    /// SIZE = len from size field through end (includes the 2B size field).
    pub fn encode(&mut self, data: &mut [u8], rto: u32) -> Vec<Vec<u8>> {
        if self.parity_shards == 0 {
            return Vec::new();
        }

        // Seal data header (seq + type)
        self.seal_data(data);

        // Write SIZE at payload_offset (Go: PutUint16(..., len(b[payloadOffset:])))
        if data.len() >= self.payload_offset + 2 {
            let plen = (data.len() - self.payload_offset) as u16;
            data[self.payload_offset..self.payload_offset + 2].copy_from_slice(&plen.to_le_bytes());
        }

        // Copy full packet into shard cache
        let sz = data.len();
        {
            let slot = &mut self.shard_cache[self.shard_count];
            if slot.len() < sz {
                slot.resize(sz, 0);
            } else {
                slot.truncate(sz);
            }
            slot[..sz].copy_from_slice(data);
        }
        self.shard_count += 1;

        if sz > self.max_size {
            self.max_size = sz;
        }

        let now = current_ms();

        // Generate parity when we have enough data shards
        if self.shard_count == self.data_shards {
            self.shard_count = 0;
            let result = if now - self.ts_latest_packet < rto as i64 {
                self.do_encode()
            } else {
                // Non-continuous data, skip parity
                self.skip_parity();
                Vec::new()
            };
            self.max_size = 0;
            self.ts_latest_packet = now;
            return result;
        }

        self.ts_latest_packet = now;
        Vec::new()
    }

    /// Wrap a raw KCP segment: `[crypt_hdr?][fec 6][size 2][kcp]`, encode, return
    /// (data_packet, parity_packets). `header_offset` reserves crypt space (0 when
    /// crypto wraps the whole FEC frame — our session layout).
    pub fn wrap_kcp_packet(&mut self, kcp: &[u8], rto: u32) -> (Vec<u8>, Vec<Vec<u8>>) {
        let ho = self.header_offset;
        let mut pkt = vec![0u8; ho + FEC_HEADER_SIZE_PLUS_2 + kcp.len()];
        pkt[ho + FEC_HEADER_SIZE_PLUS_2..].copy_from_slice(kcp);
        let parity = self.encode(&mut pkt, rto);
        (pkt, parity)
    }

    fn do_encode(&mut self) -> Vec<Vec<u8>> {
        let max_sz = self.max_size;
        let po = self.payload_offset;

        // Pad data shards to max_sz (Go: clear tail)
        for i in 0..self.data_shards {
            let slen = self.shard_cache[i].len();
            if slen < max_sz {
                self.shard_cache[i].resize(max_sz, 0u8);
            }
        }
        // Allocate parity shard buffers
        for i in self.data_shards..self.shard_size {
            self.shard_cache[i] = vec![0u8; max_sz];
        }

        // RS encode only payload region [payload_offset..max_sz] (matching Go)
        let mut slices: Vec<&mut [u8]> = self
            .shard_cache
            .iter_mut()
            .map(|s| &mut s[po..max_sz])
            .collect();
        if self.codec.encode(&mut slices).is_err() {
            self.skip_parity();
            snmp::add(&DEFAULT_SNMP.fec_errs, 1);
            return Vec::new();
        }

        let mut result = Vec::with_capacity(self.parity_shards);
        for i in 0..self.parity_shards {
            let idx = self.data_shards + i;
            self.seal_parity(idx);
            result.push(self.shard_cache[idx][..max_sz].to_vec());
        }
        result
    }

    fn seal_data(&mut self, data: &mut [u8]) {
        if data.len() < self.header_offset + 6 {
            return;
        }
        let ho = self.header_offset;
        data[ho..ho + 4].copy_from_slice(&self.next.to_le_bytes());
        data[ho + 4..ho + 6].copy_from_slice(&FEC_TYPE_DATA.to_le_bytes());
        self.next = (self.next + 1) % self.paws;
    }

    fn seal_parity(&mut self, index: usize) {
        let ho = self.header_offset;
        if self.shard_cache[index].len() < ho + 6 {
            return;
        }
        self.shard_cache[index][ho..ho + 4].copy_from_slice(&self.next.to_le_bytes());
        self.shard_cache[index][ho + 4..ho + 6].copy_from_slice(&FEC_TYPE_PARITY.to_le_bytes());
        self.next = (self.next + 1) % self.paws;
    }

    fn skip_parity(&mut self) {
        self.next = (self.next + self.parity_shards as u32) % self.paws;
    }
}

// ─── FEC Decoder ─────────────────────────────────────────────────────────

/// Min-heap wrapper for FEC packets (matching Go's shardHeap).
struct ShardEntry {
    seqid: u32,
    data: Vec<u8>,
}

impl PartialEq for ShardEntry {
    fn eq(&self, other: &Self) -> bool {
        self.seqid == other.seqid
    }
}
impl Eq for ShardEntry {}
impl PartialOrd for ShardEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ShardEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap: lower seqid first
        other.seqid.cmp(&self.seqid)
    }
}

struct ShardHeap {
    elements: BinaryHeap<Reverse<ShardEntry>>,
    marks: HashMap<u32, ()>,
}

impl ShardHeap {
    fn new() -> Self {
        ShardHeap {
            elements: BinaryHeap::new(),
            marks: HashMap::new(),
        }
    }
    fn has(&self, seqid: u32) -> bool {
        self.marks.contains_key(&seqid)
    }
    fn push(&mut self, pkt: Vec<u8>) {
        if pkt.len() < 4 {
            return;
        }
        let seqid = u32::from_le_bytes(pkt[..4].try_into().unwrap());
        self.marks.insert(seqid, ());
        self.elements.push(Reverse(ShardEntry { seqid, data: pkt }));
    }
    fn len(&self) -> usize {
        self.elements.len()
    }
    fn pop_all(&mut self) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        while let Some(Reverse(entry)) = self.elements.pop() {
            self.marks.remove(&entry.seqid);
            result.push(entry.data);
        }
        result
    }
}

/// Simple FEC auto-tuning (matching Go's autoTune).
struct AutoTune {
    pulses: Vec<(u32, bool)>, // (seqid, is_data)
    max_samples: usize,
}

impl AutoTune {
    fn new() -> Self {
        AutoTune {
            pulses: Vec::new(),
            max_samples: 258,
        }
    }

    fn sample(&mut self, is_data: bool, seq: u32) {
        self.pulses.push((seq, is_data));
        if self.pulses.len() > self.max_samples {
            self.pulses.remove(0);
        }
    }

    fn find_period(&self, bit: bool) -> usize {
        if self.pulses.len() < 3 {
            return 0;
        }
        let mut sorted = self.pulses.clone();
        sorted.sort_by_key(|&(seq, _)| seq);

        // Check continuity
        for i in 1..sorted.len() {
            let diff = (sorted[i].0 as i64) - (sorted[i - 1].0 as i64);
            if diff != 1 {
                return 0;
            }
        }

        // Find left edge (transition from !bit to bit)
        let mut left_edge = None;
        for i in 1..sorted.len() {
            if sorted[i - 1].1 != bit && sorted[i].1 == bit {
                left_edge = Some(i);
                break;
            }
        }
        let left = match left_edge {
            Some(i) => i,
            None => return 0,
        };

        // Find right edge (transition from bit to !bit)
        for i in left + 1..sorted.len() {
            if sorted[i - 1].1 == bit && sorted[i].1 != bit {
                return i - left;
            }
        }
        0
    }
}

/// Reed-Solomon FEC decoder matching Go's fecDecoder.
pub struct FecDecoder {
    data_shards: usize,
    parity_shards: usize,
    shard_size: usize,
    paws: u32,
    shard_set: HashMap<u32, ShardHeap>,
    newest_shard_id: u32,
    decode_cache: Vec<Vec<u8>>,
    flag_cache: Vec<bool>,
    codec: ReedSolomon<Field>,
    auto_tune: AutoTune,
    should_tune: bool,
}

impl FecDecoder {
    /// Create a new FEC decoder.
    pub fn new(data_shards: usize, parity_shards: usize) -> Option<Self> {
        if data_shards <= 0 || parity_shards <= 0 {
            return None;
        }
        if data_shards + parity_shards > 256 {
            return None;
        }
        let shard_size = data_shards + parity_shards;
        let paws = 0xffffffffu32 / shard_size as u32 * shard_size as u32;
        let codec = ReedSolomon::<Field>::new(data_shards, parity_shards).ok()?;
        Some(FecDecoder {
            data_shards,
            parity_shards,
            shard_size,
            paws,
            shard_set: HashMap::new(),
            newest_shard_id: 0,
            decode_cache: vec![Vec::new(); shard_size],
            flag_cache: vec![false; shard_size],
            codec,
            auto_tune: AutoTune::new(),
            should_tune: false,
        })
    }

    /// Feed a packet. Returns recovered data payloads.
    pub fn decode(&mut self, pkt: &[u8]) -> Vec<Vec<u8>> {
        if pkt.len() < 6 {
            return Vec::new();
        }
        let seqid = u32::from_le_bytes(pkt[..4].try_into().unwrap());
        let flag = u16::from_le_bytes(pkt[4..6].try_into().unwrap());

        // Auto-tune sampling
        if flag == FEC_TYPE_DATA {
            self.auto_tune.sample(true, seqid);
        } else {
            self.auto_tune.sample(false, seqid);
            if flag == FEC_TYPE_PARITY {
                snmp::add(&DEFAULT_SNMP.fec_parity_shards, 1);
            }
        }

        // Check paws
        if seqid >= self.paws {
            return Vec::new();
        }

        // Check if packet type matches expected FEC parameters
        let idx_in_shard = seqid % self.shard_size as u32;
        if idx_in_shard < self.data_shards as u32 {
            if flag != FEC_TYPE_DATA {
                self.should_tune = true;
            }
        } else {
            if flag != FEC_TYPE_PARITY {
                self.should_tune = true;
            }
        }

        // Auto-tune if needed
        if self.should_tune {
            let auto_ds = self.auto_tune.find_period(true);
            let auto_ps = self.auto_tune.find_period(false);
            if auto_ds > 0
                && auto_ps > 0
                && auto_ds + auto_ps < 256
                && (auto_ds != self.data_shards || auto_ps != self.parity_shards)
            {
                self.data_shards = auto_ds;
                self.parity_shards = auto_ps;
                self.shard_size = auto_ds + auto_ps;
                self.shard_set.clear();
                if let Ok(codec) = ReedSolomon::<Field>::new(auto_ds, auto_ps) {
                    self.codec = codec;
                }
                self.decode_cache = vec![Vec::new(); self.shard_size];
                self.flag_cache = vec![false; self.shard_size];
                self.paws = 0xffffffffu32 / self.shard_size as u32 * self.shard_size as u32;
                self.should_tune = false;
            }
            return Vec::new();
        }

        // Get shard heap
        let shard_id = seqid / self.shard_size as u32;
        let shard = self
            .shard_set
            .entry(shard_id)
            .or_insert_with(ShardHeap::new);

        // Ignore duplicates
        if shard.has(seqid) {
            return Vec::new();
        }

        // Push packet
        shard.push(pkt.to_vec());

        // Try to recover when we have enough shards
        if shard.len() >= self.data_shards {
            snmp::add(&DEFAULT_SNMP.fec_full_shards, 1);
            let pkts = shard.pop_all();
            return self.recover(pkts, shard_id);
        }

        // Update newest shard id and discard old
        if shard_id > self.newest_shard_id {
            self.newest_shard_id = shard_id;
        }
        self.discard_old();
        snmp::store(&DEFAULT_SNMP.fec_shard_min, self.newest_shard_id as u64);
        snmp::store(&DEFAULT_SNMP.fec_shard_set, self.shard_set.len() as u64);
        Vec::new()
    }

    fn recover(&mut self, mut pkts: Vec<Vec<u8>>, shard_id: u32) -> Vec<Vec<u8>> {
        if pkts.is_empty() {
            return self.cleanup(shard_id, Vec::new());
        }

        // Sort by seqid
        pkts.sort_by_key(|e| u32::from_le_bytes(e[..4].try_into().unwrap_or([0; 4])));

        let mut max_plen = 0;
        for e in &pkts {
            let plen = if e.len() > 6 { e[6..].len() } else { 0 };
            if plen > max_plen {
                max_plen = plen;
            }
        }

        // Prepare shards and flags
        for k in 0..self.shard_size {
            self.decode_cache[k].clear();
            self.flag_cache[k] = false;
        }

        let mut present = vec![false; self.shard_size];
        for e in &pkts {
            let sid = u32::from_le_bytes(e[..4].try_into().unwrap_or([0; 4]));
            let idx = (sid % self.shard_size as u32) as usize;
            if idx < self.shard_size {
                present[idx] = true;
            }
        }

        // Check if all data shards are already present
        let all_data_present = (0..self.data_shards).all(|i| present[i]);
        if all_data_present || pkts.len() < self.data_shards {
            return self.cleanup(shard_id, Vec::new());
        }

        // Fill decode cache from packets
        for e in &pkts {
            let sid = u32::from_le_bytes(e[..4].try_into().unwrap_or([0; 4]));
            let idx = (sid % self.shard_size as u32) as usize;
            if idx < self.shard_size {
                let payload = if e.len() > 6 { &e[6..] } else { &[] };
                let mut d = payload.to_vec();
                d.resize(max_plen, 0u8);
                self.decode_cache[idx] = d;
                self.flag_cache[idx] = true;
            }
        }

        // Fill missing shards with empty buffers
        let mut new_buffers: Vec<Vec<u8>> = Vec::new();
        for k in 0..self.shard_size {
            if !self.flag_cache[k] && k < self.data_shards {
                let buf = vec![0u8; max_plen];
                new_buffers.push(buf.clone());
                self.decode_cache[k] = buf;
            } else if !self.flag_cache[k] {
                self.decode_cache[k] = vec![0u8; max_plen];
            }
        }

        // ReconstructShard for (T, bool): true = present, false = missing
        // (reed-solomon-erasure: len() is None when bool is false).
        let mut shards: Vec<(&mut [u8], bool)> = Vec::with_capacity(self.shard_size);
        for (i, s) in self.decode_cache.iter_mut().enumerate() {
            shards.push((s.as_mut_slice(), present[i]));
        }
        let recovered = if self.codec.reconstruct_data(&mut shards).is_ok() {
            let rec: Vec<Vec<u8>> = (0..self.data_shards)
                .filter(|&i| !present[i])
                .map(|i| self.decode_cache[i].clone())
                .collect();
            if !rec.is_empty() {
                snmp::add(&DEFAULT_SNMP.fec_recovered, rec.len() as u64);
            }
            rec
        } else {
            snmp::add(&DEFAULT_SNMP.fec_errs, 1);
            Vec::new()
        };

        self.cleanup(shard_id, recovered)
    }

    fn cleanup(&mut self, shard_id: u32, recovered: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        if shard_id > self.newest_shard_id {
            self.newest_shard_id = shard_id;
        }
        self.discard_old();
        recovered
    }

    fn discard_old(&mut self) {
        let min_id = self.newest_shard_id.saturating_sub(MAX_SHARD_SETS);
        self.shard_set.retain(|&id, _| id >= min_id);
    }
}

// ─── Utilities ───────────────────────────────────────────────────────────

/// Parse FEC header from raw data.
#[inline]
pub fn parse_fec_header(data: &[u8]) -> Option<(u32, u16)> {
    if data.len() < FEC_HEADER_SIZE {
        return None;
    }
    let seq = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let fec_type = u16::from_le_bytes(data[4..6].try_into().ok()?);
    Some((seq, fec_type))
}

#[inline]
pub fn is_data_packet(data: &[u8]) -> bool {
    if data.len() < FEC_HEADER_SIZE {
        return true;
    }
    let t = u16::from_le_bytes(data[4..6].try_into().unwrap_or([0; 2]));
    t == FEC_TYPE_DATA
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// Expand raw KCP segments into FEC data frames (+ parity when a group fills).
/// Each packet is ready for crypto: `[FEC 6][SIZE 2][KCP…]` when header_offset=0.
pub fn fec_expand_packets(
    encoder: &mut FecEncoder,
    packets: &[bytes::Bytes],
    rto_ms: u32,
) -> Vec<bytes::Bytes> {
    let mut out = Vec::with_capacity(packets.len() + packets.len() / 2 + 4);
    for kcp in packets {
        let (data, parity) = encoder.wrap_kcp_packet(kcp, rto_ms);
        out.push(bytes::Bytes::from(data));
        for p in parity {
            out.push(bytes::Bytes::from(p));
        }
    }
    out
}

/// Strip the 2B SIZE field from a recovered FEC payload and return the KCP segment.
///
/// Matches Go `kcpInput` recovered handling:
/// ```text
/// sz := binary.LittleEndian.Uint16(r)
/// if int(sz) <= len(r) && sz >= 2 { kcp.Input(r[2:sz], …) }
/// ```
/// SIZE includes the 2B field itself; RS recovery may pad beyond SIZE — must not
/// feed that pad into KCP.
#[inline]
pub fn fec_kcp_from_recovered(r: &[u8]) -> Option<&[u8]> {
    if r.len() < 2 {
        return None;
    }
    let sz = u16::from_le_bytes([r[0], r[1]]) as usize;
    if sz >= 2 && sz <= r.len() {
        Some(&r[2..sz])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fec_header_parse() {
        let mut h = vec![0u8; 6];
        h[..4].copy_from_slice(&42u32.to_le_bytes());
        h[4..6].copy_from_slice(&FEC_TYPE_DATA.to_le_bytes());
        assert_eq!(parse_fec_header(&h), Some((42, FEC_TYPE_DATA)));
    }

    #[test]
    fn fec_short() {
        assert!(parse_fec_header(&[0u8; 3]).is_none());
    }

    #[test]
    fn fec_data_type() {
        let mut h = vec![0u8; 10];
        h[4..6].copy_from_slice(&FEC_TYPE_DATA.to_le_bytes());
        assert!(is_data_packet(&h));
        h[4..6].copy_from_slice(&FEC_TYPE_PARITY.to_le_bytes());
        assert!(!is_data_packet(&h));
    }

    #[test]
    fn fec_enc_dec_create() {
        assert!(FecEncoder::new(10, 3, 20).is_some());
        assert!(FecDecoder::new(10, 3).is_some());
    }

    #[test]
    fn fec_encoder_generates_parity() {
        let mut enc = FecEncoder::new(3, 2, 0).unwrap();
        for i in 0..3 {
            let mut pkt = vec![0u8; 16];
            pkt[8..16].copy_from_slice(&[i as u8; 8]);
            let result = enc.encode(&mut pkt, 1000);
            if i == 2 {
                assert_eq!(result.len(), 2, "Should produce 2 parity packets");
            }
        }
    }

    #[test]
    fn fec_roundtrip() {
        let mut enc = FecEncoder::new(3, 2, FEC_HEADER_SIZE).unwrap();
        let mut dec = FecDecoder::new(3, 2).unwrap();
        let mut data_pkts = Vec::new();
        let mut parity_pkts = Vec::new();

        for i in 0..3 {
            let mut pkt = vec![0u8; 28];
            pkt[8..28].copy_from_slice(&[i as u8; 20]);
            let p = enc.encode(&mut pkt, 1000);
            parity_pkts = p;
            data_pkts.push(pkt);
        }

        // Feed all data + parity
        for p in &data_pkts {
            dec.decode(p);
        }
        for p in &parity_pkts {
            dec.decode(p);
        }

        // Test recovery: feed 2 data + 2 parity
        let mut dec2 = FecDecoder::new(3, 2).unwrap();
        for i in 1..3 {
            dec2.decode(&data_pkts[i]);
        }
        for p in &parity_pkts {
            dec2.decode(p);
        }
        let _recovered = dec2.decode(&parity_pkts[0]);
    }

    #[test]
    fn fec_wrap_generates_parity_and_roundtrip_headers() {
        let mut enc = FecEncoder::new(3, 2, 0).unwrap();
        let mut data_frames = Vec::new();
        let mut parity_frames = Vec::new();
        for i in 0..3u8 {
            let kcp = {
                let mut v = vec![0u8; 32];
                v[0..4].copy_from_slice(&1u32.to_le_bytes());
                v[4] = 81;
                v[24..32].fill(i + 1);
                v
            };
            let (data, parity) = enc.wrap_kcp_packet(&kcp, 1000);
            let size = u16::from_le_bytes(data[6..8].try_into().unwrap()) as usize;
            assert_eq!(size, data.len() - 6);
            assert_eq!(&data[8..], &kcp[..]);
            assert_eq!(
                u16::from_le_bytes(data[4..6].try_into().unwrap()),
                FEC_TYPE_DATA
            );
            data_frames.push(data);
            if i == 2 {
                assert_eq!(parity.len(), 2);
                for p in &parity {
                    assert_eq!(
                        u16::from_le_bytes(p[4..6].try_into().unwrap()),
                        FEC_TYPE_PARITY
                    );
                    assert_eq!(p.len(), data_frames[0].len());
                }
                parity_frames = parity;
            } else {
                assert!(parity.is_empty());
            }
        }
        assert_eq!(data_frames.len(), 3);
        assert_eq!(parity_frames.len(), 2);
        // Seqids should be 0,1,2 for data and 3,4 for parity
        for (i, f) in data_frames.iter().enumerate() {
            let seq = u32::from_le_bytes(f[0..4].try_into().unwrap());
            assert_eq!(seq, i as u32);
        }
        for (i, f) in parity_frames.iter().enumerate() {
            let seq = u32::from_le_bytes(f[0..4].try_into().unwrap());
            assert_eq!(seq, 3 + i as u32);
        }
    }

    #[test]
    fn fec_recover_strips_size_and_rs_pad() {
        // Variable-length KCP payloads so RS pads shorter shards; recovered
        // path must use SIZE (Go r[2:sz]), not the whole padded buffer.
        let mut enc = FecEncoder::new(3, 2, 0).unwrap();
        let mut data_frames = Vec::new();
        let mut parity_frames = Vec::new();
        let payloads: [&[u8]; 3] = [
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", // 32
            b"bbbbbbbbbbbbbbbb",                 // 16 — shorter
            b"cccccccccccccccccccccccc",         // 24
        ];
        for (i, kcp) in payloads.iter().enumerate() {
            let (data, parity) = enc.wrap_kcp_packet(kcp, 1000);
            data_frames.push(data);
            if i == 2 {
                assert_eq!(parity.len(), 2);
                parity_frames = parity;
            }
        }
        // Drop data shard 0; recover from data1 + data2 + both parity.
        let mut dec = FecDecoder::new(3, 2).unwrap();
        assert!(dec.decode(&data_frames[1]).is_empty());
        assert!(dec.decode(&data_frames[2]).is_empty());
        // Third present shard triggers reconstruct; either parity works.
        let mut recovered = dec.decode(&parity_frames[0]);
        if recovered.is_empty() {
            recovered = dec.decode(&parity_frames[1]);
        }
        assert_eq!(recovered.len(), 1, "expected data shard 0 recovered");
        let kcp = fec_kcp_from_recovered(&recovered[0]).expect("valid SIZE");
        assert_eq!(kcp, payloads[0]);
        // Without SIZE trim, RS pad would make the slice longer than original.
        assert!(recovered[0].len() >= kcp.len() + 2);
        assert_eq!(kcp.len(), payloads[0].len());
    }

    #[test]
    fn fec_kcp_from_recovered_rejects_bad_size() {
        assert!(fec_kcp_from_recovered(&[]).is_none());
        assert!(fec_kcp_from_recovered(&[1]).is_none());
        // sz=1 < 2
        assert!(fec_kcp_from_recovered(&[1, 0, 9, 9]).is_none());
        // sz > len
        assert!(fec_kcp_from_recovered(&[10, 0, 1, 2]).is_none());
        // valid: sz=4 → payload [0xAA, 0xBB]
        assert_eq!(
            fec_kcp_from_recovered(&[4, 0, 0xAA, 0xBB]),
            Some(&[0xAAu8, 0xBB][..])
        );
    }
}
