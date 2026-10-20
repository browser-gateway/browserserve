//! Refuses upstream proxies on the server's own or private network.

use super::ProxyError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// True for addresses a customer-supplied proxy must never point at:
/// loopback, private, link-local, carrier-grade NAT, unique-local,
/// multicast, unspecified, documentation and benchmarking ranges, and the
/// IPv4-mapped / IPv4-compatible / NAT64 forms of any of those.
#[must_use]
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => blocked_v6(v6),
    }
}

fn blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
}

fn blocked_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return blocked_v4(v4);
    }
    let seg = ip.segments();
    let compatible = seg[..6] == [0, 0, 0, 0, 0, 0] && !ip.is_unspecified() && !ip.is_loopback();
    let nat64 = seg[0] == 0x64 && seg[1] == 0xff9b;
    if compatible || nat64 {
        let [.., a, b, c, d] = ip.octets();
        return blocked_v4(Ipv4Addr::new(a, b, c, d));
    }
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00
        || (seg[0] & 0xffc0) == 0xfe80
        || (seg[0] & 0xffc0) == 0xfec0
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)
}

/// Resolves the proxy host once and keeps only addresses that may be dialled.
///
/// # Errors
///
/// [`ProxyError::AddressNotAllowed`] when the name does not resolve or every
/// address is blocked (unless `allow_private`).
pub(super) async fn resolve_allowed(
    host: &str,
    port: u16,
    allow_private: bool,
) -> Result<Vec<SocketAddr>, ProxyError> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| ProxyError::AddressNotAllowed("host does not resolve".into()))?
            .collect(),
    };
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| allow_private || !is_blocked_ip(a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(ProxyError::AddressNotAllowed(
            "host is on a private or local network".into(),
        ));
    }
    Ok(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(s: &str) -> bool {
        is_blocked_ip(s.parse().unwrap())
    }

    #[test]
    fn blocks_local_and_private_ranges() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::10.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2001:db8::1",
        ] {
            assert!(blocked(ip), "{ip} should be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!blocked(ip), "{ip} should be allowed");
        }
    }

    #[tokio::test]
    async fn resolves_literals_and_honours_the_private_switch() {
        assert!(resolve_allowed("127.0.0.1", 3128, false).await.is_err());
        assert_eq!(
            resolve_allowed("127.0.0.1", 3128, true)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(resolve_allowed("localhost", 3128, false).await.is_err());
        assert!(resolve_allowed("8.8.8.8", 3128, false).await.is_ok());
    }
}
