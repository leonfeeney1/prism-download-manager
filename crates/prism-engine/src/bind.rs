//! Bonding: spread one download's segments across every usable network at
//! once (Wi-Fi, Ethernet, a phone hotspot) instead of a single interface.
//!
//! Each segment worker binds its socket to one of the machine's local
//! addresses, round-robin. An address that cannot reach the server fails its
//! connection like any other worker failure, and the supervisor reassigns the
//! segment to a worker on a working interface — so a dead interface costs
//! nothing but the segments it tried.

use std::net::IpAddr;

/// Usable local addresses for outbound connections: every non-loopback
/// address of every interface (IPv4, plus IPv6 except link-local, which is
/// unusable without a scope id). Empty when enumeration fails.
pub fn detect_interfaces() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return out;
    };
    for iface in ifaces {
        let ip = iface.addr.ip();
        if ip.is_loopback() {
            continue;
        }
        if let IpAddr::V6(v6) = ip {
            // fe80::/10 link-local: no scope id to bind with, skip it.
            if v6.segments()[0] & 0xffc0 == 0xfe80 {
                continue;
            }
        }
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// The address a worker binds to: round-robin over the usable addresses.
/// `None` means "let the OS pick" (bonding off, or only one interface).
pub fn address_for_worker(addresses: &[IpAddr], worker: usize) -> Option<IpAddr> {
    if addresses.is_empty() {
        None
    } else {
        Some(addresses[worker % addresses.len()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_spreads_workers() {
        let addrs = vec![
            "192.168.1.2".parse().unwrap(),
            "10.0.0.5".parse().unwrap(),
        ];
        assert_eq!(address_for_worker(&addrs, 0), Some(addrs[0]));
        assert_eq!(address_for_worker(&addrs, 1), Some(addrs[1]));
        assert_eq!(address_for_worker(&addrs, 2), Some(addrs[0]));
        assert_eq!(address_for_worker(&addrs, 3), Some(addrs[1]));
        assert_eq!(address_for_worker(&[], 0), None);
        assert_eq!(address_for_worker(&addrs[..1], 7), Some(addrs[0]));
    }

    #[test]
    fn detection_returns_usable_addresses_only() {
        // The sandbox has at least one non-loopback address; whatever comes
        // back must never be loopback or IPv6 link-local.
        for ip in detect_interfaces() {
            assert!(!ip.is_loopback(), "{ip} is loopback");
            if let IpAddr::V6(v6) = ip {
                assert_ne!(v6.segments()[0] & 0xffc0, 0xfe80, "{ip} is link-local");
            }
        }
    }
}
