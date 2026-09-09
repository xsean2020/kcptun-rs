//! Build [`kcp_rs::KcpConfig`] from CLI-like fields shared by client/server.
//!
//! [`KcpConfig`] / [`KcpMode`] live in **kcp-rs** (always available). This module
//! only maps clap-shaped strings and **kcptun product defaults**.

use kcp_rs::{KcpConfig, KcpMode};

/// Default conversation ID (matches historical kcptun client/server).
pub const DEFAULT_CONV: u32 = 0xDEAD_BEEF;

/// CLI-shaped KCP parameters used by both binaries.
///
/// This builds the KCP/FEC portion of [`crate::KcptunConfig`]; SMUX, Snappy,
/// crypto, and rate limiting remain at their owning layers.
#[derive(Debug, Clone)]
pub struct KcpCliParams {
    /// Mode profile: `normal` / `fast` / `fast2` / `fast3`, or anything else
    /// for manual nodelay/interval/resend/nc (Go semantics).
    pub mode: String,
    pub mtu: u32,
    pub sndwnd: u32,
    pub rcvwnd: u32,
    pub datashard: u32,
    pub parityshard: u32,
    pub acknodelay: bool,
    /// Used only when `mode` is not a known profile (`manual` / unknown).
    pub nodelay: u32,
    pub interval: u32,
    pub resend: u32,
    pub nc: u32,
    /// Conversation ID; default [`DEFAULT_CONV`].
    pub conv: u32,
    /// Auth token (Go kcp-go); usually 0.
    pub token: u32,
}

impl Default for KcpCliParams {
    fn default() -> Self {
        Self {
            // Match KcpConfig / Fast3 defaults used by client CLI default.
            mode: "fast3".to_string(),
            mtu: 1350,
            sndwnd: 128,
            rcvwnd: 128,
            // FEC defaults match Go kcptun (10/3); set both 0 to disable.
            datashard: 10,
            parityshard: 3,
            acknodelay: false,
            nodelay: 0,
            interval: 50,
            resend: 0,
            nc: 0,
            conv: DEFAULT_CONV,
            token: 0,
        }
    }
}

impl KcpCliParams {
    /// Convert to [`KcpConfig`] (stream mode always on for kcptun).
    pub fn to_kcp_config(&self) -> KcpConfig {
        kcp_config_from(
            &self.mode,
            self.mtu,
            self.sndwnd,
            self.rcvwnd,
            self.nodelay,
            self.interval,
            self.resend,
            self.nc,
            self.acknodelay,
            self.datashard,
            self.parityshard,
            self.conv,
            self.token,
        )
    }
}

/// Parse a mode string into [`KcpMode`].
///
/// Known profiles (`normal`/`fast`/`fast2`/`fast3`) map 1:1. Anything else
/// (including `"manual"` and typos) becomes [`KcpMode::Manual`] so the
/// explicit nodelay/interval/resend/nc fields apply — matching binary
/// `apply_mode` / set_nodelay branching.
pub fn parse_kcp_mode(mode: &str) -> KcpMode {
    match mode {
        "normal" => KcpMode::Normal,
        "fast" => KcpMode::Fast,
        "fast2" => KcpMode::Fast2,
        "fast3" => KcpMode::Fast3,
        _ => KcpMode::Manual,
    }
}

/// Build [`KcpConfig`] from CLI-like fields.
///
/// - Known modes override the four nodelay knobs (Go kcptun behavior).
/// - Unknown / `"manual"` uses the explicit `nodelay`/`interval`/`resend`/`nc`.
/// - `stream` is always `true` (kcptun SMUX stack).
/// - FEC is enabled only when both `datashard` and `parityshard` are > 0
///   (enforced later by `kcp_stream_*` / KcpStream builder).
///
/// `conv` defaults to [`DEFAULT_CONV`] when callers pass that constant;
/// use a non-default value for multi-peer servers that allocate per-peer conv.
#[allow(clippy::too_many_arguments)]
pub fn kcp_config_from(
    mode: &str,
    mtu: u32,
    sndwnd: u32,
    rcvwnd: u32,
    nodelay: u32,
    interval: u32,
    resend: u32,
    nc: u32,
    acknodelay: bool,
    datashard: u32,
    parityshard: u32,
    conv: u32,
    token: u32,
) -> KcpConfig {
    let kcp_mode = parse_kcp_mode(mode);
    KcpConfig {
        mtu,
        sndwnd,
        rcvwnd,
        mode: kcp_mode,
        nodelay,
        interval,
        resend,
        nc,
        stream: true,
        acknodelay,
        datashard,
        parityshard,
        conv,
        token,
    }
}

/// Convenience: same as [`kcp_config_from`] with default conv/token.
#[allow(clippy::too_many_arguments)]
pub fn kcp_config_from_cli(
    mode: &str,
    mtu: u32,
    sndwnd: u32,
    rcvwnd: u32,
    nodelay: u32,
    interval: u32,
    resend: u32,
    nc: u32,
    acknodelay: bool,
    datashard: u32,
    parityshard: u32,
) -> KcpConfig {
    kcp_config_from(
        mode,
        mtu,
        sndwnd,
        rcvwnd,
        nodelay,
        interval,
        resend,
        nc,
        acknodelay,
        datashard,
        parityshard,
        DEFAULT_CONV,
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_modes_map() {
        assert_eq!(parse_kcp_mode("normal"), KcpMode::Normal);
        assert_eq!(parse_kcp_mode("fast"), KcpMode::Fast);
        assert_eq!(parse_kcp_mode("fast2"), KcpMode::Fast2);
        assert_eq!(parse_kcp_mode("fast3"), KcpMode::Fast3);
        assert_eq!(parse_kcp_mode("manual"), KcpMode::Manual);
        assert_eq!(parse_kcp_mode("wat"), KcpMode::Manual);
    }

    #[test]
    fn kcp_config_from_fast3_profile() {
        let cfg = kcp_config_from_cli(
            "fast3", 1350, 128, 512, /* ignored for known mode */ 0, 50, 0, 0, true, 10, 3,
        );
        assert_eq!(cfg.mode, KcpMode::Fast3);
        assert_eq!(cfg.mtu, 1350);
        assert_eq!(cfg.sndwnd, 128);
        assert_eq!(cfg.rcvwnd, 512);
        assert!(cfg.stream);
        assert!(cfg.acknodelay);
        assert_eq!(cfg.datashard, 10);
        assert_eq!(cfg.parityshard, 3);
        assert_eq!(cfg.conv, DEFAULT_CONV);
    }

    #[test]
    fn kcp_config_from_manual_keeps_knobs() {
        let cfg = kcp_config_from(
            "manual", 1400, 256, 256, 1, 15, 2, 1, false, 0, 0, 0xC0FFEE, 7,
        );
        assert_eq!(cfg.mode, KcpMode::Manual);
        assert_eq!(cfg.nodelay, 1);
        assert_eq!(cfg.interval, 15);
        assert_eq!(cfg.resend, 2);
        assert_eq!(cfg.nc, 1);
        assert!(!cfg.acknodelay);
        assert_eq!(cfg.datashard, 0);
        assert_eq!(cfg.parityshard, 0);
        assert_eq!(cfg.conv, 0xC0FFEE);
        assert_eq!(cfg.token, 7);
        assert!(cfg.stream);
    }

    #[test]
    fn cli_params_to_config() {
        let p = KcpCliParams {
            mode: "fast2".into(),
            mtu: 1200,
            sndwnd: 64,
            rcvwnd: 64,
            datashard: 0,
            parityshard: 0,
            acknodelay: true,
            nodelay: 9,
            interval: 99,
            resend: 9,
            nc: 9,
            conv: 1,
            token: 2,
        };
        let cfg = p.to_kcp_config();
        assert_eq!(cfg.mode, KcpMode::Fast2);
        // knobs ignored for known profile but still stored on the struct
        assert_eq!(cfg.nodelay, 9);
        assert_eq!(cfg.mtu, 1200);
        assert_eq!(cfg.conv, 1);
        assert_eq!(cfg.token, 2);
    }

    // ─── Golden test: legacy `apply_mode` / binary branch ≡ library path ───────

    /// Every known mode curve, locked as the single source of truth. Must match
    /// BOTH `crate::apply_mode` (mode.rs) and `KcpMode::nodelay_params` (kcp-rs).
    type ModeCurve = (&'static str, KcpMode, (u32, u32, u32, u32));
    const CURVES: [ModeCurve; 4] = [
        ("normal", KcpMode::Normal, (0, 40, 2, 1)),
        ("fast", KcpMode::Fast, (0, 30, 2, 1)),
        ("fast2", KcpMode::Fast2, (1, 20, 2, 1)),
        ("fast3", KcpMode::Fast3, (1, 10, 2, 1)),
    ];

    #[test]
    fn golden_mode_curves_legacy_vs_config() {
        for (name, mode_enum, curve) in CURVES {
            // Library side: the curve lives in KcpMode::nodelay_params.
            assert_eq!(mode_enum.nodelay_params(), Some(curve), "KcpMode {name}");

            // Legacy client: `set_snd_wnd`/`set_rcv_wnd` (client main.rs) then
            // `apply_mode` for known modes (KCP set_mode).
            let mut legacy = kcp_rs::KCP::new(1, 0, |_| {});
            legacy.set_snd_wnd(128);
            legacy.set_rcv_wnd(512);
            crate::apply_mode(&mut legacy, name);

            // Library client path: kcp_config_from → KcpConfig.apply.
            let mut lib = kcp_rs::KCP::new(1, 0, |_| {});
            let cfg = kcp_config_from_cli(name, 1350, 128, 512, 0, 50, 0, 0, false, 0, 0);
            lib.apply(&cfg);

            assert_eq!(legacy.interval(), lib.interval(), "interval {name}");
            assert_eq!(legacy.snd_wnd(), lib.snd_wnd(), "snd_wnd {name}");
            assert_eq!(legacy.rcv_wnd(), lib.rcv_wnd(), "rcv_wnd {name}");
        }
    }

    #[test]
    fn golden_manual_mode_interval_clamp_matches_go() {
        // Go's `kcp.NoDelay` clamps the interval to [10, 5000]. A sub-10 ms
        // request therefore becomes 10 ms; it used to become 40 ms here
        // (`KCP::set_mode` substituted its own floor), so `--mode manual
        // --interval 5` ran 8× slower than the same flags under Go.
        for (interval, want) in [
            (0u32, 10u32),
            (5, 10),
            (10, 10),
            (15, 15),
            (50, 50),
            (6000, 5000),
        ] {
            let mut lib = kcp_rs::KCP::new(1, 0, |_| {});
            lib.apply(&kcp_config_from(
                "manual",
                1350,
                128,
                512,
                1,
                interval,
                2,
                1,
                false,
                0,
                0,
                DEFAULT_CONV,
                0,
            ));

            assert_eq!(
                lib.interval(),
                want,
                "manual interval clamp for interval={interval}"
            );
        }
    }

    #[test]
    fn golden_unknown_mode_uses_manual_knobs() {
        // Legacy client `_` branch treats unknown modes as manual (explicit
        // nodelay/interval/resend/nc). Library parse_kcp_mode → Manual.
        let cfg = kcp_config_from(
            "weird-mode",
            1350,
            128,
            512,
            2,
            25,
            3,
            0,
            false,
            10,
            3,
            DEFAULT_CONV,
            0,
        );
        assert_eq!(cfg.mode, KcpMode::Manual);
        assert_eq!(cfg.nodelay, 2);
        assert_eq!(cfg.interval, 25);
        assert_eq!(cfg.resend, 3);
        assert_eq!(cfg.nc, 0);
        assert_eq!(cfg.datashard, 10);
        assert_eq!(cfg.parityshard, 3);

        let mut kcp = kcp_rs::KCP::new(1, 0, |_| {});
        kcp.apply(&cfg);
        // Explicit interval 25 (≥10) is used verbatim — not a mode curve.
        assert_eq!(kcp.interval(), 25);
    }
}
