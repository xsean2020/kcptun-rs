//! # kcp-rs
//!
//! A high-performance Rust implementation of the KCP (KCP Protocol) reliable
//! UDP transport. KCP is a fast ARQ (Automatic Repeat-reQuest) protocol that
//! provides reliable, ordered, and connection-oriented data delivery over UDP.
//!
//! ## Design
//!
//! - **Zero-copy** segment parsing via `bytes::BytesMut`
//! - **Lock-free** segment pooling with `crossbeam::queue::SegQueue`
//! - **Atomic SNMP counters** via `std::sync::atomic` with precise `Ordering`
//! - **Cache-friendly** `#[repr(C)]` struct layouts aligned to 64-byte cache lines
//! - **Pluggable** `BlockCrypt` trait for encryption at the segment level
//! - **Reed-Solomon FEC** for forward error correction
//!
//! ## Encryption
//!
//! The block-cipher and AEAD implementations live in the dedicated
//! [`kcrypt-rs`](../kcrypt_rs) crate and are re-exported here for backward
//! compatibility. New code should depend on `kcrypt-rs` directly.

// The KCP state machine is a close port of Go's kcp-go v5 and intentionally
// mirrors the upstream control flow for easy auditing. Several clippy lints
// (collapsible-if, while-let, type-complexity, etc.) would obscure that
// correspondence, so they are suppressed at the crate level here.
#![allow(
    clippy::type_complexity,
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::while_let_loop,
    clippy::manual_div_ceil,
    clippy::unnecessary_cast,
    clippy::needless_range_loop,
    clippy::redundant_pattern_matching,
    clippy::uninlined_format_args,
    clippy::too_many_arguments,
    clippy::new_without_default,
    clippy::len_without_is_empty,
    clippy::empty_line_after_doc_comments,
    clippy::absurd_extreme_comparisons,
    clippy::if_same_then_else,
    clippy::manual_range_contains,
    clippy::useless_conversion,
    clippy::arc_with_non_send_sync,
    clippy::needless_late_init,
    clippy::manual_checked_ops
)]

pub mod crypto_buf;
pub mod fec;
pub mod kcp;
pub mod segment;
pub mod session;
pub mod snmp;

// Re-export the crypto modules from the shared `kcrypt-rs` crate.
pub use kcrypt_rs::cast5;
pub use kcrypt_rs::crypt;

// Re-export the primary public API.
pub use crypt::{select_aead_crypt, select_block_crypt, AeadCrypt, BlockCrypt, SelectBlockCrypt};
pub use crypto_buf::{encrypt_batch, should_cpu_block_encrypt, CryptoBuf};
pub use fec::FecDecoder;
pub use kcp::KCP;
pub use segment::SegmentPool;
pub use session::UDPSession;
pub use snmp::{DEFAULT_SNMP, SNMP};
