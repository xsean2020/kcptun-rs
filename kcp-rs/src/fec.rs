//! Reed-Solomon Forward Error Correction (FEC).
//!
//! Port of Go's `github.com/xtaci/kcp-go/v5/fec.go` with full wire-level
//! compatibility.

use crate::snmp::{self as snmp, DEFAULT_SNMP};
use bytes::Bytes;
use reed_solomon_erasure::galois_8::Field;
use reed_solomon_erasure::ReedSolomon;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::{HashMap, VecDeque};

/// FEC header size (seqid + type = 6 bytes).
pub const FEC_HEADER_SIZE: usize = 6;
/// FEC header + 2B data size.
pub(crate) const FEC_HEADER_SIZE_PLUS_2: usize = 8;
/// FEC type: data packet.
pub const FEC_TYPE_DATA: u16 = 0x00f1;
/// FEC type: parity packet.
pub const FEC_TYPE_PARITY: u16 = 0x00f2;
const MAX_SHARD_SETS: u32 = 3;

// ─── Utilities ───────────────────────────────────────────────────────────

/// Wall-clock ms since UNIX_EPOCH — shared cached clock (`crate::kcp::wall_ms`),
/// one `SystemTime::now()` at first use then `Instant::elapsed()` per call: no
/// epoch-conversion syscall on the per-packet FEC encode path.
#[inline]
fn current_ms() -> i64 {
    crate::kcp::wall_ms() as i64
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
    ///
    /// Allocation note: `vec![0u8; n]` is `alloc_zeroed` (calloc) — the payload
    /// region is fully overwritten below, and macOS/Linux allocators serve
    /// calloc from pre-zeroed pages, so the full-frame zeroing is free. A/B
    /// micro-benchmarks (examples/fec_encode_bench, since removed) showed no
    /// win for with_capacity+resize+extend over this form; keep the calloc.
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

        // Pad data shards to max_sz (Go: clear tail) — reuse existing buffers
        for i in 0..self.data_shards {
            let slen = self.shard_cache[i].len();
            if slen < max_sz {
                self.shard_cache[i].resize(max_sz, 0u8);
            }
        }
        // Reuse parity shard buffers instead of allocating new ones — avoids
        // a malloc + memset per FEC group (measured ~99KB cumulative in pprof).
        for i in self.data_shards..self.shard_size {
            if self.shard_cache[i].len() < max_sz {
                self.shard_cache[i].resize(max_sz, 0u8);
            } else {
                self.shard_cache[i].truncate(max_sz);
            }
            // Zero-fill the buffer (required for RS encoding)
            self.shard_cache[i].fill(0);
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

        // Copy each parity shard out. The `to_vec` allocates + copies once per
        // parity packet; it cannot be avoided with `Vec<Vec<u8>>` output because
        // `shard_cache[idx]` is recycled for the next group while the packet is
        // still in flight (callers get ownership via `Bytes::from(Vec)` — a
        // free move, no second copy).
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
    data: bytes::Bytes,
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
    fn has_all_data(&self, base_seqid: u32, data_shards: usize) -> bool {
        (0..data_shards).all(|idx| self.has(base_seqid + idx as u32))
    }
    fn push(&mut self, pkt: Bytes) {
        if pkt.len() < 4 {
            return;
        }
        let seqid = u32::from_le_bytes([pkt[0], pkt[1], pkt[2], pkt[3]]);
        self.marks.insert(seqid, ());
        self.elements.push(Reverse(ShardEntry { seqid, data: pkt }));
    }
    fn len(&self) -> usize {
        self.elements.len()
    }
    fn pop_all(&mut self) -> Vec<Bytes> {
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
    pulses: VecDeque<(u32, bool)>, // (seqid, is_data)
    max_samples: usize,
}

impl AutoTune {
    fn new() -> Self {
        AutoTune {
            pulses: VecDeque::with_capacity(258),
            max_samples: 258,
        }
    }

    fn sample(&mut self, is_data: bool, seq: u32) {
        self.pulses.push_back((seq, is_data));
        if self.pulses.len() > self.max_samples {
            self.pulses.pop_front();
        }
    }

    fn find_period(&self, bit: bool, paws: u32) -> usize {
        if self.pulses.len() < 3 {
            return 0;
        }
        let mut sorted = self.pulses.clone();
        sorted.make_contiguous().sort_by_key(|&(seq, _)| seq);

        // Sequence IDs wrap at PAWS. Rotate the sorted history across the
        // largest cyclic gap, which is the seam between the oldest and newest
        // samples, then require every adjacent sample to be contiguous.
        let mut largest_gap = 0;
        let mut largest_gap_at = 0;
        for i in 0..sorted.len() {
            let a = sorted[i].0;
            let b = sorted[(i + 1) % sorted.len()].0;
            let gap = if b >= a { b - a } else { paws - a + b };
            if gap > largest_gap {
                largest_gap = gap;
                largest_gap_at = i;
            }
        }
        let start = (largest_gap_at + 1) % sorted.len();
        let ordered: Vec<(u32, bool)> = (0..sorted.len())
            .map(|offset| sorted[(start + offset) % sorted.len()])
            .collect();
        for i in 1..ordered.len() {
            let prev = ordered[i - 1].0;
            let current = ordered[i].0;
            let diff = if current >= prev {
                current - prev
            } else {
                paws - prev + current
            };
            if diff != 1 {
                return 0;
            }
        }

        // Find left edge (transition from !bit to bit)
        let mut left_edge = None;
        for i in 1..ordered.len() {
            if ordered[i - 1].1 != bit && ordered[i].1 == bit {
                left_edge = Some(i);
                break;
            }
        }
        let left = match left_edge {
            Some(i) => i,
            None => return 0,
        };

        // Find right edge (transition from bit to !bit)
        for i in left + 1..ordered.len() {
            if ordered[i - 1].1 == bit && ordered[i].1 != bit {
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
    newest_shard_id: Option<u32>,
    completed_shards: HashMap<u32, ()>,
    decode_cache: Vec<Vec<u8>>,
    flag_cache: Vec<bool>,
    codec: ReedSolomon<Field>,
    auto_tune: AutoTune,
    should_tune: bool,
}

impl FecDecoder {
    /// Create a new FEC decoder.
    pub fn new(data_shards: usize, parity_shards: usize) -> Option<Self> {
        if data_shards == 0 || parity_shards == 0 {
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
            newest_shard_id: None,
            completed_shards: HashMap::new(),
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
        // Safe slice reads: length already checked (>= 6).
        let seqid = u32::from_le_bytes([pkt[0], pkt[1], pkt[2], pkt[3]]);
        let flag = u16::from_le_bytes([pkt[4], pkt[5]]);

        // Check paws
        if seqid >= self.paws {
            return Vec::new();
        }

        // Only valid FEC types participate in auto-tuning. Unknown flags are
        // malformed packets, not parity samples; accepting them here would
        // make an attacker able to perturb the inferred shard dimensions.
        let is_data = match flag {
            FEC_TYPE_DATA => true,
            FEC_TYPE_PARITY => {
                snmp::add(&DEFAULT_SNMP.fec_parity_shards, 1);
                false
            }
            _ => return Vec::new(),
        };
        self.auto_tune.sample(is_data, seqid);

        // Check if packet type matches expected FEC parameters
        let idx_in_shard = seqid % self.shard_size as u32;
        if idx_in_shard < self.data_shards as u32 {
            if flag != FEC_TYPE_DATA {
                self.should_tune = true;
            }
        } else if flag != FEC_TYPE_PARITY {
            self.should_tune = true;
        }

        // Auto-tune if needed
        if self.should_tune {
            let auto_ds = self.auto_tune.find_period(true, self.paws);
            let auto_ps = self.auto_tune.find_period(false, self.paws);
            if auto_ds > 0
                && auto_ps > 0
                && auto_ds + auto_ps <= 256
                && (auto_ds != self.data_shards || auto_ps != self.parity_shards)
            {
                // Build all replacement state before swapping dimensions. If
                // Reed-Solomon rejects the inferred dimensions, retain the
                // current decoder rather than leaving it half-retuned.
                if let Ok(codec) = ReedSolomon::<Field>::new(auto_ds, auto_ps) {
                    let shard_size = auto_ds + auto_ps;
                    let paws = 0xffffffffu32 / shard_size as u32 * shard_size as u32;
                    self.codec = codec;
                    self.data_shards = auto_ds;
                    self.parity_shards = auto_ps;
                    self.shard_size = shard_size;
                    self.paws = paws;
                    self.shard_set.clear();
                    self.completed_shards.clear();
                    self.newest_shard_id = None;
                    self.decode_cache = vec![Vec::new(); shard_size];
                    self.flag_cache = vec![false; shard_size];
                    self.should_tune = false;
                }
            }
            return Vec::new();
        }

        let shard_id = seqid / self.shard_size as u32;
        if !self.observe_shard(shard_id) {
            return Vec::new();
        }
        if self.completed_shards.contains_key(&shard_id) {
            // A group which already had enough shards has been retired. Late
            // parity/data must not recreate it and repeatedly trigger work.
            return Vec::new();
        }

        // Get shard heap
        let ready = {
            let shard = self
                .shard_set
                .entry(shard_id)
                .or_insert_with(ShardHeap::new);

            // Ignore duplicates
            if shard.has(seqid) {
                return Vec::new();
            }

            // Push packet. The `&[u8]` API forces one owned copy here — the
            // burst-slot datagram buffers above are reused by the input loop
            // and cannot be handed to the decoder. (Not zero-copy despite the
            // buffer-reuse elsewhere.)
            shard.push(Bytes::copy_from_slice(pkt));

            // Try to recover when we have enough shards
            if shard.len() >= self.data_shards {
                snmp::add(&DEFAULT_SNMP.fec_full_shards, 1);
                let base_seqid =
                    ((shard_id as u64 * self.shard_size as u64) % self.paws as u64) as u32;
                let all_data_present = shard.has_all_data(base_seqid, self.data_shards);
                Some((shard.pop_all(), all_data_present))
            } else {
                None
            }
        };

        if let Some((pkts, all_data_present)) = ready {
            if all_data_present {
                self.complete_group(shard_id);
                return Vec::new();
            }
            return self.recover(pkts, shard_id);
        }

        self.discard_old();
        if let Some(newest) = self.newest_shard_id {
            snmp::store(&DEFAULT_SNMP.fec_shard_min, newest as u64);
        }
        snmp::store(&DEFAULT_SNMP.fec_shard_set, self.shard_set.len() as u64);
        Vec::new()
    }

    fn recover(&mut self, mut pkts: Vec<Bytes>, shard_id: u32) -> Vec<Vec<u8>> {
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

        // Fill decode cache from packets — reuse buffers to avoid allocation
        for e in &pkts {
            let sid = u32::from_le_bytes(e[..4].try_into().unwrap_or([0; 4]));
            let idx = (sid % self.shard_size as u32) as usize;
            if idx < self.shard_size {
                let payload = if e.len() > 6 { &e[6..] } else { &[] };
                // Reuse existing buffer: clear, extend from payload, then resize
                let slot = &mut self.decode_cache[idx];
                slot.clear();
                slot.extend_from_slice(payload);
                if slot.len() < max_plen {
                    slot.resize(max_plen, 0u8);
                }
                self.flag_cache[idx] = true;
            }
        }

        // Fill missing shards with empty buffers — reuse via clear+resize
        for k in 0..self.shard_size {
            if !self.flag_cache[k] {
                let slot = &mut self.decode_cache[k];
                slot.clear();
                slot.resize(max_plen, 0u8);
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
        self.complete_group(shard_id);
        self.discard_old();
        recovered
    }

    fn complete_group(&mut self, shard_id: u32) {
        self.shard_set.remove(&shard_id);
        self.completed_shards.insert(shard_id, ());
    }

    fn group_count(&self) -> u32 {
        self.paws / self.shard_size as u32
    }

    /// Forward distance on the PAWS-sized group ring.
    fn group_distance(&self, newer: u32, older: u32) -> u32 {
        let groups = self.group_count();
        if newer >= older {
            newer - older
        } else {
            newer + groups - older
        }
    }

    fn group_is_newer(&self, candidate: u32, reference: u32) -> bool {
        let distance = self.group_distance(candidate, reference);
        distance != 0 && distance <= self.group_count() / 2
    }

    /// Accept a group only while it is near the newest observed group. This
    /// keeps the shard/tombstone maps bounded and remains correct at PAWS wrap.
    fn observe_shard(&mut self, shard_id: u32) -> bool {
        match self.newest_shard_id {
            None => self.newest_shard_id = Some(shard_id),
            Some(newest) if self.group_is_newer(shard_id, newest) => {
                self.newest_shard_id = Some(shard_id);
                self.discard_old();
            }
            Some(newest) if self.group_distance(newest, shard_id) > MAX_SHARD_SETS => {
                return false;
            }
            Some(_) => {}
        }
        true
    }

    fn discard_old(&mut self) {
        let Some(newest) = self.newest_shard_id else {
            return;
        };
        let groups = self.group_count();
        let age = |id: u32| {
            if newest >= id {
                newest - id
            } else {
                newest + groups - id
            }
        };
        self.shard_set.retain(|&id, _| age(id) <= MAX_SHARD_SETS);
        self.completed_shards
            .retain(|&id, _| age(id) <= MAX_SHARD_SETS);
    }
}

// ─── Utilities ───────────────────────────────────────────────────────────

/// Parse FEC header from raw data.
#[cfg(test)]
#[inline]
pub(crate) fn parse_fec_header(data: &[u8]) -> Option<(u32, u16)> {
    if data.len() < FEC_HEADER_SIZE {
        return None;
    }
    let seq = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let fec_type = u16::from_le_bytes(data[4..6].try_into().ok()?);
    Some((seq, fec_type))
}

#[cfg(test)]
#[inline]
pub(crate) fn is_data_packet(data: &[u8]) -> bool {
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
            // Use direct indexing instead of try_into().unwrap() for our own well-formed buffers.
            let size = u16::from_le_bytes([data[6], data[7]]) as usize;
            assert_eq!(size, data.len() - 6);
            assert_eq!(&data[8..], &kcp[..]);
            assert_eq!(u16::from_le_bytes([data[4], data[5]]), FEC_TYPE_DATA);
            data_frames.push(data);
            if i == 2 {
                assert_eq!(parity.len(), 2);
                for p in &parity {
                    assert_eq!(u16::from_le_bytes([p[4], p[5]]), FEC_TYPE_PARITY);
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
            let seq = u32::from_le_bytes([f[0], f[1], f[2], f[3]]);
            assert_eq!(seq, i as u32);
        }
        for (i, f) in parity_frames.iter().enumerate() {
            let seq = u32::from_le_bytes([f[0], f[1], f[2], f[3]]);
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

    #[test]
    fn fec_autotune_history_is_bounded_and_wrap_safe() {
        let mut tune = AutoTune::new();
        for seq in 0..300u32 {
            tune.sample(seq % 5 < 3, seq);
        }
        assert_eq!(tune.pulses.len(), tune.max_samples);
        assert_eq!(tune.pulses.front().map(|&(seq, _)| seq), Some(42));
        assert_eq!(tune.find_period(true, u32::MAX), 3);
        assert_eq!(tune.find_period(false, u32::MAX), 2);

        let mut seam = AutoTune::new();
        let paws = 20;
        for seq in [17, 18, 19, 0, 1, 2] {
            seam.sample(seq == 18 || seq == 19 || seq == 0, seq);
        }
        assert_eq!(seam.find_period(true, paws), 3);
        assert_eq!(seam.find_period(false, paws), 0);
    }

    #[test]
    fn fec_unknown_flags_do_not_pollute_autotune() {
        let mut dec = FecDecoder::new(3, 2).unwrap();
        let mut unknown = vec![0u8; FEC_HEADER_SIZE];
        unknown[..4].copy_from_slice(&0u32.to_le_bytes());
        unknown[4..].copy_from_slice(&0x00f3u16.to_le_bytes());
        assert!(dec.decode(&unknown).is_empty());
        assert!(dec.auto_tune.pulses.is_empty());
        assert!(!dec.should_tune);

        let mut invalid_seq = unknown;
        invalid_seq[..4].copy_from_slice(&dec.paws.to_le_bytes());
        invalid_seq[4..].copy_from_slice(&FEC_TYPE_DATA.to_le_bytes());
        assert!(dec.decode(&invalid_seq).is_empty());
        assert!(dec.auto_tune.pulses.is_empty());
    }

    #[test]
    fn fec_group_age_eviction_handles_paws_wrap() {
        let mut dec = FecDecoder::new(3, 2).unwrap();
        let groups = dec.group_count();
        let last = groups - 1;
        dec.newest_shard_id = Some(last);
        dec.shard_set.insert(last, ShardHeap::new());
        dec.shard_set
            .insert(last - MAX_SHARD_SETS - 1, ShardHeap::new());
        dec.completed_shards.insert(last - 1, ());
        dec.completed_shards.insert(last - MAX_SHARD_SETS - 1, ());

        // Group 0 is the next group after the PAWS seam. Observing it moves
        // the newest marker across the seam and keeps the prior final group
        // as age one.
        assert!(dec.observe_shard(0));
        dec.shard_set.insert(0, ShardHeap::new());
        assert!(dec.shard_set.contains_key(&last));
        assert!(dec.shard_set.contains_key(&0));
        assert!(!dec.shard_set.contains_key(&(last - MAX_SHARD_SETS - 1)));
        assert!(dec.completed_shards.contains_key(&(last - 1)));
        assert!(!dec
            .completed_shards
            .contains_key(&(last - MAX_SHARD_SETS - 1)));
        assert_eq!(dec.newest_shard_id, Some(0));
    }

    #[test]
    fn fec_no_loss_group_retires_and_late_parity_is_ignored() {
        let mut enc = FecEncoder::new(3, 2, 0).unwrap();
        let mut data = Vec::new();
        let mut parity = Vec::new();
        for value in 1..=3u8 {
            let (frame, mut generated) = enc.wrap_kcp_packet(&[value; 12], 1000);
            data.push(frame);
            parity.append(&mut generated);
        }
        assert_eq!(parity.len(), 2);

        let mut dec = FecDecoder::new(3, 2).unwrap();
        for frame in &data {
            assert!(dec.decode(frame).is_empty());
        }
        assert!(dec.shard_set.is_empty());
        assert!(dec.completed_shards.contains_key(&0));
        for frame in &parity {
            assert!(dec.decode(frame).is_empty());
        }
        assert!(dec.shard_set.is_empty());
        assert_eq!(dec.completed_shards.len(), 1);
    }

    #[test]
    fn fec_interleaved_duplicate_recovery_is_exact() {
        let mut enc = FecEncoder::new(3, 2, 0).unwrap();
        let mut groups: Vec<Vec<Vec<u8>>> = Vec::new();
        for group in 0..2u8 {
            let mut frames = Vec::new();
            let mut parity = Vec::new();
            for value in 0..3u8 {
                let payload = vec![group * 10 + value; 12 + value as usize];
                let (frame, mut generated) = enc.wrap_kcp_packet(&payload, 1000);
                frames.push(frame);
                parity.append(&mut generated);
            }
            assert_eq!(parity.len(), 2);
            frames.extend(parity);
            groups.push(frames);
        }

        let mut dec = FecDecoder::new(3, 2).unwrap();
        // Interleave groups, duplicate a data shard, and recover group 0 from
        // two data shards plus one parity shard.
        assert!(dec.decode(&groups[0][1]).is_empty());
        assert!(dec.decode(&groups[1][0]).is_empty());
        assert!(dec.decode(&groups[0][1]).is_empty());
        assert!(dec.decode(&groups[0][2]).is_empty());
        let recovered = dec.decode(&groups[0][3]);
        assert_eq!(recovered.len(), 1);
        let recovered_kcp = fec_kcp_from_recovered(&recovered[0]).unwrap();
        assert_eq!(recovered_kcp, &[0u8; 12]);

        // Complete group 1 without loss, then ensure a late parity packet from
        // group 0 cannot reopen its retired heap.
        assert!(dec.decode(&groups[1][1]).is_empty());
        assert!(dec.decode(&groups[1][2]).is_empty());
        assert!(dec.decode(&groups[0][4]).is_empty());
        assert!(dec.shard_set.is_empty());
        assert!(dec.completed_shards.contains_key(&0));
        assert!(dec.completed_shards.contains_key(&1));
    }
}
