//! Endpoint selection order (researcher-03 §3, "Also apply"): Tailscale MagicDNS (only if it
//! actually resolves — checked, not assumed) → Android mDNS discovery (platform-only,
//! injected as a candidate here since this crate is host-testable and has no OS mDNS API) →
//! the QR's own literal host (loopback for `adb reverse` dev loop, or a LAN IP under M2's
//! opt-in). The selection function itself is pure so it is unit-testable without touching a
//! real resolver or the network; `resolve_via_dns` below is the one impure edge, isolated so
//! `src-tauri-mobile` can swap in an actual platform mDNS lookup later without touching the
//! ordering logic.
use haily_types::PairingQr;
use std::net::SocketAddr;

/// One candidate the client can try dialing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    fn from_qr(qr: &PairingQr) -> Self {
        Self {
            host: qr.host.clone(),
            port: qr.port,
        }
    }
}

/// Which rung of the endpoint order was actually selected — surfaced to the connection-state
/// banner (e.g. "connected via Tailscale" vs "connected via local network") and to logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSource {
    MagicDns,
    Mdns,
    QrLiteral,
}

/// Pure selection: given the QR's own host/port, an injected "does MagicDNS resolve this host"
/// check, and an optional mDNS-discovered candidate, returns the endpoint to dial first plus
/// which source it came from. `magicdns_resolves` is a plain `bool` (not a live resolver call)
/// so this function has no I/O and is fully deterministic under test — the actual DNS lookup
/// happens in [`resolve_via_dns`] and is passed in by the caller.
pub fn select_endpoint(
    qr: &PairingQr,
    magicdns_resolves: bool,
    mdns_candidate: Option<Endpoint>,
) -> (Endpoint, EndpointSource) {
    if magicdns_resolves {
        return (Endpoint::from_qr(qr), EndpointSource::MagicDns);
    }
    if let Some(candidate) = mdns_candidate {
        return (candidate, EndpointSource::Mdns);
    }
    // The QR's host doubles as the literal fallback — a raw loopback/LAN address the desktop
    // rendered directly (M2's opt-in path) or a tailnet MagicDNS name treated as an opaque
    // string when resolution couldn't be confirmed (dialing it directly still works if the
    // OS resolver quietly succeeds later; this rung is "stop being clever, just try it").
    (Endpoint::from_qr(qr), EndpointSource::QrLiteral)
}

/// Best-effort DNS resolve-check (the impure edge `select_endpoint` is parameterized over).
/// Returns `false` on any resolution failure — a host that doesn't resolve is treated
/// identically to "MagicDNS not present", never a hard error, since a resolve failure here
/// just means "try the next rung of the endpoint order", not "abort connecting".
pub async fn resolve_via_dns(host: &str, port: u16) -> bool {
    tokio::net::lookup_host((host, port))
        .await
        .map(|mut addrs| addrs.next().is_some())
        .unwrap_or(false)
}

/// Formats an [`Endpoint`] as the `wss://` URL `ws.rs` connects to.
pub fn websocket_url(endpoint: &Endpoint) -> String {
    format!("wss://{}:{}/ws", endpoint.host, endpoint.port)
}

/// Parses `host:port` for the loopback dev loop (`adb reverse tcp:PORT tcp:PORT`, M12) or a
/// manually-entered pairing fallback — not used for the QR path, which already carries
/// host/port as separate fields.
pub fn parse_host_port(input: &str) -> Option<(String, u16)> {
    let addr: SocketAddr = input.parse().ok()?;
    Some((addr.ip().to_string(), addr.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qr(host: &str) -> PairingQr {
        PairingQr {
            host: host.to_string(),
            port: 7443,
            cert_fingerprint: "sha256:deadbeef".into(),
            pairing_code: "123456".into(),
            expires_at: "2026-07-12T00:00:00Z".into(),
        }
    }

    #[test]
    fn prefers_magicdns_when_it_resolves() {
        let q = qr("haily.tailnet.ts.net");
        let (endpoint, source) = select_endpoint(&q, true, None);
        assert_eq!(source, EndpointSource::MagicDns);
        assert_eq!(endpoint.host, "haily.tailnet.ts.net");
    }

    #[test]
    fn falls_back_to_mdns_candidate_when_magicdns_does_not_resolve() {
        let q = qr("haily.tailnet.ts.net");
        let mdns = Endpoint {
            host: "192.168.1.50".into(),
            port: 7443,
        };
        let (endpoint, source) = select_endpoint(&q, false, Some(mdns.clone()));
        assert_eq!(source, EndpointSource::Mdns);
        assert_eq!(endpoint, mdns);
    }

    #[test]
    fn falls_back_to_qr_literal_host_when_neither_magicdns_nor_mdns_available() {
        let q = qr("127.0.0.1");
        let (endpoint, source) = select_endpoint(&q, false, None);
        assert_eq!(source, EndpointSource::QrLiteral);
        assert_eq!(endpoint.host, "127.0.0.1");
    }

    #[test]
    fn magicdns_takes_priority_over_an_available_mdns_candidate() {
        let q = qr("haily.tailnet.ts.net");
        let mdns = Endpoint {
            host: "192.168.1.50".into(),
            port: 7443,
        };
        let (_, source) = select_endpoint(&q, true, Some(mdns));
        assert_eq!(source, EndpointSource::MagicDns);
    }

    #[test]
    fn websocket_url_is_always_wss() {
        let endpoint = Endpoint {
            host: "127.0.0.1".into(),
            port: 7443,
        };
        assert_eq!(websocket_url(&endpoint), "wss://127.0.0.1:7443/ws");
    }

    #[test]
    fn parse_host_port_reads_ipv4_socket_addr() {
        let parsed = parse_host_port("127.0.0.1:7443").expect("parse");
        assert_eq!(parsed, ("127.0.0.1".to_string(), 7443));
    }

    #[test]
    fn parse_host_port_rejects_a_bare_hostname_without_port() {
        assert!(parse_host_port("haily.tailnet.ts.net").is_none());
    }

    #[tokio::test]
    async fn resolve_via_dns_returns_false_for_an_unresolvable_host() {
        assert!(!resolve_via_dns("this-host-does-not-exist.invalid", 7443).await);
    }

    #[tokio::test]
    async fn resolve_via_dns_returns_true_for_loopback() {
        assert!(resolve_via_dns("localhost", 7443).await);
    }
}
