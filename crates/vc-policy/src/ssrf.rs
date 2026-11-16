//! The SSRF guard (spec §5.2): checked once per hop on the resolved, pinned IP.
//!
//! Blocks loopback, RFC 1918, link-local (169.254/16, fe80::/10), CGNAT (100.64/10), 0/8,
//! ULA (fc00::/7), IPv4-mapped/-compatible IPv6, NAT64/6to4/Teredo forms, multicast, reserved
//! and documentation ranges, the metadata server (by IP and by name) and every port other than
//! the configured ones (80/443). A host written as an IP literal in any WHATWG form (decimal,
//! octal, hex, shortened) is normalised and must be public too, and must equal the pinned IP.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Host;

use crate::denylist::{host_within, norm_host};

/// Host names of cloud metadata servers.
const METADATA_HOSTS: &[&str] = &["metadata.google.internal", "metadata.goog", "metadata"];

/// Metadata server IPs (GCP/AWS/Azure 169.254.169.254, AWS ECS 169.254.170.2, Oracle
/// 169.254.169.254, Alibaba 100.100.100.200) and the AWS IPv6 IMDS address.
fn metadata_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => matches!(
            v4.octets(),
            [169, 254, 169, 254] | [169, 254, 170, 2] | [100, 100, 100, 200]
        ),
        IpAddr::V6(v6) => v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    }
}

/// The rule an IPv4 address falls under, if it is not public.
pub fn v4_rule(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if metadata_ip(IpAddr::V4(ip)) {
        return Some("metadata");
    }
    let rule = match o {
        [0, ..] => "this_network",
        [127, ..] => "loopback",
        [10, ..] | [172, 16..=31, ..] | [192, 168, ..] => "rfc1918",
        [169, 254, ..] => "link_local",
        [100, 64..=127, ..] => "cgnat",
        [192, 0, 0, _] | [192, 0, 2, _] | [198, 18..=19, ..] | [198, 51, 100, _] => "reserved",
        [203, 0, 113, _] | [192, 88, 99, _] => "reserved",
        [224..=239, ..] => "multicast",
        [240..=255, ..] => "reserved",
        _ => return None,
    };
    Some(rule)
}

/// The rule an IPv6 address falls under, if it is not public.
pub fn v6_rule(ip: Ipv6Addr) -> Option<&'static str> {
    if metadata_ip(IpAddr::V6(ip)) {
        return Some("metadata");
    }
    let s = ip.segments();
    if ip.is_unspecified() {
        return Some("this_network");
    }
    if ip.is_loopback() {
        return Some("loopback");
    }
    // ::ffff:a.b.c.d (IPv4-mapped) is blocked outright, whatever the embedded address.
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return Some("ipv4_mapped");
    }
    // ::a.b.c.d (deprecated IPv4-compatible) and ::ffff:0:a.b.c.d (IPv4-translated).
    if s[..6] == [0; 6] || (s[..4] == [0; 4] && s[4] == 0xffff && s[5] == 0) {
        return Some("ipv4_mapped");
    }
    let rule = match s[0] {
        0xfc00..=0xfdff => "ula",
        0xfe80..=0xfebf => "link_local",
        0xfec0..=0xfeff => "reserved",
        0xff00..=0xffff => "multicast",
        // NAT64 64:ff9b::/96 and 64:ff9b:1::/48 embed IPv4 addresses.
        0x0064 if s[1] == 0xff9b => "nat64",
        // 6to4 and Teredo tunnel to IPv4.
        0x2002 => "6to4",
        0x2001 if s[1] == 0 => "teredo",
        0x2001 if s[1] == 0x0db8 => "reserved",
        0x0100 if s[1..4] == [0, 0, 0] => "reserved",
        _ => return None,
    };
    Some(rule)
}

/// The rule an IP falls under, if it is not a public unicast address.
pub fn ip_rule(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => v4_rule(v4),
        IpAddr::V6(v6) => v6_rule(v6),
    }
}

/// Parses `host` the way a URL parser would (WHATWG: decimal, octal and hex IPv4 forms,
/// bracketed IPv6) and returns the IP when it is an IP literal.
pub fn literal_ip(host: &str) -> Option<IpAddr> {
    let h = host.trim().trim_end_matches('.');
    if let Ok(ip) = h.parse::<IpAddr>() {
        return Some(ip);
    }
    match Host::parse(h) {
        Ok(Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Ok(Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    }
}

/// The SSRF decision: `Some(rule)` when blocked.
pub fn check(host: &str, ip: IpAddr, port: u16, allowed_ports: &[u16]) -> Option<&'static str> {
    if !allowed_ports.contains(&port) {
        return Some("port");
    }
    let name = norm_host(host);
    if METADATA_HOSTS.contains(&name.as_str()) {
        return Some("metadata");
    }
    if host_within(&name, "localhost") {
        return Some("loopback");
    }
    if host_within(&name, "internal") || host_within(&name, "local") {
        return Some("internal_name");
    }
    if name.is_empty() {
        return Some("empty_host");
    }
    if let Some(rule) = ip_rule(ip) {
        return Some(rule);
    }
    if let Some(lit) = literal_ip(&name) {
        if let Some(rule) = ip_rule(lit) {
            return Some(rule);
        }
        if lit != ip {
            return Some("ip_mismatch");
        }
    }
    None
}
