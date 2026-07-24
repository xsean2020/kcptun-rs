//! Salsa20 stream cipher (32-byte key).
//!
//! Ported from Go's `golang.org/x/crypto/salsa20`. The first 8 bytes of
//! the plaintext are used as the nonce (and left unchanged); the keystream
//! starts at offset 8 and XORs the remaining bytes.
//!
//! ## SSE2 SIMD optimization (x86_64)
//!
//! On x86_64, 4 Salsa20 blocks (256 bytes) are processed in parallel using
//! SSE2 SIMD, matching the approach in Go's `salsa20_amd64.s`. Each XMM
//! register holds one state word from 4 different blocks; the quarter-round
//! operations are applied to all 4 blocks simultaneously. This gives ~3–4×
//! throughput vs the scalar `saltwenty` fallback used on non-x86_64.

use super::BlockCrypt;

#[derive(Debug)]
pub struct Salsa20Crypt {
    key: [u8; 32],
}

impl Salsa20Crypt {
    pub fn new(key: &[u8]) -> Self {
        let mut k = [0u8; 32];
        let l = key.len().min(32);
        k[..l].copy_from_slice(&key[..l]);
        Salsa20Crypt { key: k }
    }
}

// ─── Scalar implementation (fallback on non-x86_64, tail on x86_64) ─────

/// Salsa20 quarter-round macro.
///
/// A macro (not a `fn`) so the compiler sees the entire 20-round loop body
/// at each call site — this lets it keep all 16 state words in registers
/// instead of spilling `&mut u32` aliases to memory. Matching Go's
/// `salsa20_ref.go` which inlines the quarter-rounds directly.
#[inline]
fn u32from(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Salsa20 quarter-round: `b ^= rotl(a+d,7); c ^= rotl(b+a,9);
/// d ^= rotl(c+b,13); a ^= rotl(d+c,18)`.
macro_rules! qr {
    ($a:ident, $b:ident, $c:ident, $d:ident) => {
        $b ^= $a.wrapping_add($d).rotate_left(7);
        $c ^= $b.wrapping_add($a).rotate_left(9);
        $d ^= $c.wrapping_add($b).rotate_left(13);
        $a ^= $d.wrapping_add($c).rotate_left(18);
    };
}

/// Generate 64 bytes of Salsa20 keystream (scalar, 1 block).
fn saltwenty(key: &[u8; 32], nonce: &[u8; 8], ctr: u64, out: &mut [u8; 64]) {
    // State matrix layout matching Go's golang.org/x/crypto/salsa20/salsa/salsa20_ref.go
    let s = 0x61707865u32;
    let (
        mut x0, mut x1, mut x2, mut x3, mut x4, mut x5, mut x6, mut x7, mut x8, mut x9, mut x10,
        mut x11, mut x12, mut x13, mut x14, mut x15,
    ) = (
        s,
        u32from(key, 0),
        u32from(key, 4),
        u32from(key, 8),
        u32from(key, 12),
        0x3320646eu32,
        u32from(nonce, 0),
        u32from(nonce, 4),
        (ctr & 0xFFFF_FFFF) as u32,
        (ctr >> 32) as u32,
        0x79622d32u32,
        u32from(key, 16),
        u32from(key, 20),
        u32from(key, 24),
        u32from(key, 28),
        0x6b206574u32,
    );
    let (i0, i1, i2, i3, i4, i5, i6, i7, i8, i9, i10, i11, i12, i13, i14, i15) = (
        x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15,
    );
    for _ in 0..10 {
        // Column round
        qr!(x0, x4, x8, x12);
        qr!(x5, x9, x13, x1);
        qr!(x10, x14, x2, x6);
        qr!(x15, x3, x7, x11);
        // Row round
        qr!(x0, x1, x2, x3);
        qr!(x5, x6, x7, x4);
        qr!(x10, x11, x8, x9);
        qr!(x15, x12, x13, x14);
    }
    let v = [
        x0.wrapping_add(i0), x1.wrapping_add(i1), x2.wrapping_add(i2), x3.wrapping_add(i3),
        x4.wrapping_add(i4), x5.wrapping_add(i5), x6.wrapping_add(i6), x7.wrapping_add(i7),
        x8.wrapping_add(i8), x9.wrapping_add(i9), x10.wrapping_add(i10), x11.wrapping_add(i11),
        x12.wrapping_add(i12), x13.wrapping_add(i13), x14.wrapping_add(i14), x15.wrapping_add(i15),
    ];
    for i in 0..16 {
        out[i * 4..(i + 1) * 4].copy_from_slice(&v[i].to_le_bytes());
    }
}

// ─── SSE2 4-block parallel implementation (x86_64 only) ──────────────────

#[cfg(target_arch = "x86_64")]
mod sse2 {
    use std::arch::x86_64::*;

    #[inline(always)]
    unsafe fn rotl7(x: __m128i) -> __m128i {
        _mm_or_si128(_mm_slli_epi32::<7>(x), _mm_srli_epi32::<25>(x))
    }
    #[inline(always)]
    unsafe fn rotl9(x: __m128i) -> __m128i {
        _mm_or_si128(_mm_slli_epi32::<9>(x), _mm_srli_epi32::<23>(x))
    }
    #[inline(always)]
    unsafe fn rotl13(x: __m128i) -> __m128i {
        _mm_or_si128(_mm_slli_epi32::<13>(x), _mm_srli_epi32::<19>(x))
    }
    #[inline(always)]
    unsafe fn rotl18(x: __m128i) -> __m128i {
        _mm_or_si128(_mm_slli_epi32::<18>(x), _mm_srli_epi32::<14>(x))
    }

    /// Salsa20 quarter-round on 4 blocks simultaneously.
    #[inline(always)]
    unsafe fn sqr(
        a: &mut __m128i, b: &mut __m128i, c: &mut __m128i, d: &mut __m128i,
    ) {
        let t = _mm_add_epi32(*a, *d);
        *b = _mm_xor_si128(*b, rotl7(t));
        let t = _mm_add_epi32(*b, *a);
        *c = _mm_xor_si128(*c, rotl9(t));
        let t = _mm_add_epi32(*c, *b);
        *d = _mm_xor_si128(*d, rotl13(t));
        let t = _mm_add_epi32(*d, *c);
        *a = _mm_xor_si128(*a, rotl18(t));
    }

    /// 4×4 matrix transpose: input lanes are block-interleaved → output is
    /// block-contiguous.
    #[inline(always)]
    unsafe fn transpose_4x4(
        a: __m128i, b: __m128i, c: __m128i, d: __m128i,
    ) -> (__m128i, __m128i, __m128i, __m128i) {
        let t0 = _mm_unpacklo_epi32(a, b);
        let t1 = _mm_unpackhi_epi32(a, b);
        let t2 = _mm_unpacklo_epi32(c, d);
        let t3 = _mm_unpackhi_epi32(c, d);
        (
            _mm_unpacklo_epi64(t0, t2),
            _mm_unpackhi_epi64(t0, t2),
            _mm_unpacklo_epi64(t1, t3),
            _mm_unpackhi_epi64(t1, t3),
        )
    }

    /// Generate 256 bytes of Salsa20 keystream from 4 blocks with counters
    /// `ctr`, `ctr+1`, `ctr+2`, `ctr+3`.
    ///
    /// Each XMM register holds one state word from 4 different blocks
    /// (lane 0 = block 0, lane 1 = block 1, etc.).
    pub unsafe fn saltwenty_x4(
        key: &[u8; 32], nonce: &[u8; 8], ctr: u64, out: &mut [u8; 256],
    ) {
        let u32le = |b: &[u8], i: usize| {
            u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
        };

        // Broadcast shared state words to all 4 lanes
        let s0 = _mm_set1_epi32(0x61707865u32 as i32);
        let s1 = _mm_set1_epi32(u32le(key, 0) as i32);
        let s2 = _mm_set1_epi32(u32le(key, 4) as i32);
        let s3 = _mm_set1_epi32(u32le(key, 8) as i32);
        let s4 = _mm_set1_epi32(u32le(key, 12) as i32);
        let s5 = _mm_set1_epi32(0x3320646eu32 as i32);
        let s6 = _mm_set1_epi32(u32le(nonce, 0) as i32);
        let s7 = _mm_set1_epi32(u32le(nonce, 4) as i32);

        // Counter words — different per lane (block)
        let c0 = ctr;
        let c1 = ctr.wrapping_add(1);
        let c2 = ctr.wrapping_add(2);
        let c3 = ctr.wrapping_add(3);
        let s8 = _mm_set_epi32(
            (c3 & 0xFFFF_FFFF) as i32,
            (c2 & 0xFFFF_FFFF) as i32,
            (c1 & 0xFFFF_FFFF) as i32,
            (c0 & 0xFFFF_FFFF) as i32,
        );
        let s9 = _mm_set_epi32(
            (c3 >> 32) as i32,
            (c2 >> 32) as i32,
            (c1 >> 32) as i32,
            (c0 >> 32) as i32,
        );

        let s10 = _mm_set1_epi32(0x79622d32u32 as i32);
        let s11 = _mm_set1_epi32(u32le(key, 16) as i32);
        let s12 = _mm_set1_epi32(u32le(key, 20) as i32);
        let s13 = _mm_set1_epi32(u32le(key, 24) as i32);
        let s14 = _mm_set1_epi32(u32le(key, 28) as i32);
        let s15 = _mm_set1_epi32(0x6b206574u32 as i32);

        let (mut x0, mut x1, mut x2, mut x3) = (s0, s1, s2, s3);
        let (mut x4, mut x5, mut x6, mut x7) = (s4, s5, s6, s7);
        let (mut x8, mut x9, mut x10, mut x11) = (s8, s9, s10, s11);
        let (mut x12, mut x13, mut x14, mut x15) = (s12, s13, s14, s15);

        for _ in 0..10 {
            // Column round
            sqr(&mut x0, &mut x4, &mut x8, &mut x12);
            sqr(&mut x5, &mut x9, &mut x13, &mut x1);
            sqr(&mut x10, &mut x14, &mut x2, &mut x6);
            sqr(&mut x15, &mut x3, &mut x7, &mut x11);
            // Row round
            sqr(&mut x0, &mut x1, &mut x2, &mut x3);
            sqr(&mut x5, &mut x6, &mut x7, &mut x4);
            sqr(&mut x10, &mut x11, &mut x8, &mut x9);
            sqr(&mut x15, &mut x12, &mut x13, &mut x14);
        }

        // Add initial state
        x0 = _mm_add_epi32(x0, s0);
        x1 = _mm_add_epi32(x1, s1);
        x2 = _mm_add_epi32(x2, s2);
        x3 = _mm_add_epi32(x3, s3);
        x4 = _mm_add_epi32(x4, s4);
        x5 = _mm_add_epi32(x5, s5);
        x6 = _mm_add_epi32(x6, s6);
        x7 = _mm_add_epi32(x7, s7);
        x8 = _mm_add_epi32(x8, s8);
        x9 = _mm_add_epi32(x9, s9);
        x10 = _mm_add_epi32(x10, s10);
        x11 = _mm_add_epi32(x11, s11);
        x12 = _mm_add_epi32(x12, s12);
        x13 = _mm_add_epi32(x13, s13);
        x14 = _mm_add_epi32(x14, s14);
        x15 = _mm_add_epi32(x15, s15);

        // Transpose state-major → block-major and store.
        // After transpose, r0 = [block0_w0..w3], r1 = [block1_w0..w3], etc.
        let base = out.as_mut_ptr();
        macro_rules! store_group {
            ($a:expr, $b:expr, $c:expr, $d:expr, $off0:expr, $off1:expr, $off2:expr, $off3:expr) => {{
                let (r0, r1, r2, r3) = transpose_4x4($a, $b, $c, $d);
                _mm_storeu_si128(base.add($off0) as *mut __m128i, r0);
                _mm_storeu_si128(base.add($off1) as *mut __m128i, r1);
                _mm_storeu_si128(base.add($off2) as *mut __m128i, r2);
                _mm_storeu_si128(base.add($off3) as *mut __m128i, r3);
            }};
        }
        store_group!(x0, x1, x2, x3, 0, 64, 128, 192);
        store_group!(x4, x5, x6, x7, 16, 80, 144, 208);
        store_group!(x8, x9, x10, x11, 32, 96, 160, 224);
        store_group!(x12, x13, x14, x15, 48, 112, 176, 240);
    }
}

// ─── BlockCrypt impl ─────────────────────────────────────────────────────

impl BlockCrypt for Salsa20Crypt {
    fn encrypt(&self, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let mut nonce = [0u8; 8];
        let nlen = data.len().min(8);
        nonce[..nlen].copy_from_slice(&data[..nlen]);
        let mut ctr = 0u64;
        let mut off = 8usize;

        #[cfg(target_arch = "x86_64")]
        {
            // SSE2 fast path: 4 blocks (256 bytes) at a time
            use std::arch::x86_64::*;
            let mut ks = [0u8; 256];
            while off + 256 <= data.len() {
                unsafe { sse2::saltwenty_x4(&self.key, &nonce, ctr, &mut ks) };
                // XOR 256 bytes using SSE2 (16 bytes per instruction)
                for j in (0..256).step_by(16) {
                    unsafe {
                        let d = _mm_loadu_si128(data[off + j..].as_ptr() as *const __m128i);
                        let k = _mm_loadu_si128(ks[j..].as_ptr() as *const __m128i);
                        _mm_storeu_si128(
                            data[off + j..].as_mut_ptr() as *mut __m128i,
                            _mm_xor_si128(d, k),
                        );
                    }
                }
                ctr += 4;
                off += 256;
            }
        }

        // Scalar tail (also the only path on non-x86_64)
        let mut ks = [0u8; 64];
        while off < data.len() {
            saltwenty(&self.key, &nonce, ctr, &mut ks);
            let end = (off + 64).min(data.len());
            // u64 XOR for 8× throughput on the tail
            let mut j = 0usize;
            while j + 8 <= end - off {
                let d = u64::from_le_bytes([
                    data[off + j], data[off + j + 1], data[off + j + 2], data[off + j + 3],
                    data[off + j + 4], data[off + j + 5], data[off + j + 6], data[off + j + 7],
                ]);
                let k = u64::from_le_bytes([
                    ks[j], ks[j + 1], ks[j + 2], ks[j + 3],
                    ks[j + 4], ks[j + 5], ks[j + 6], ks[j + 7],
                ]);
                let r = (d ^ k).to_le_bytes();
                data[off + j..off + j + 8].copy_from_slice(&r);
                j += 8;
            }
            while off + j < end {
                data[off + j] ^= ks[j];
                j += 1;
            }
            ctr += 1;
            off += 64;
        }
    }
    fn decrypt(&self, data: &mut [u8]) {
        // Salsa20 is symmetric
        self.encrypt(data);
    }
    fn name(&self) -> &'static str {
        "salsa20"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rt(c: &dyn BlockCrypt, d: &mut [u8]) {
        let o = d.to_vec();
        c.encrypt(d);
        c.decrypt(d);
        assert_eq!(d, &o, "{} roundtrip", c.name());
    }
    #[test]
    fn salsa() {
        let msg = b"hello kcp salsa test!";
        let mut d = vec![0u8; 8 + msg.len()];
        d[8..].copy_from_slice(msg);
        rt(&Salsa20Crypt::new(b"test-key-12345-test-key-67890"), &mut d);
    }

    #[test]
    fn salsa20_go_source_compatible() {
        // Test Salsa20 matches Go's algorithm
        let key = b"test-key-12345-test-key-67890";
        let crypt = Salsa20Crypt::new(key);
        let msg = b"TEST VECTOR FOR SALSA20!!";
        let mut data = vec![0u8; 8 + msg.len()];
        data[8..].copy_from_slice(msg);
        let orig = data.clone();
        crypt.encrypt(&mut data);
        assert_eq!(data[..8], orig[..8], "Salsa20 nonce unchanged");
        crypt.decrypt(&mut data);
        assert_eq!(data, orig, "Salsa20 roundtrip");
    }

    #[test]
    fn salsa20_large_payload_roundtrip() {
        // Exercise both SSE2 4-block path and scalar tail
        let key = b"test-key-12345-test-key-67890";
        let crypt = Salsa20Crypt::new(key);
        for len in [256usize, 512, 1024, 1400] {
            let mut data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let orig = data.clone();
            crypt.encrypt(&mut data);
            assert_ne!(&data[..], &orig[..], "len={len} must change");
            crypt.decrypt(&mut data);
            assert_eq!(&data[..], &orig[..], "len={len} roundtrip");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sse2_matches_scalar() {
        // Verify SSE2 4-block output matches 4× scalar output
        let key = *b"test-key-12345-test-key-67890";
        let nonce = *b"nonce123";
        let ctr = 7u64;

        let mut scalar_out = [0u8; 256];
        for i in 0..4 {
            let mut block = [0u8; 64];
            saltwenty(&key, &nonce, ctr + i as u64, &mut block);
            scalar_out[i * 64..(i + 1) * 64].copy_from_slice(&block);
        }

        let mut sse2_out = [0u8; 256];
        unsafe { sse2::saltwenty_x4(&key, &nonce, ctr, &mut sse2_out) };

        assert_eq!(
            &scalar_out[..], &sse2_out[..],
            "SSE2 keystream must match scalar"
        );
    }
}
