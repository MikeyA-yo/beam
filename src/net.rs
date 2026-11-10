use std::net::{IpAddr, Ipv4Addr};

/// Interface names that usually belong to virtual adapters the phone can't reach
/// (WSL, Hyper-V, VirtualBox, VMware, Docker). They are listed last, not hidden.
const VIRTUAL_HINTS: &[&str] = &[
    "vethernet",
    "virtualbox",
    "vbox",
    "vmware",
    "vmnet",
    "docker",
    "wsl",
    "hyper-v",
    "loopback",
];

/// LAN IPv4 candidates for this machine, best first. Empty if detection fails.
pub fn lan_candidates() -> Vec<Ipv4Addr> {
    match local_ip_address::list_afinet_netifas() {
        Ok(ifaces) => rank(ifaces),
        Err(e) => {
            tracing::debug!("listing network interfaces failed: {e}");
            Vec::new()
        }
    }
}

/// Filters out addresses a phone can't use and orders the rest: physical adapters
/// before virtual ones, then 192.168/16, 10/8, 172.16/12, anything else.
pub fn rank(ifaces: Vec<(String, IpAddr)>) -> Vec<Ipv4Addr> {
    let mut found: Vec<(bool, u8, Ipv4Addr)> = Vec::new();
    for (name, ip) in ifaces {
        let IpAddr::V4(v4) = ip else { continue };
        if v4.is_loopback()
            || v4.is_link_local()
            || v4.is_unspecified()
            || v4.is_broadcast()
            || v4.is_multicast()
        {
            continue;
        }
        if found.iter().any(|(_, _, seen)| *seen == v4) {
            continue;
        }
        let lname = name.to_lowercase();
        let is_virtual = VIRTUAL_HINTS.iter().any(|h| lname.contains(h));
        found.push((is_virtual, range_rank(v4), v4));
    }
    // Stable sort keeps the OS order within a bucket.
    found.sort_by_key(|(is_virtual, rank, _)| (*is_virtual, *rank));
    found.into_iter().map(|(_, _, ip)| ip).collect()
}

fn range_rank(ip: Ipv4Addr) -> u8 {
    match ip.octets() {
        [192, 168, ..] => 0,
        [10, ..] => 1,
        [172, b, ..] if (16..=31).contains(&b) => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }

    #[test]
    fn skips_loopback_link_local_and_ipv6() {
        let got = rank(vec![
            ("lo".into(), v4("127.0.0.1")),
            ("eth".into(), v4("169.254.10.2")),
            ("eth".into(), "fe80::1".parse().unwrap()),
            ("wifi".into(), v4("192.168.1.5")),
        ]);
        assert_eq!(got, vec!["192.168.1.5".parse::<Ipv4Addr>().unwrap()]);
    }

    #[test]
    fn prefers_private_ranges_in_order() {
        let got = rank(vec![
            ("a".into(), v4("100.64.0.3")),
            ("b".into(), v4("172.20.0.4")),
            ("c".into(), v4("10.0.0.2")),
            ("d".into(), v4("192.168.43.10")),
        ]);
        let want: Vec<Ipv4Addr> = ["192.168.43.10", "10.0.0.2", "172.20.0.4", "100.64.0.3"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn virtual_adapters_go_last() {
        let got = rank(vec![
            ("vEthernet (WSL)".into(), v4("192.168.80.1")),
            ("Wi-Fi".into(), v4("10.1.2.3")),
        ]);
        assert_eq!(got[0], "10.1.2.3".parse::<Ipv4Addr>().unwrap());
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn dedups() {
        let got = rank(vec![
            ("a".into(), v4("192.168.1.5")),
            ("b".into(), v4("192.168.1.5")),
        ]);
        assert_eq!(got.len(), 1);
    }
}
