//! Multi-port address parser (Go `ParseMultiPort`).
//!
//! Parses "host:port" and "host:minport-maxport" strings into a list of
//! [`SocketAddr`]. The host may be an IP literal **or a DNS hostname** — the
//! latter is resolved (Go's `net.ResolveUDPAddr` behavior), so
//! `-r example.com:29900` / `-l myhost:29900` work. Shared by client dial and
//! server listen paths.

use std::net::{SocketAddr, ToSocketAddrs};

use anyhow::{Context, Result};

/// Resolve one `(host, port)` to a concrete [`SocketAddr`] (first A/AAAA
/// record), handling both IP literals and DNS hostnames. IPv6 literals are
/// stripped of their brackets (`[::1]` → `::1`) before lookup.
fn resolve_one(host: &str, port: u16) -> Result<SocketAddr> {
    let h = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    (h, port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve address {host}:{port}"))?
        .next()
        .with_context(|| format!("no address for {host}:{port}"))
}

/// Parse a "host:minport-maxport" or "host:port" string into a list of SocketAddr.
/// Supports ":port" shorthand where host defaults to "0.0.0.0".
pub fn parse_multi_port(addr: &str) -> Result<Vec<SocketAddr>> {
    let colon = addr.rfind(':').context("address must include host:port")?;
    let host = if colon == 0 {
        "0.0.0.0"
    } else {
        &addr[..colon]
    };
    let port_spec = &addr[colon + 1..];

    if let Some(dash) = port_spec.find('-') {
        let min_port: u16 = port_spec[..dash].parse()?;
        let max_port: u16 = port_spec[dash + 1..].parse()?;
        if min_port > max_port || min_port == 0 || max_port == 0 {
            anyhow::bail!(
                "invalid port range: minport={} -> maxport={}",
                min_port,
                max_port
            );
        }
        let mut addrs = Vec::with_capacity((max_port - min_port + 1) as usize);
        for port in min_port..=max_port {
            addrs.push(resolve_one(host, port)?);
        }
        Ok(addrs)
    } else {
        let port: u16 = port_spec.parse()?;
        if port == 0 {
            anyhow::bail!("invalid port: 0");
        }
        Ok(vec![resolve_one(host, port)?])
    }
}

/// Resolve a client destination and choose a cryptographically random port
/// from its configured range, matching Go's per-dial selection.
pub fn random_remote_addr(addr: &str) -> Result<SocketAddr> {
    let addrs = parse_multi_port(addr)?;
    let mut random = [0u8; 8];
    getrandom::getrandom(&mut random)
        .map_err(|error| anyhow::anyhow!("cannot obtain random remote port: {error}"))?;
    let index = (u64::from_le_bytes(random) % addrs.len() as u64) as usize;
    Ok(addrs[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_port() {
        let addrs = parse_multi_port("127.0.0.1:1234").unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 1234);
    }

    #[test]
    fn test_port_range() {
        let addrs = parse_multi_port("127.0.0.1:1000-1003").unwrap();
        assert_eq!(addrs.len(), 4);
        assert_eq!(addrs[0].port(), 1000);
        assert_eq!(addrs[3].port(), 1003);
    }

    #[test]
    fn test_shorthand_host() {
        let addrs = parse_multi_port(":29900-29901").unwrap();
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].port(), 29900);
        assert_eq!(addrs[1].port(), 29901);
    }

    #[test]
    fn test_ipv6() {
        let addrs = parse_multi_port("[::1]:1234").unwrap();
        assert_eq!(addrs.len(), 1);
    }

    #[test]
    fn test_invalid_range() {
        assert!(parse_multi_port("127.0.0.1:10-5").is_err());
        assert!(parse_multi_port("127.0.0.1:0-10").is_err());
        assert!(parse_multi_port("127.0.0.1:99999-99999").is_err());
        assert!(parse_multi_port("127.0.0.1:0").is_err());
    }

    #[test]
    fn test_random_remote_addr_stays_in_range() {
        for _ in 0..32 {
            let addr = random_remote_addr("127.0.0.1:1000-1003").unwrap();
            assert!((1000..=1003).contains(&addr.port()));
        }
    }

    #[test]
    fn test_no_port() {
        assert!(parse_multi_port("127.0.0.1").is_err());
    }

    #[test]
    fn test_hostname_resolution() {
        // "localhost" resolves to a loopback address (127.0.0.1 or ::1).
        let addrs = parse_multi_port("localhost:1234").unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 1234);
        assert!(addrs[0].ip().is_loopback());

        // Hostname + port range.
        let addrs = parse_multi_port("localhost:1000-1001").unwrap();
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].port(), 1000);
        assert_eq!(addrs[1].port(), 1001);

        // Unresolvable hostname must error, not panic.
        assert!(parse_multi_port("no-such-host.invalid:29900").is_err());
    }
}
