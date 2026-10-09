//! Limiting the HTTP server to some of this computer's addresses, chosen by
//! network adapter, address or range: which adapters this computer has, and
//! the addresses the server listens on for a choice. Addresses come and go
//! (Wi-Fi joins, Tailscale connects), so the server asks again every few
//! seconds and follows.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _};

use anyhow::{anyhow, Result};

use crate::config::HttpConfig;

/// The name that picks whichever adapter has this computer's Tailscale
/// address. On macOS that adapter is a `utunN` whose number can change when
/// Tailscale restarts, so naming it directly would break.
pub const TAILSCALE: &str = "tailscale";

/// A network adapter and the addresses it has now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adapter {
    pub name: String,
    pub addrs: Vec<IpAddr>,
}

/// Tailscale's addresses: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
pub fn is_tailscale(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40,
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// This computer's adapters that have an address, by name. Link-local IPv6
/// addresses are left out (listening on one needs its scope id, and nobody
/// reaches a computer by one).
pub fn adapters() -> Vec<Adapter> {
    let found: Vec<(String, IpAddr)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| {
            let ip = i.ip();
            (i.name, ip)
        })
        .collect();
    group(&found)
}

fn group(found: &[(String, IpAddr)]) -> Vec<Adapter> {
    let mut out: Vec<Adapter> = Vec::new();
    for (name, ip) in found {
        if let IpAddr::V6(v6) = ip {
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                continue;
            }
        }
        match out.iter_mut().find(|a| &a.name == name) {
            Some(a) if !a.addrs.contains(ip) => a.addrs.push(*ip),
            Some(_) => {}
            None => out.push(Adapter {
                name: name.clone(),
                addrs: vec![*ip],
            }),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One `listen_on` entry: what it picks among this computer's addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Every address of the adapter with this name.
    Adapter(String),
    /// The Tailscale addresses, on whichever adapter has them.
    Tailscale,
    /// The addresses in a range; a single address is a range of one.
    Range(IpAddr, u8),
}

impl Filter {
    pub fn parse(entry: &str) -> Result<Self> {
        let entry = entry.trim();
        anyhow::ensure!(!entry.is_empty(), "an empty adapter or address");
        if entry.eq_ignore_ascii_case(TAILSCALE) {
            return Ok(Self::Tailscale);
        }
        if let Ok(ip) = entry.parse::<IpAddr>() {
            return Ok(Self::Range(ip, bits(&ip)));
        }
        if let Some((ip, prefix)) = entry.split_once('/') {
            let bad =
                || anyhow!("{entry} is not a range like 192.168.1.0/24 or fd7a:115c:a1e0::/48");
            let ip: IpAddr = ip.parse().map_err(|_| bad())?;
            let prefix: u8 = prefix.parse().map_err(|_| bad())?;
            anyhow::ensure!(prefix <= bits(&ip), bad());
            return Ok(Self::Range(ip, prefix));
        }
        // Anything else names an adapter; on Windows those are names like
        // "Wi-Fi 2".
        Ok(Self::Adapter(entry.to_string()))
    }

    fn picks(&self, adapter: &str, ip: &IpAddr) -> bool {
        match self {
            Self::Adapter(name) => name == adapter,
            Self::Tailscale => is_tailscale(ip),
            Self::Range(net, prefix) => in_range(ip, net, *prefix),
        }
    }
}

fn bits(ip: &IpAddr) -> u8 {
    if ip.is_ipv4() {
        32
    } else {
        128
    }
}

fn in_range(ip: &IpAddr, net: &IpAddr, prefix: u8) -> bool {
    let (a, b) = match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => (u32::from(*a) as u128, u32::from(*b) as u128),
        (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(*a), u128::from(*b)),
        _ => return false,
    };
    let shift = bits(ip) - prefix;
    shift == bits(ip) || a >> shift == b >> shift
}

/// The addresses among `adapters` that `listen_on` picks. Entries that don't
/// parse pick nothing (they are refused when set, so only a hand-edited
/// settings file has them).
pub fn chosen_addrs(listen_on: &[String], adapters: &[Adapter]) -> Vec<IpAddr> {
    let filters: Vec<Filter> = listen_on
        .iter()
        .filter_map(|e| Filter::parse(e).ok())
        .collect();
    let mut out = Vec::new();
    for a in adapters {
        for ip in &a.addrs {
            if filters.iter().any(|f| f.picks(&a.name, ip)) && !out.contains(ip) {
                out.push(*ip);
            }
        }
    }
    out
}

/// Where the server listens now: on `bind` when no adapters are chosen, or
/// on every address `listen_on` picks. That can be none, while the chosen
/// adapters are down.
pub fn listen_addrs(http: &HttpConfig) -> Result<Vec<SocketAddr>> {
    if http.listen_on.is_empty() {
        return bind_addrs(&http.bind, http.port);
    }
    Ok(chosen_addrs(&http.listen_on, &adapters())
        .into_iter()
        .map(|ip| SocketAddr::new(ip, http.port))
        .collect())
}

/// `bind` (an address, or a host name to look up) with the port.
fn bind_addrs(bind: &str, port: u16) -> Result<Vec<SocketAddr>> {
    if let Ok(ip) = bind.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = (bind, port)
        .to_socket_addrs()
        .map_err(|e| anyhow!("can't look up {bind}: {e}"))?
        .take(1)
        .collect();
    Ok(addrs)
}

/// A short list of addresses for a label: IPv4 first, and past `show`, a
/// count of the rest (IPv6 privacy addresses alone can run to several).
pub fn summary<T: ToString>(addrs: &[T], show: usize) -> String {
    let mut all: Vec<String> = addrs.iter().map(ToString::to_string).collect();
    // IPv6 addresses, bare or bracketed with a port, go last.
    all.sort_by_key(|a| a.starts_with('[') || a.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()));
    if all.len() <= show {
        return all.join(", ");
    }
    format!("{} and {} more", all[..show].join(", "), all.len() - show)
}

/// `listen_on` entries from the user, checked: trimmed, without blanks or
/// repeats, and refused if one is not an adapter name, address or range.
pub fn clean(entries: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for e in entries.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
        Filter::parse(e)?;
        if !out.iter().any(|o| o == e) {
            out.push(e.to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn sample() -> Vec<Adapter> {
        group(&[
            ("lo0".into(), ip("127.0.0.1")),
            ("lo0".into(), ip("::1")),
            ("en0".into(), ip("192.168.1.5")),
            ("en0".into(), ip("fe80::1")),
            ("en0".into(), ip("2001:db8::5")),
            ("utun4".into(), ip("100.101.102.103")),
            ("utun4".into(), ip("fd7a:115c:a1e0::1")),
            ("utun4".into(), ip("100.101.102.103")),
            ("en1".into(), ip("100.128.0.1")),
        ])
    }

    #[test]
    fn adapters_are_grouped_without_link_local() {
        let a = sample();
        let names: Vec<&str> = a.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["en0", "en1", "lo0", "utun4"]);
        assert_eq!(a[0].addrs, [ip("192.168.1.5"), ip("2001:db8::5")]);
        assert_eq!(a[3].addrs.len(), 2);
    }

    #[test]
    fn chosen_adapters_give_their_addresses() {
        let a = sample();
        assert_eq!(
            chosen_addrs(&["en0".into()], &a),
            [ip("192.168.1.5"), ip("2001:db8::5")]
        );
        assert_eq!(chosen_addrs(&["en9".into()], &a), Vec::<IpAddr>::new());
        // In the adapters' order, each address once.
        assert_eq!(
            chosen_addrs(&["utun4".into(), "lo0".into(), "lo0".into()], &a),
            [
                ip("127.0.0.1"),
                ip("::1"),
                ip("100.101.102.103"),
                ip("fd7a:115c:a1e0::1")
            ]
        );
    }

    #[test]
    fn tailscale_picks_tailscale_addresses_on_any_adapter() {
        // en1's 100.128.0.1 is just outside 100.64.0.0/10.
        assert_eq!(
            chosen_addrs(&["tailscale".into()], &sample()),
            [ip("100.101.102.103"), ip("fd7a:115c:a1e0::1")]
        );
    }

    #[test]
    fn addresses_and_ranges_pick_matching_addresses() {
        let a = sample();
        let pick =
            |e: &[&str]| chosen_addrs(&e.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &a);
        assert_eq!(pick(&["192.168.1.5"]), [ip("192.168.1.5")]);
        // An address this computer doesn't have picks nothing.
        assert_eq!(pick(&["192.168.1.6"]), Vec::<IpAddr>::new());
        assert_eq!(pick(&["192.168.0.0/16"]), [ip("192.168.1.5")]);
        assert_eq!(pick(&["2001:db8::/32"]), [ip("2001:db8::5")]);
        assert_eq!(pick(&["100.64.0.0/10"]), [ip("100.101.102.103")]);
        assert_eq!(pick(&["0.0.0.0/0"]).len(), 4);
        assert_eq!(
            pick(&["en0", "10.0.0.0/8"]),
            [ip("192.168.1.5"), ip("2001:db8::5")]
        );
    }

    #[test]
    fn entries_are_parsed() {
        assert_eq!(Filter::parse("en0").unwrap(), Filter::Adapter("en0".into()));
        assert_eq!(Filter::parse(" Tailscale ").unwrap(), Filter::Tailscale);
        assert_eq!(Filter::parse("::1").unwrap(), Filter::Range(ip("::1"), 128));
        assert_eq!(
            Filter::parse("10.0.0.0/8").unwrap(),
            Filter::Range(ip("10.0.0.0"), 8)
        );
        assert!(Filter::parse("10.0.0.0/33").is_err());
        assert!(Filter::parse("10.0.0/8").is_err());
        assert_eq!(
            Filter::parse("Wi-Fi 2").unwrap(),
            Filter::Adapter("Wi-Fi 2".into())
        );
        assert!(Filter::parse("").is_err());
    }

    #[test]
    fn no_adapters_listens_on_bind() {
        let http = HttpConfig::default();
        assert_eq!(
            listen_addrs(&http).unwrap(),
            [SocketAddr::new(ip("0.0.0.0"), 8642)]
        );
        assert_eq!(
            bind_addrs("::1", 9000).unwrap(),
            [SocketAddr::new(ip("::1"), 9000)]
        );
    }

    #[test]
    fn summaries_put_ipv4_first_and_count_the_rest() {
        assert_eq!(summary(&[ip("::1"), ip("127.0.0.1")], 2), "127.0.0.1, ::1");
        assert_eq!(
            summary(
                &[
                    ip("2001:db8::1"),
                    ip("2001:db8::2"),
                    ip("192.168.1.5"),
                    ip("2001:db8::3")
                ],
                2
            ),
            "192.168.1.5, 2001:db8::1 and 2 more"
        );
        let sock = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            summary(&[sock("[::1]:80"), sock("127.0.0.1:80")], 1),
            "127.0.0.1:80 and 1 more"
        );
    }

    #[test]
    fn names_are_cleaned() {
        let names = clean(&[" en0 ".into(), "".into(), "en0".into(), "10.0.0.0/8".into()]);
        assert_eq!(names.unwrap(), ["en0", "10.0.0.0/8"]);
        assert!(clean(&["10.0.0.0/99".into()]).is_err());
    }

    #[test]
    fn this_computer_has_a_loopback_adapter() {
        assert!(adapters()
            .iter()
            .any(|a| a.addrs.iter().any(IpAddr::is_loopback)));
    }
}
