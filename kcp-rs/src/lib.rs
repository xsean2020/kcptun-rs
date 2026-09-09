//! # kcp-rs
//!
//! A high-performance Rust implementation of the KCP (KCP Protocol) reliable
//! UDP transport. KCP is a fast ARQ (Automatic Repeat-reQuest) protocol that
//! provides reliable, ordered, and connection-oriented data delivery over UDP.
//!
//! ## Design
//!
//! - **Zero-copy** segment parsing via `bytes::BytesMut`
//! - **Zero-alloc** segment pooling with a `Vec`-backed free list
//! - **Atomic SNMP counters** via `std::sync::atomic` with precise `Ordering`
//! - **Reed-Solomon FEC** for forward error correction
//!
//! ## Encryption
//!
//! Block-cipher / AEAD engines and **wire packing** (`CryptoBuf`,
//! `encrypt_batch`, offload heuristics) live in [`kcrypt-rs`](../kcrypt_rs).
//! This crate does **not** depend on it — depend on `kcrypt-rs` directly for
//! anything crypto related.

// The KCP state machine is a close port of Go's kcp-go v5 and intentionally
// mirrors the upstream control flow for easy auditing. Several clippy lints
// (collapsible-if, while-let, type-complexity, etc.) would obscure that
// correspondence, so they are suppressed at the crate level here.
#![allow(
    // mirrors Go kcp-go control flow for easy auditing
    clippy::collapsible_if,
    clippy::while_let_loop,
    // KCP API surface matches Go kcp-go
    clippy::too_many_arguments,
    // index-based iteration matches Go kcp-go
    clippy::needless_range_loop,
    // Go kcp-go uses same action in multiple branches for clarity
    clippy::if_same_then_else,
)]

#[cfg(feature = "async")]
pub mod config;
pub mod fec;
pub mod kcp;
pub mod segment;
pub mod snmp;

#[cfg(feature = "async")]
pub mod conn;

#[cfg(feature = "async")]
pub mod listener;

#[cfg(feature = "async")]
pub mod sharded;

#[cfg(feature = "async")]
pub(crate) mod transport;

#[cfg(test)]
mod kcp_p999_optimizations_test;

#[cfg(feature = "async")]
pub use config::{KcpConfig, KcpMode, DEFAULT_CONV};
pub use fec::{
    fec_expand_packets, fec_kcp_from_recovered, FecDecoder, FecEncoder, FEC_HEADER_SIZE,
    FEC_TYPE_DATA, FEC_TYPE_PARITY,
};
pub use kcp::KCP;
pub use segment::SegmentPool;
pub use snmp::{add as snmp_add, enable as snmp_enable, store as snmp_store, DEFAULT_SNMP, SNMP};

#[cfg(feature = "async")]
pub use conn::{KcpStream, KcpStreamBuilder};

#[cfg(feature = "async")]
pub use listener::{KcpTcpListener, KcpTcpListenerBuilder};

#[cfg(feature = "async")]
pub use transport::PacketTransport;

#[cfg(feature = "async")]
pub use sharded::{
    bind_listener, from_socket_listener, KcpListener, KcpListenerBuilder, WorkerPoolLimits,
    WorkerPoolStats,
};
