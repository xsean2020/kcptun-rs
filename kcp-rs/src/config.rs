//! KCP configuration: plain data + apply via public set_* on [`KCP`].
//!
//! Always available (not gated on async). Async [`crate::conn::KcpStream`] reuses
//! the same types.
//!
//! # Design
//!
//! - [`KcpConfig`] is a **value object** (fields + [`Default`]) — no Config builder.
//! - Tuning is done with **active** [`KCP::set_*`](crate::KCP) methods, or
//!   [`KCP::apply`](crate::KCP::apply) which only calls those setters.
//! - Mode curves (normal/fast/fast2/fast3) live **only** here.

use crate::kcp::KCP;

/// Default conversation ID (historical kcptun client/server).
pub const DEFAULT_CONV: u32 = 0xDEAD_BEEF;

/// KCP operating mode profiles (Go kcptun `--mode` curves).
///
/// Each non-[`Manual`](KcpMode::Manual) variant maps to fixed
/// `(nodelay, interval, resend, nc)` applied via [`KCP::set_nodelay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KcpMode {
    /// nodelay=0, interval=40, resend=2, nc=1
    Normal,
    /// nodelay=0, interval=30, resend=2, nc=1
    Fast,
    /// nodelay=1, interval=20, resend=2, nc=1
    Fast2,
    /// nodelay=1, interval=10, resend=2, nc=1
    #[default]
    Fast3,
    /// Use explicit `nodelay` / `interval` / `resend` / `nc` on [`KcpConfig`].
    Manual,
}

impl KcpMode {
    /// `(nodelay, interval, resend, nc)` for this profile.
    ///
    /// [`Manual`](KcpMode::Manual) returns `None` — caller uses config fields.
    pub fn nodelay_params(self) -> Option<(u32, u32, u32, u32)> {
        Some(match self {
            KcpMode::Normal => (0, 40, 2, 1),
            KcpMode::Fast => (0, 30, 2, 1),
            KcpMode::Fast2 => (1, 20, 2, 1),
            KcpMode::Fast3 => (1, 10, 2, 1),
            KcpMode::Manual => return None,
        })
    }
}

/// Snapshot of KCP (+ optional FEC shard counts) parameters.
///
/// Library default: Fast3-ish, FEC **off** (`datashard`/`parityshard` = 0).
/// kcptun product defaults (e.g. FEC 10/3) belong in `kcptun-common` CLI mapping.
#[derive(Debug, Clone)]
pub struct KcpConfig {
    pub mtu: u32,
    pub sndwnd: u32,
    pub rcvwnd: u32,
    pub mode: KcpMode,
    /// Used when [`mode`](KcpConfig::mode) is [`KcpMode::Manual`].
    pub nodelay: u32,
    pub interval: u32,
    pub resend: u32,
    pub nc: u32,
    pub stream: bool,
    /// Session/input ACK nodelay (async conn); bare [`KCP`] ignores this field in [`KCP::apply`].
    pub acknodelay: bool,
    /// Reed-Solomon data shards (0 = FEC off). Both shards must be > 0 to enable.
    pub datashard: u32,
    /// Reed-Solomon parity shards (0 = FEC off).
    pub parityshard: u32,
    pub conv: u32,
    pub token: u32,
}

impl Default for KcpConfig {
    fn default() -> Self {
        Self {
            mtu: 1350,
            sndwnd: 128,
            rcvwnd: 128,
            mode: KcpMode::Fast3,
            nodelay: 1,
            interval: 10,
            resend: 2,
            nc: 1,
            stream: true,
            acknodelay: true,
            datashard: 0,
            parityshard: 0,
            conv: DEFAULT_CONV,
            token: 0,
        }
    }
}

impl KCP {
    /// Apply a [`KcpConfig`] by calling public setters only.
    ///
    /// Does not change `conv`/`token` (fixed at [`KCP::new`]). FEC shard fields
    /// are stored on async [`crate::KcpStream`] builders, not on bare `KCP`.
    pub fn apply(&mut self, cfg: &KcpConfig) {
        self.set_mtu(cfg.mtu);
        self.set_snd_wnd(cfg.sndwnd);
        self.set_rcv_wnd(cfg.rcvwnd);
        self.set_stream_mode(cfg.stream);
        self.set_mode(cfg.mode, cfg.nodelay, cfg.interval, cfg.resend, cfg.nc);
    }

    /// Apply a mode profile, or manual nodelay knobs when `mode` is [`KcpMode::Manual`].
    pub fn set_mode(&mut self, mode: KcpMode, nodelay: u32, interval: u32, resend: u32, nc: u32) {
        let (n, i, r, c) = match mode.nodelay_params() {
            Some(p) => p,
            None => (nodelay, interval, resend, nc),
        };
        let interval = if i >= 10 { i } else { 40 };
        self.set_nodelay(n, interval, r, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_fast3_ish() {
        let c = KcpConfig::default();
        assert_eq!(c.mtu, 1350);
        assert_eq!(c.sndwnd, 128);
        assert_eq!(c.rcvwnd, 128);
        assert!(matches!(c.mode, KcpMode::Fast3));
        assert!(c.stream);
        assert!(c.acknodelay);
        assert_eq!(c.datashard, 0);
        assert_eq!(c.parityshard, 0);
    }

    #[test]
    fn apply_fast3_sets_interval_10() {
        let mut kcp = KCP::new(1, 0, |_| {});
        let cfg = KcpConfig {
            mode: KcpMode::Fast3,
            mtu: 1350,
            ..KcpConfig::default()
        };
        kcp.apply(&cfg);
        assert_eq!(kcp.mtu(), 1350);
        assert_eq!(kcp.snd_wnd(), 128);
        assert_eq!(kcp.interval(), 10);
    }

    #[test]
    fn apply_normal_sets_interval_40() {
        let mut kcp = KCP::new(1, 0, |_| {});
        kcp.apply(&KcpConfig {
            mode: KcpMode::Normal,
            ..KcpConfig::default()
        });
        assert_eq!(kcp.interval(), 40);
    }

    #[test]
    fn apply_fast_sets_interval_30() {
        let mut kcp = KCP::new(1, 0, |_| {});
        kcp.apply(&KcpConfig {
            mode: KcpMode::Fast,
            ..KcpConfig::default()
        });
        assert_eq!(kcp.interval(), 30);
    }

    #[test]
    fn apply_manual_uses_explicit_knobs() {
        let mut kcp = KCP::new(1, 0, |_| {});
        kcp.apply(&KcpConfig {
            mode: KcpMode::Manual,
            nodelay: 1,
            interval: 15,
            resend: 2,
            nc: 1,
            ..KcpConfig::default()
        });
        assert_eq!(kcp.interval(), 15);
    }

    #[test]
    fn set_mode_fast2() {
        let mut kcp = KCP::new(1, 0, |_| {});
        kcp.set_mode(KcpMode::Fast2, 0, 0, 0, 0);
        assert_eq!(kcp.interval(), 20);
    }
}
