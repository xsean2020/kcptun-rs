//! Raw byte/packet queue types shared by the session engine and the
//! `KcpStream` facade: the pending/spare wire-packet FIFO drained by the
//! flush path, and the byte/entry-accounted read-prefetch buffer.

//! Split out of `conn.rs` (engine/facade layering); no behavior change.

use super::*;

/// Completed messages may be moved from KCP into a small bounded read queue by
/// the input task. This pipelines short bursts with application reads without
/// recreating the old unbounded side queue. Allocation is lazy and the cap is
/// per active connection; data beyond it remains governed by KCP's window.
pub(crate) const READ_PREFETCH_MAX_BYTES: usize = 32 * 1024;
/// Keep object/count overhead bounded for tiny application messages. The byte
/// cap alone would permit tens of thousands of one-byte `Bytes` entries.
pub(crate) const READ_PREFETCH_MAX_MESSAGES: usize = 32;

/// Max wire-packet slots retained in the recycled raw-output batch. Bounds the
/// memory held across drains after a pathological giant burst; typical batches
/// (tens of packets) are unaffected.
pub(crate) const MAX_RETAINED_RAW_BATCH: usize = 256;

/// Pending/spare double-buffer for produced wire packets, so draining swaps
/// buffers instead of allocating a fresh `Vec<Bytes>` under the lock. The KCP
/// output callback fills `pending`; a sender swaps `pending`↔`spare` to hand
/// out the batch, sends it, then recycles the (cleared) batch back into
/// `spare` so the next burst accumulates without re-growing from zero.
#[derive(Default)]
pub(crate) struct RawPacketQueue {
    /// Accumulating wire packets (filled by the KCP output callback).
    pub(crate) pending: Vec<Bytes>,
    /// Recycled capacity from the last drained batch.
    pub(crate) spare: Vec<Bytes>,
}

impl RawPacketQueue {
    pub(crate) fn push(&mut self, data: Bytes) {
        self.pending.push(data);
    }

    /// Prepend a batch back onto `pending` (FIFO: these must go out before
    /// anything accumulated after them). Used by the timeout path in
    /// `spawn_send_remainder` so the flush loop retries the unsent suffix
    /// instead of waiting for a full KCP RTO.
    pub(crate) fn requeue_front(&mut self, mut batch: Vec<Bytes>) {
        if batch.is_empty() {
            return;
        }
        batch.append(&mut self.pending);
        self.pending = batch;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Hand the accumulated batch to the sender: swap `pending` with `spare`
    /// (the recycled buffer becomes the next accumulation target, inheriting
    /// its capacity) and take the full batch. No allocation under the lock.
    pub(crate) fn drain(&mut self) -> Vec<Bytes> {
        std::mem::swap(&mut self.pending, &mut self.spare);
        std::mem::take(&mut self.spare)
    }

    /// Recycle the drained batch's capacity back into the spare slot, unless
    /// it is pathologically large (keeps retained memory bounded). Callers
    /// invoke this on both success and error paths.
    pub(crate) fn recycle(&mut self, mut batch: Vec<Bytes>) {
        if batch.capacity() <= MAX_RETAINED_RAW_BATCH {
            batch.clear();
            self.spare = batch;
        }
    }
}

// ─── Shared state ─────────────────────────────────────────────────────────────

/// Byte- and entry-accounted receive prefetch queue.
///
/// Keeping the accounting under the same mutex makes capacity checks O(1) and
/// ensures a short-read remainder cannot make a stale byte snapshot exceed the
/// configured prefetch bound.
#[derive(Default)]
pub(crate) struct ReadBuffer {
    queue: VecDeque<Bytes>,
    bytes: usize,
}

impl ReadBuffer {
    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn front(&self) -> Option<&Bytes> {
        self.queue.front()
    }

    pub(crate) fn pop_front(&mut self) -> Option<Bytes> {
        let data = self.queue.pop_front()?;
        self.bytes = self.bytes.saturating_sub(data.len());
        Some(data)
    }

    pub(crate) fn push_front(&mut self, data: Bytes) {
        self.bytes = self.bytes.saturating_add(data.len());
        self.queue.push_front(data);
    }

    pub(crate) fn push_back(&mut self, data: Bytes) {
        self.bytes = self.bytes.saturating_add(data.len());
        self.queue.push_back(data);
    }

    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn requeue_front_preserves_fifo_order() {
        let mut q = RawPacketQueue::default();
        q.push(Bytes::from_static(b"a"));
        q.push(Bytes::from_static(b"b"));
        let drained = q.drain();
        assert_eq!(drained.len(), 2);

        // New packets arrive while the sender is away.
        q.push(Bytes::from_static(b"c"));
        q.push(Bytes::from_static(b"d"));

        // Sender times out and requeues the unsent suffix ("b" only).
        let unsent = vec![drained[1].clone()];
        q.requeue_front(unsent);

        let next = q.drain();
        let texts: Vec<&[u8]> = next.iter().map(|b| &b[..]).collect();
        assert_eq!(texts, vec![&b"b"[..], &b"c"[..], &b"d"[..]]);
    }

    #[test]
    fn requeue_front_ignores_empty_batch() {
        let mut q = RawPacketQueue::default();
        q.push(Bytes::from_static(b"x"));
        q.requeue_front(Vec::new());
        assert_eq!(q.drain().len(), 1);
    }
}
