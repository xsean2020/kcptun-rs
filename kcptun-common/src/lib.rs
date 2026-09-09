//! Shared session helpers for kcptun binaries.
//!
//! Runtime-agnostic: key derivation, KCP mode profiles, Snappy framing,
//! rate limiter.
//! Runtime-gated (`tokio`): pipe, snmp logger, encrypted KCP
//! transport assembly, KCP config, and optional QPP port.
//!
//! `KcptunSession` is the complete per-peer KCP + Snappy + SMUX abstraction.
//! Shared-UDP servers assemble `kcp_rs::KcpListener` + `CryptoTransport`
//! (see `kcptun-server/src/app.rs`); raw-TCP sockets are per-peer and connect
//! directly via `KcptunSession::serve_transport`.

mod key;
mod mode;
mod multiport;
mod ratelimit;
mod snappy_frame;

pub use key::derive_key;
pub use mode::apply_mode;
pub use multiport::{parse_multi_port, random_remote_addr};
pub use ratelimit::RateLimiter;
pub use snappy_frame::SnappyStreamDecoder;

#[cfg(feature = "tokio")]
mod kcp_config;
#[cfg(feature = "tokio")]
mod kcp_transport;
#[cfg(feature = "tokio")]
mod kcptun_session;
#[cfg(feature = "tokio")]
mod pipe;
#[cfg(feature = "tokio")]
mod snappy_pipe;
#[cfg(feature = "tokio")]
mod snmp_log;

#[cfg(feature = "tokio")]
pub use kcp_config::{
    kcp_config_from, kcp_config_from_cli, parse_kcp_mode, KcpCliParams, DEFAULT_CONV,
};
#[cfg(feature = "tokio")]
pub use kcp_transport::CryptoTransport;
#[cfg(feature = "tokio")]
pub use kcptun_session::{KcptunConfig, KcptunSession};
#[cfg(feature = "tokio")]
pub use pipe::pipe;
#[cfg(feature = "tokio")]
pub use snappy_pipe::SnappyPipe;
#[cfg(feature = "tokio")]
pub use snmp_log::{snmp_logger, snmp_signal_logger};

#[cfg(feature = "qpp")]
mod qpp_port;
#[cfg(feature = "qpp")]
mod qpp_validate;
#[cfg(feature = "qpp")]
pub use qpp_port::QPPPort;
#[cfg(feature = "qpp")]
pub use qpp_validate::validate_qpp_params;
