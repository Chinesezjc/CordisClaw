//! P2-2: Shared SSRF guard for plugins that make outbound HTTP requests.
//!
//! `ip_is_forbidden` decides whether a resolved IP is unsafe for a plugin
//! to reach; `check_url_safety` validates a full URL (scheme + host +
//! DNS resolution). Both `web` and `vision` plugins use this to enforce
//! the P0-22 policy. Adding a new outbound-HTTP plugin? Depend on this
//! crate — do not copy-paste the checks.

use std::net::{IpAddr, ToSocketAddrs};

/// Return `Some(reason)` if `ip` should never be reachable from a
/// fetched URL. Callers reject the request when this returns `Some`.
///
/// Covered ranges:
///   * IPv4 loopback (127/8), private (10/8, 172.16/12, 192.168/16),
///     link-local (169.254/16 — cloud metadata surface), broadcast /
///     unspecified / multicast, CGNAT (100.64/10), 0.0.0.0/8.
///   * IPv6 loopback (::1), unspecified (::), multicast (ff00::/8),
///     IPv4-mapped IPv6 (::ffff:0:0/96 — re-checked as v4), ULA
///     (fc00::/7), link-local (fe80::/10).
pub fn ip_is_forbidden(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if v4.is_loopback() {
                return Some("loopback address");
            }
            if v4.is_private() {
                return Some("RFC1918 private address");
            }
            if v4.is_link_local() {
                return Some("link-local address (cloud metadata surface)");
            }
            if v4.is_broadcast() || v4.is_unspecified() || v4.is_multicast() {
                return Some("special-purpose address");
            }
            if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 {
                return Some("CGNAT (100.64/10) address");
            }
            if octets[0] == 0 {
                return Some("0.0.0.0/8 address");
            }
            None
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Some("loopback address");
            }
            if v6.is_unspecified() || v6.is_multicast() {
                return Some("special-purpose address");
            }
            let seg = v6.segments();
            // IPv4-mapped IPv6 (::ffff:0:0/96) — unwrap and re-check as v4.
            if seg[0] == 0
                && seg[1] == 0
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0xffff
            {
                let mapped = std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return ip_is_forbidden(IpAddr::V4(mapped));
            }
            // ULA fc00::/7
            if (seg[0] & 0xfe00) == 0xfc00 {
                return Some("IPv6 ULA (fc00::/7)");
            }
            // Link-local fe80::/10
            if (seg[0] & 0xffc0) == 0xfe80 {
                return Some("IPv6 link-local (fe80::/10)");
            }
            None
        }
    }
}

/// Validate that a URL is safe to fetch: scheme is http(s), the parsed
/// host literal (if any) is not in a forbidden range, and — for
/// hostnames — DNS resolves to only allowed addresses. Callers wire
/// this into both the pre-flight check and every redirect hop.
///
/// Returns `Err(reason)` when the URL should be rejected.
pub fn check_url_safety(url_str: &str) -> Result<(), String> {
    let parsed = ::url::Url::parse(url_str).map_err(|_| format!("invalid URL: {url_str}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("only http/https allowed, got: {scheme}"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("URL missing host: {url_str}"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if let Some(reason) = ip_is_forbidden(ip) {
            return Err(format!("host {host} is forbidden ({reason})"));
        }
        return Ok(());
    }
    let addrs = (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?;
    let mut saw_any = false;
    for sa in addrs {
        saw_any = true;
        if let Some(reason) = ip_is_forbidden(sa.ip()) {
            return Err(format!(
                "host {host} resolves to forbidden address {}: {reason}",
                sa.ip()
            ));
        }
    }
    if !saw_any {
        return Err(format!("host {host} did not resolve to any address"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn rfc1918_and_metadata_and_docker_bridges_are_blocked() {
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(172, 20, 0, 1))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 1))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(100, 64, 1, 1))).is_some());
    }

    #[test]
    fn ipv6_loopback_ula_linklocal_are_blocked() {
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_some());
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1))).is_some());
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1))).is_some());
        assert!(
            ip_is_forbidden(IpAddr::V6("::ffff:127.0.0.1".parse().unwrap())).is_some(),
            "IPv4-mapped loopback must round-trip"
        );
    }

    #[test]
    fn public_ips_are_allowed() {
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_none());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))).is_none());
    }
}
