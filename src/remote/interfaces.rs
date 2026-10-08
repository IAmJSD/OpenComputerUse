//! The addresses the HTTP server can listen on, for the settings window's
//! dropdown: this computer only, every interface, and each address one of
//! this computer's network interfaces has, with Tailscale's marked.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub addr: String,
}

fn choice(label: impl Into<String>, addr: impl ToString) -> Choice {
    Choice {
        label: label.into(),
        addr: addr.to_string(),
    }
}

/// Tailscale's addresses: 100.64.0.0/10 for IPv4, fd7a:115c:a1e0::/48 for
/// IPv6.
fn is_tailscale(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40,
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// The choices for the dropdown given the interfaces' `(name, address)`
/// pairs: the fixed ones first, then each interface address that is not
/// loopback or link-local (an IPv6 link-local address cannot be listened on
/// without its interface's scope id).
pub fn choices_for(interfaces: &[(String, IpAddr)]) -> Vec<Choice> {
    let mut out = vec![
        choice("localhost, IPv4 (127.0.0.1)", Ipv4Addr::LOCALHOST),
        choice("localhost, IPv6 (::1)", Ipv6Addr::LOCALHOST),
        choice("Every interface, IPv4 (0.0.0.0)", Ipv4Addr::UNSPECIFIED),
        choice("Every interface, IPv6 (::)", Ipv6Addr::UNSPECIFIED),
    ];
    for (name, ip) in interfaces {
        let link_local = match ip {
            IpAddr::V4(v4) => v4.is_link_local(),
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
        };
        if ip.is_loopback() || ip.is_unspecified() || link_local {
            continue;
        }
        let kind = if is_tailscale(ip) {
            "Tailscale"
        } else if ip.is_ipv4() {
            "IPv4"
        } else {
            "IPv6"
        };
        if !out.iter().any(|c| c.addr == ip.to_string()) {
            out.push(choice(format!("{kind}: {ip} ({name})"), ip));
        }
    }
    out
}

/// The choices on this computer.
pub fn choices() -> Vec<Choice> {
    choices_for(&interface_addresses())
}

/// Every address of every interface that is up.
#[cfg(unix)]
fn interface_addresses() -> Vec<(String, IpAddr)> {
    use std::ffi::CStr;

    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that stays valid
    // until freeifaddrs; every pointer is checked for null before it is
    // read, and the sockaddr is cast only to the type its family names.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_flags & libc::IFF_UP as libc::c_uint == 0 {
                continue;
            }
            let ip = match (*ifa.ifa_addr).sa_family as libc::c_int {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            found.push((name, ip));
        }
        libc::freeifaddrs(list);
    }
    found
}

/// Windows has no `getifaddrs`; the fixed choices stand, and the settings
/// file still takes any address.
#[cfg(not(unix))]
fn interface_addresses() -> Vec<(String, IpAddr)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn fixed_choices_cover_localhost_and_every_interface() {
        let addrs: Vec<String> = choices_for(&[]).into_iter().map(|c| c.addr).collect();
        assert_eq!(addrs, ["127.0.0.1", "::1", "0.0.0.0", "::"]);
    }

    #[test]
    fn interface_addresses_are_labelled_by_kind() {
        let list = choices_for(&[
            ("lo0".into(), ip("127.0.0.1")),
            ("en0".into(), ip("192.168.1.5")),
            ("en0".into(), ip("2001:db8::5")),
            ("en0".into(), ip("fe80::1")),
            ("en0".into(), ip("169.254.3.4")),
            ("utun4".into(), ip("100.101.102.103")),
            ("utun4".into(), ip("fd7a:115c:a1e0::1")),
            ("en1".into(), ip("100.128.0.1")),
            ("en2".into(), ip("192.168.1.5")),
        ]);
        let labels: Vec<&str> = list[4..].iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "IPv4: 192.168.1.5 (en0)",
                "IPv6: 2001:db8::5 (en0)",
                "Tailscale: 100.101.102.103 (utun4)",
                "Tailscale: fd7a:115c:a1e0::1 (utun4)",
                // Just outside 100.64.0.0/10.
                "IPv4: 100.128.0.1 (en1)",
            ]
        );
    }

    #[test]
    fn this_computer_lists_its_own_loopback_choice() {
        assert!(choices().iter().any(|c| c.addr == "127.0.0.1"));
    }
}
