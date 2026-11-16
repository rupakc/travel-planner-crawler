//! The SSRF guard (spec §5.2): every listed range, in every host encoding.

mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use common::policy;
use proptest::prelude::*;
use vc_core::policy::Policy;

/// (base, prefix length, rule) for every IPv4 range the spec lists.
const V4_RANGES: &[([u8; 4], u8, &str)] = &[
    ([0, 0, 0, 0], 8, "this_network"),
    ([127, 0, 0, 0], 8, "loopback"),
    ([10, 0, 0, 0], 8, "rfc1918"),
    ([172, 16, 0, 0], 12, "rfc1918"),
    ([192, 168, 0, 0], 16, "rfc1918"),
    ([169, 254, 0, 0], 16, "link_local"),
    ([100, 64, 0, 0], 10, "cgnat"),
];

fn v4_in(base: [u8; 4], prefix: u8, r: u32) -> Ipv4Addr {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ipv4Addr::from((u32::from(Ipv4Addr::from(base)) & mask) | (r & !mask))
}

/// Host spellings a URL parser normalises to the same IPv4 address.
fn encodings(ip: Ipv4Addr) -> Vec<String> {
    let o = ip.octets();
    let n = u32::from(ip);
    vec![
        ip.to_string(),
        n.to_string(),
        format!("0{:o}", n),
        format!("0x{:x}", n),
        format!("0{:o}.0{:o}.0{:o}.0{:o}", o[0], o[1], o[2], o[3]),
        format!("0x{:x}.0x{:x}.0x{:x}.0x{:x}", o[0], o[1], o[2], o[3]),
        format!(
            "{}.{}",
            o[0],
            (u32::from(o[1]) << 16) | (u32::from(o[2]) << 8) | u32::from(o[3])
        ),
        format!("[::ffff:{ip}]"),
        format!("{ip}."),
    ]
}

const PUBLIC: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn ssrf_blocks_all_listed_ranges(idx in 0..V4_RANGES.len(), r in any::<u32>()) {
        let p = policy();
        let (base, prefix, rule) = V4_RANGES[idx];
        let ip = v4_in(base, prefix, r);
        // The pinned IP itself, whatever the host is called.
        let err = p.ssrf_check("example.com", IpAddr::V4(ip), 443).unwrap_err();
        prop_assert!(err.rule == rule || err.rule == "metadata", "{} -> {}", ip, err.rule);
        // The IPv4-mapped IPv6 form of the pinned IP.
        let mapped = IpAddr::V6(ip.to_ipv6_mapped());
        prop_assert_eq!(p.ssrf_check("example.com", mapped, 443).unwrap_err().rule, "ipv4_mapped");
        // A host literal in any encoding is blocked even when the pinned IP looks public.
        for host in encodings(ip) {
            prop_assert!(p.ssrf_check(&host, IpAddr::V4(PUBLIC), 80).is_err(), "{}", host);
            prop_assert!(p.ssrf_check(&host, IpAddr::V4(ip), 80).is_err(), "{}", host);
        }
    }

    #[test]
    fn ssrf_blocks_ula_and_link_local_v6(r in any::<u128>()) {
        let p = policy();
        let ula = Ipv6Addr::from((0xfc00u128 << 112) | (r & ((1u128 << 121) - 1)));
        prop_assert_eq!(p.ssrf_check("example.com", IpAddr::V6(ula), 443).unwrap_err().rule, "ula");
        let ll = Ipv6Addr::from((0xfe80u128 << 112) | (r & ((1u128 << 118) - 1)));
        prop_assert_eq!(p.ssrf_check("example.com", IpAddr::V6(ll), 443).unwrap_err().rule, "link_local");
        let host = format!("[{ula}]");
        prop_assert!(p.ssrf_check(&host, IpAddr::V4(PUBLIC), 443).is_err());
    }

    #[test]
    fn ssrf_only_ports_80_and_443(port in any::<u16>()) {
        let p = policy();
        let res = p.ssrf_check("example.com", IpAddr::V4(PUBLIC), port);
        if port == 80 || port == 443 {
            prop_assert!(res.is_ok());
        } else {
            prop_assert_eq!(res.unwrap_err().rule, "port");
        }
    }
}

#[test]
fn ssrf_metadata_server_by_ip_and_name() {
    let p = policy();
    let md = IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254));
    assert_eq!(
        p.ssrf_check("example.com", md, 80).unwrap_err().rule,
        "metadata"
    );
    for host in [
        "metadata.google.internal",
        "METADATA.GOOGLE.INTERNAL.",
        "metadata",
    ] {
        assert_eq!(
            p.ssrf_check(host, IpAddr::V4(PUBLIC), 80).unwrap_err().rule,
            "metadata"
        );
    }
    let aws6: IpAddr = "fd00:ec2::254".parse().unwrap();
    assert_eq!(
        p.ssrf_check("example.com", aws6, 80).unwrap_err().rule,
        "metadata"
    );
    assert!(p.ssrf_check("2852039166", IpAddr::V4(PUBLIC), 80).is_err());
}

#[test]
fn ssrf_other_special_addresses() {
    let p = policy();
    for (ip, rule) in [
        ("::1", "loopback"),
        ("::", "this_network"),
        ("::7f00:1", "ipv4_mapped"),
        ("64:ff9b::a00:1", "nat64"),
        ("2002:a00:1::", "6to4"),
        ("2001:0:4136:e378::1", "teredo"),
        ("ff02::1", "multicast"),
        ("2001:db8::1", "reserved"),
        ("224.0.0.1", "multicast"),
        ("255.255.255.255", "reserved"),
        ("192.0.2.1", "reserved"),
        ("198.18.0.1", "reserved"),
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert_eq!(
            p.ssrf_check("example.com", ip, 443).unwrap_err().rule,
            rule,
            "{ip}"
        );
    }
    assert_eq!(
        p.ssrf_check("localhost", IpAddr::V4(PUBLIC), 80)
            .unwrap_err()
            .rule,
        "loopback"
    );
    assert_eq!(
        p.ssrf_check("db.localhost", IpAddr::V4(PUBLIC), 80)
            .unwrap_err()
            .rule,
        "loopback"
    );
    assert_eq!(
        p.ssrf_check("vm.c.project.internal", IpAddr::V4(PUBLIC), 80)
            .unwrap_err()
            .rule,
        "internal_name"
    );
}

#[test]
fn ssrf_public_addresses_pass_and_mismatch_fails() {
    let p = policy();
    assert!(p.ssrf_check("example.com", IpAddr::V4(PUBLIC), 443).is_ok());
    let v6: IpAddr = "2606:2800:220:1:248:1893:25c8:1946".parse().unwrap();
    assert!(p.ssrf_check("example.com", v6, 443).is_ok());
    assert!(
        p.ssrf_check("93.184.216.34", IpAddr::V4(PUBLIC), 443)
            .is_ok()
    );
    assert!(p.ssrf_check("1572395042", IpAddr::V4(PUBLIC), 443).is_ok());
    let other = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
    assert_eq!(
        p.ssrf_check("93.184.216.34", other, 443).unwrap_err().rule,
        "ip_mismatch"
    );
    let err = p
        .ssrf_check("example.com", IpAddr::V4(Ipv4Addr::LOCALHOST), 443)
        .unwrap_err();
    assert_eq!(err.host, "example.com");
    assert_eq!(err.port, 443);
}
