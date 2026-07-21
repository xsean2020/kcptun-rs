//! Synchronization primitives.
//!
//! `Notify` provides a lightweight wakeup mechanism for backpressure
//! signalling (replacing `tokio::sync::Notify`).
//!
//! `Mutex` is re-exported from `async_lock` — runtime-agnostic, works on both
//! tokio and smol backends.

pub use async_lock::Mutex;

// ─── Backend selection ────────────────────────────────────────────────────────
#[cfg(feature = "tokio")]
mod tokio;

#[cfg(feature = "smol")]
mod smol;

#[cfg(feature = "tokio")]
pub use self::tokio::Notify;

#[cfg(feature = "smol")]
pub use self::smol::Notify;
