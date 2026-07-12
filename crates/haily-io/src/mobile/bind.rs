//! Bind-address selection (red team M2/M3) — a PURE function over an injected interface
//! list, so "a public/unknown interface is never bound" is provable in a unit test without
//! touching real OS network state. `enumerate_interfaces` is the thin, untested I/O edge that
//! feeds it real data.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// One local network interface, as reported by the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetInterface {
    pub name: String,
    pub ip: IpAddr,
}

/// Tailscale's documented CGNAT range (100.64.0.0/10) — the desktop's tailnet interface, if
/// Tailscale is running, always falls inside this block. IPv4-only for v1 (see the module's
/// deviation note); a future IPv6 tailnet address would need its own detection path.
fn is_tailnet_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000 // 100.64.0.0 – 100.127.255.255
}

/// RFC1918 private ranges MINUS the tailnet CGNAT block (that one is handled separately, always
/// on) and MINUS link-local (169.254.0.0/16, never a real LAN address a user picked).
fn is_private_lan(ip: Ipv4Addr) -> bool {
    if ip.is_link_local() || is_tailnet_cgnat(ip) {
        return false;
    }
    let o = ip.octets();
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

/// Select which addresses the mobile server binds to, given the current interface list and
/// whether the operator has opted in to a LAN-direct listener (M2 default: tailnet + loopback
/// ONLY; a public/unknown-class address is NEVER selected regardless of `lan_opt_in` — there is
/// no override for that). Pure: no I/O, deterministic on its inputs — this is what makes "public
/// never selected" a provable unit-test property rather than an assertion about live network
/// state.
pub fn select_bind_addrs(
    interfaces: &[NetInterface],
    lan_opt_in: bool,
    port: u16,
) -> Vec<SocketAddr> {
    let mut addrs = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)];

    for iface in interfaces {
        let IpAddr::V4(v4) = iface.ip else {
            // IPv6 tailnet/LAN detection deferred (see module doc) — never bind an
            // unclassified IPv6 address.
            continue;
        };
        if v4.is_loopback() {
            continue; // already covered above
        }
        let selectable = is_tailnet_cgnat(v4) || (lan_opt_in && is_private_lan(v4));
        if selectable {
            addrs.push(SocketAddr::new(IpAddr::V4(v4), port));
        }
    }

    addrs.sort_by_key(|a| a.ip());
    addrs.dedup();
    addrs
}

/// Whether a SELECTED bind address (one already returned by [`select_bind_addrs`]) needs TLS.
/// Loopback and tailnet addresses are served plain `ws://` (Tailscale's own WireGuard tunnel —
/// or the local machine itself — already provides transport security); anything else in the
/// selected set can only be a LAN-opt-in address, which gets `wss://` (red team M2: a
/// coffee-shop network has no equivalent built-in encryption).
pub fn requires_tls(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !is_tailnet_cgnat(v4),
        IpAddr::V6(_) => false,
    }
}

/// Real OS interface enumeration — the impure edge `select_bind_addrs` is tested independently
/// of. Best-effort: an enumeration failure (permissions, an exotic platform) degrades to "no
/// non-loopback interfaces found" rather than propagating an error, so the bind step can still
/// fall back to loopback-only (M11: never abort bootstrap over this).
pub fn enumerate_interfaces() -> Vec<NetInterface> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .map(|i| {
                let ip = i.ip();
                NetInterface { name: i.name, ip }
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                "mobile: interface enumeration failed, defaulting to loopback-only: {e:#}"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, ip: &str) -> NetInterface {
        NetInterface {
            name: name.to_string(),
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn loopback_is_always_included() {
        let addrs = select_bind_addrs(&[], false, 9443);
        assert!(addrs.contains(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9443)));
    }

    #[test]
    fn tailnet_interface_is_selected_by_default_without_lan_opt_in() {
        let ifaces = vec![iface("tailscale0", "100.101.102.103")];
        let addrs = select_bind_addrs(&ifaces, false, 9443);
        assert!(addrs
            .iter()
            .any(|a| a.ip() == "100.101.102.103".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn public_interface_is_never_selected_even_with_lan_opt_in() {
        let ifaces = vec![iface("eth0", "203.0.113.5")]; // TEST-NET-3, public-class
        let addrs = select_bind_addrs(&ifaces, true, 9443);
        assert!(!addrs
            .iter()
            .any(|a| a.ip() == "203.0.113.5".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn private_lan_interface_is_excluded_by_default_and_included_when_opted_in() {
        let ifaces = vec![iface("eth0", "192.168.1.50")];
        let without_opt_in = select_bind_addrs(&ifaces, false, 9443);
        assert!(!without_opt_in
            .iter()
            .any(|a| a.ip() == "192.168.1.50".parse::<IpAddr>().unwrap()));

        let with_opt_in = select_bind_addrs(&ifaces, true, 9443);
        assert!(with_opt_in
            .iter()
            .any(|a| a.ip() == "192.168.1.50".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn link_local_interface_is_never_selected() {
        let ifaces = vec![iface("eth0", "169.254.1.1")];
        let addrs = select_bind_addrs(&ifaces, true, 9443);
        assert!(!addrs
            .iter()
            .any(|a| a.ip() == "169.254.1.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn tailnet_cgnat_boundary_is_exact() {
        // 100.63.x.x is just OUTSIDE the /10 block — must never be treated as tailnet.
        assert!(!is_tailnet_cgnat("100.63.255.255".parse().unwrap()));
        assert!(is_tailnet_cgnat("100.64.0.0".parse().unwrap()));
        assert!(is_tailnet_cgnat("100.127.255.255".parse().unwrap()));
        assert!(!is_tailnet_cgnat("100.128.0.0".parse().unwrap()));
    }

    #[test]
    fn requires_tls_is_false_for_loopback_and_tailnet_true_for_lan() {
        assert!(!requires_tls(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!requires_tls("100.64.1.1".parse().unwrap()));
        assert!(requires_tls("192.168.1.50".parse().unwrap()));
    }

    #[test]
    fn multiple_interfaces_combine_loopback_and_tailnet() {
        let ifaces = vec![
            iface("tailscale0", "100.64.5.5"),
            iface("eth0", "203.0.113.9"),
        ];
        let addrs = select_bind_addrs(&ifaces, false, 443);
        assert_eq!(
            addrs.len(),
            2,
            "loopback + tailnet only, public excluded: {addrs:?}"
        );
    }
}
