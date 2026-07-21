//! # smux-rs
//!
//! A stream multiplexer for kcptun. SMUX allows multiple logical streams over
//! a single underlying transport (typically a KCP connection). It is a Rust
//! port of the Go `smux` library by xtaci.
//!
//! ## Protocol
//!
//! SMUX frames have a header followed by optional payload:
//!
//! ```text
//! 0      4       8       12      16      20
//! +------+-------+-------+-------+-------+
//! | ver  | cmd   | length| stream_id    |
//! +------+-------+-------+-------+-------+
//! | data...                               |
//! +---------------------------------------+
//! ```
//!
//! ## Architecture
//!
//! - `Session` — multiplexer over a single transport
//! - `Stream` — individual logical channel (implements AsyncRead + AsyncWrite)
//! - Keepalive via periodic ping frames

pub mod frame;
pub mod session;
pub mod stream;

pub use frame::{Cmd, Frame, FrameCodec, FrameError};
pub use session::{Config, Session, SessionError, UpdFrame, DEFAULT_CONFIG};
pub use stream::Stream;
