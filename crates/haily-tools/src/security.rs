/// SSRF and injection guards for all outbound/tool activity.
use anyhow::{bail, Result};
use reqwest::{RequestBuilder, Response};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

/// Hard cap on HTML input accepted by [`html_to_text`] — bounds parse time and
/// allocation for arbitrarily large fetched pages. 1MB comfortably covers real
/// article/doc pages; anything larger is truncated before parsing rather than
/// rejected outright, since the caller (`url_fetch`) already truncates output.
const HTML_MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Per-file diff size cap for `worktree_apply`'s untracked-file inlining.
pub const DIFF_MAX_FILE_BYTES: usize = 256 * 1024;
/// Total diff size cap (tracked + untracked combined) for `worktree_apply`.
pub const DIFF_MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;

/// Marker appended when a diff (or a file within it) is cut short by a size cap —
/// deliberately distinct from a truncated-mid-line output so callers/users can tell
/// "this is incomplete by design" from "this is corrupted".
pub const TRUNCATED_MARKER: &str = "\n[truncated]\n";

// Known cloud metadata endpoints to block regardless of IP classification.
const BLOCKED_HOSTS: &[&str] = &[
    "169.254.169.254", // AWS/GCP/Azure IMDS
    "metadata.google.internal",
    "100.100.100.200", // Alibaba Cloud metadata
    "fd00:ec2::254",   // AWS IPv6 IMDS
];

/// Maximum redirect hops a caller may manually follow after an `ssrf_guard` re-check
/// per hop. Mirrors a conservative real-world default (browsers commonly cap at 20;
/// tool-initiated fetches have no legitimate need for anywhere near that many).
pub const MAX_REDIRECT_HOPS: u8 = 5;

/// Validated request target: the host/port to connect to, pre-vetted against
/// SSRF-blocked ranges, and the exact `SocketAddr` to pin the connection to so the
/// checked IP is provably the connected IP (closes the DNS-rebind TOCTOU window
/// between "resolve for the check" and "resolve again to actually connect").
#[derive(Debug, Clone, Copy)]
pub struct VettedAddr {
    pub addr: SocketAddr,
}

/// Block requests to private networks, loopback, link-local, and cloud metadata, and
/// return the exact `SocketAddr` that was vetted.
///
/// Call before every outbound HTTP request from tools. Callers MUST connect to the
/// returned `addr` (e.g. via `reqwest::ClientBuilder::resolve(host, addr)`) rather
/// than re-resolving the host — a second DNS lookup at connect time could return a
/// different (attacker-controlled, DNS-rebound) address than the one just checked.
pub async fn ssrf_guard(raw_url: &str) -> Result<VettedAddr> {
    let url = Url::parse(raw_url).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;

    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        bail!("disallowed URL scheme '{scheme}' — only http/https are permitted");
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;

    // Block known metadata endpoints by hostname.
    for blocked in BLOCKED_HOSTS {
        if host.eq_ignore_ascii_case(blocked) {
            bail!("SSRF: blocked metadata endpoint '{host}'");
        }
    }

    let port = url.port_or_known_default().unwrap_or(80);

    // Try parsing the host as a bare IP first (fast path — no DNS).
    if let Ok(ip) = host.parse::<IpAddr>() {
        check_ip(ip, host)?;
        return Ok(VettedAddr {
            addr: SocketAddr::new(ip, port),
        });
    }

    // DNS resolution: check every resolved address, but only pin the first — that is
    // the one `reqwest`'s `.resolve()` override will actually be told to use.
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<IpAddr> = tokio::net::lookup_host(addr_str)
        .await
        .map_err(|e| anyhow::anyhow!("DNS lookup failed for '{host}': {e}"))?
        .map(|s| s.ip())
        .collect();

    if addrs.is_empty() {
        bail!("SSRF: host '{host}' resolved to no addresses");
    }

    for ip in &addrs {
        check_ip(*ip, host)?;
    }

    Ok(VettedAddr {
        addr: SocketAddr::new(addrs[0], port),
    })
}

/// V4 ranges that must never be reachable from a tool: loopback, RFC1918 private,
/// link-local (incl. the 169.254.169.254 cloud metadata endpoint), broadcast,
/// documentation, unspecified, RFC6598 CGNAT (100.64.0.0/10), class-E reserved
/// (240.0.0.0/4), and the Alibaba Cloud metadata IP.
fn ipv4_blocked(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        // CGNAT 100.64.0.0/10 (RFC 6598) — not covered by is_private()
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
        // Reserved class E 240.0.0.0/4
        || v4.octets()[0] >= 240
        // Alibaba Cloud metadata (100.100.100.200)
        || v4.octets().starts_with(&[100, 100, 100])
}

fn check_ip(ip: IpAddr, host: &str) -> Result<()> {
    let blocked = match ip {
        IpAddr::V4(v4) => ipv4_blocked(v4),
        IpAddr::V6(v6) => {
            let native = v6.is_loopback()
                || v6.is_unspecified()
                // ULA fc00::/7
                || (v6.octets()[0] & 0xfe) == 0xfc
                // Link-local fe80::/10
                || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80);
            // An IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible (`::a.b.c.d`), or
            // NAT64 (`64:ff9b::/96`) address embeds a routable V4 that the OS will
            // connect to — classify that embedded V4 with the V4 rules, or a mapped
            // metadata/loopback/private IP slips straight through the V6 branch.
            let o = v6.octets();
            let embedded = if o[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
                ipv4_blocked(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
            } else {
                v6.to_ipv4().map(ipv4_blocked).unwrap_or(false)
            };
            native || embedded
        }
    };

    if blocked {
        bail!("SSRF: request to private/reserved IP '{ip}' (host '{host}') is blocked");
    }
    Ok(())
}

/// Addresses that are NEVER allowable, even when a manifest lists them in its pinned
/// allowance (C3) — the cloud-metadata / link-local surface an SSRF is trying to reach.
/// A private RFC1918 host (e.g. an on-prem ERP at 10.x) CAN be allowed via a pinned CIDR,
/// but IMDS/link-local can NOT: allowing it would defeat the entire guard. Distinct from
/// [`ipv4_blocked`], which also blocks ordinary private ranges the allowance may re-permit.
fn is_never_allowable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // Link-local 169.254.0.0/16 (covers 169.254.169.254 IMDS)
            v4.is_link_local()
                // Loopback (metadata is never on loopback, but never-allow it anyway)
                || v4.is_loopback()
                // Alibaba Cloud metadata 100.100.100.0/24 (superset of .200)
                || v4.octets().starts_with(&[100, 100, 100])
        }
        IpAddr::V6(v6) => {
            // Canonicalize an embedded V4 (mapped/compat/NAT64) and re-check as V4 —
            // ::ffff:169.254.169.254 must be treated as the metadata IP (memory 2026-06-21).
            let o = v6.octets();
            let embedded = if o[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
                Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
            } else {
                v6.to_ipv4()
            };
            if let Some(v4) = embedded {
                return is_never_allowable(IpAddr::V4(v4));
            }
            v6.is_loopback()
                // Link-local fe80::/10
                || (o[0] == 0xfe && (o[1] & 0xc0) == 0x80)
                // AWS IPv6 IMDS fd00:ec2::254 (and its /64 metadata prefix)
                || o[..8] == [0xfd, 0x00, 0x0e, 0xc2, 0, 0, 0, 0]
        }
    }
}

/// Match `ip` against a CIDR string like `"10.0.0.0/8"` or `"93.184.216.34/32"`, or a bare
/// IP literal (treated as a /32 or /128). Returns `false` on any parse error — an
/// unparseable allowance entry can NEVER widen the allowance (fail-closed).
fn cidr_contains(cidr: &str, ip: IpAddr) -> bool {
    let (net_str, prefix) = match cidr.split_once('/') {
        Some((n, p)) => match p.parse::<u8>() {
            Ok(p) => (n, Some(p)),
            Err(_) => return false,
        },
        None => (cidr, None),
    };
    let net: IpAddr = match net_str.trim().parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    match (net, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let prefix = prefix.unwrap_or(32);
            if prefix > 32 {
                return false;
            }
            let mask = v4_mask(prefix);
            (u32::from(net) & mask) == (u32::from(ip) & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let prefix = prefix.unwrap_or(128);
            if prefix > 128 {
                return false;
            }
            let mask = v6_mask(prefix);
            (u128::from(net) & mask) == (u128::from(ip) & mask)
        }
        // Never match across families — an allowance is family-specific.
        _ => false,
    }
}

fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    }
}

fn v6_mask(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix as u32)
    }
}

/// `true` when `ip` is permitted under a per-manifest allowance: it matches at least one
/// pinned CIDR AND is not on the never-allowable metadata/link-local surface. A public IP
/// is permitted by the default guard regardless (it doesn't need to be on the allowance).
fn allowance_permits(ip: IpAddr, allowed_ip_cidrs: &[String]) -> bool {
    if is_never_allowable(ip) {
        return false;
    }
    allowed_ip_cidrs.iter().any(|c| cidr_contains(c, ip))
}

/// SSRF guard for a manifest-approved connector URL, honoring a per-manifest IP/CIDR
/// allowance (C3). A resolved address is permitted when EITHER the default [`ssrf_guard`]
/// would already permit it (a public IP) OR it matches a pinned CIDR AND is not on the
/// never-allowable metadata/link-local surface. The allowance is the single controlled
/// weakening — scoped to a human-approved base_url's PINNED IP/CIDR (not the hostname,
/// which is DNS-rebindable to IMDS), re-resolved + compared here at call time.
///
/// Callers MUST connect to the returned `addr` (`.resolve(host, addr)`) rather than
/// re-resolving — a second lookup could DNS-rebind to a different address than the one
/// vetted here. [`follow_redirects_with_guard_allowance`] re-runs THIS guard (same
/// allowance) on every redirect hop so a `302 → 169.254.169.254` is denied even though
/// the manifest host itself was allowed.
///
/// # Errors
/// Returns an error for a non-http(s) scheme, a missing host, a DNS failure, a
/// metadata/link-local target (never allowable), or a private address not on the pin.
pub async fn ssrf_guard_with_allowance(
    raw_url: &str,
    allowed_ip_cidrs: &[String],
) -> Result<VettedAddr> {
    let url = Url::parse(raw_url).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;

    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        bail!("disallowed URL scheme '{scheme}' — only http/https are permitted");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;

    // Metadata endpoints blocked by hostname regardless of any allowance.
    for blocked in BLOCKED_HOSTS {
        if host.eq_ignore_ascii_case(blocked) {
            bail!("SSRF: blocked metadata endpoint '{host}'");
        }
    }
    let port = url.port_or_known_default().unwrap_or(80);

    // Resolve to concrete addresses (IP literal fast path, else DNS).
    let addrs: Vec<IpAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else {
        tokio::net::lookup_host(format!("{host}:{port}"))
            .await
            .map_err(|e| anyhow::anyhow!("DNS lookup failed for '{host}': {e}"))?
            .map(|s| s.ip())
            .collect()
    };
    if addrs.is_empty() {
        bail!("SSRF: host '{host}' resolved to no addresses");
    }

    // Every resolved address must pass EITHER the default guard (public) OR the allowance
    // (a pinned private CIDR that is not metadata/link-local). A single failing address
    // fails the whole check — an attacker cannot slip a bad IP into a multi-A record.
    for ip in &addrs {
        if check_ip(*ip, host).is_err() && !allowance_permits(*ip, allowed_ip_cidrs) {
            bail!(
                "SSRF: '{ip}' (host '{host}') is neither public nor on the manifest's pinned \
                 allowance (or is a never-allowable metadata/link-local address)"
            );
        }
    }

    Ok(VettedAddr {
        addr: SocketAddr::new(addrs[0], port),
    })
}

/// Validate a manifest's `base_url` at APPROVAL/INSERT time: it must NOT resolve into a
/// never-allowable metadata/link-local range, EVEN IF that address is listed in the
/// allowance. A human approving a base_url that (now or via a poisoned allowance) points
/// at IMDS must be rejected outright — the allowance can re-permit an on-prem private
/// host, never the metadata surface.
///
/// # Errors
/// Returns an error if the URL is malformed, has a bad scheme/host, fails to resolve, or
/// resolves to a never-allowable address.
pub async fn validate_manifest_base_url(base_url: &str) -> Result<()> {
    let url = Url::parse(base_url).map_err(|e| anyhow::anyhow!("invalid base_url: {e}"))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        bail!("connector base_url must be http/https, got '{scheme}'");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("base_url has no host"))?;
    for blocked in BLOCKED_HOSTS {
        if host.eq_ignore_ascii_case(blocked) {
            bail!("connector base_url points at blocked metadata endpoint '{host}'");
        }
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<IpAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else {
        tokio::net::lookup_host(format!("{host}:{port}"))
            .await
            .map_err(|e| anyhow::anyhow!("base_url DNS lookup failed for '{host}': {e}"))?
            .map(|s| s.ip())
            .collect()
    };
    for ip in &addrs {
        if is_never_allowable(*ip) {
            bail!(
                "connector base_url '{base_url}' resolves to never-allowable \
                 metadata/link-local address '{ip}' — rejected at approval"
            );
        }
    }
    Ok(())
}

/// Build a per-request `reqwest::Client` pinned to `vetted.addr` for `host`, with
/// automatic redirect-following disabled.
///
/// Redirects MUST be disabled at the client level (not just "don't call
/// `.send()` again") because `reqwest`'s default policy follows up to 10 redirects
/// internally before `.send()` ever returns — a `302 → http://169.254.169.254/`
/// would reach the metadata endpoint before the caller gets a chance to inspect
/// anything. [`follow_redirects_with_guard`] re-implements following manually, one
/// hop at a time, re-running `ssrf_guard` on every `Location` header.
fn build_pinned_client(
    host: &str,
    vetted: VettedAddr,
    timeout: Duration,
) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .user_agent("Haily/1.0")
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, vetted.addr)
        .build()?)
}

/// Issues `req` (already built against a pinned, redirect-disabled client) and
/// manually follows up to [`MAX_REDIRECT_HOPS`] redirects, re-running `ssrf_guard`
/// (fresh DNS resolution + IP-range check) and re-pinning the connection on every
/// hop. This is the only safe way to follow a redirect for an SSRF-guarded fetch:
/// checking the original URL and then letting the HTTP client auto-follow blindly
/// would let a `302 → http://169.254.169.254/` (or any other blocked target) bypass
/// the guard entirely, since the guard only ever saw the first URL.
///
/// `build_request` re-creates the caller's method/headers/body against a freshly
/// pinned client for each hop (a `Client` is immutable once built — a new redirect
/// target requires a new `.resolve()` pin, hence a new `Client`); it receives the
/// hop's client and current URL and must return a request ready to `.send()`.
pub async fn follow_redirects_with_guard(
    initial_url: &str,
    timeout: Duration,
    build_request: impl Fn(&reqwest::Client, &str) -> RequestBuilder,
) -> Result<Response> {
    let mut current_url = initial_url.to_string();

    for hop in 0..=MAX_REDIRECT_HOPS {
        let vetted = ssrf_guard(&current_url).await?;
        let parsed = Url::parse(&current_url).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL has no host"))?
            .to_string();

        let client = build_pinned_client(&host, vetted, timeout)?;
        let resp = build_request(&client, &current_url).send().await?;

        if !resp.status().is_redirection() {
            return Ok(resp);
        }
        if hop == MAX_REDIRECT_HOPS {
            bail!("too many redirects (>{MAX_REDIRECT_HOPS}) for {initial_url}");
        }

        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("redirect response missing Location header"))?;

        // Location may be relative — resolve it against the current URL per RFC 7231.
        current_url = parsed
            .join(location)
            .map_err(|e| anyhow::anyhow!("invalid redirect Location '{location}': {e}"))?
            .to_string();

        tracing::warn!(hop, target = %current_url, "following redirect (re-vetting on next hop)");
    }

    unreachable!("loop always returns or bails by the last iteration")
}

/// Redirect-following variant of [`follow_redirects_with_guard`] that re-runs
/// [`ssrf_guard_with_allowance`] with the SAME per-manifest allowance on EVERY hop. This
/// is what stops a manifest host (allowed to reach an on-prem private IP) from being
/// bounced via `302 → http://169.254.169.254/` to the metadata endpoint: the metadata IP
/// is never-allowable, so the re-guard on the redirect target denies it even though the
/// original host was permitted. The allowance is passed by reference so the same slice
/// governs the initial request and every hop identically.
///
/// # Errors
/// Returns an error if any hop fails the allowance guard, the redirect chain exceeds
/// [`MAX_REDIRECT_HOPS`], or a transport error occurs.
pub async fn follow_redirects_with_guard_allowance(
    initial_url: &str,
    allowed_ip_cidrs: &[String],
    timeout: Duration,
    build_request: impl Fn(&reqwest::Client, &str) -> RequestBuilder,
) -> Result<Response> {
    let mut current_url = initial_url.to_string();

    for hop in 0..=MAX_REDIRECT_HOPS {
        let vetted = ssrf_guard_with_allowance(&current_url, allowed_ip_cidrs).await?;
        let parsed = Url::parse(&current_url).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL has no host"))?
            .to_string();

        let client = build_pinned_client(&host, vetted, timeout)?;
        let resp = build_request(&client, &current_url).send().await?;

        if !resp.status().is_redirection() {
            return Ok(resp);
        }
        if hop == MAX_REDIRECT_HOPS {
            bail!("too many redirects (>{MAX_REDIRECT_HOPS}) for {initial_url}");
        }

        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("redirect response missing Location header"))?;

        current_url = parsed
            .join(location)
            .map_err(|e| anyhow::anyhow!("invalid redirect Location '{location}': {e}"))?
            .to_string();

        tracing::warn!(hop, target = %current_url, "connector redirect (re-vetting allowance)");
    }

    unreachable!("loop always returns or bails by the last iteration")
}

/// HTTP request headers `http_request` must reject outright — each would let the
/// caller subvert something the SSRF pin or transport layer relies on:
/// - `Host` — overriding it would defeat `.resolve(host, addr)` pinning (the server
///   could be told a different virtual host than the one that was vetted).
/// - `Content-Length` / `Transfer-Encoding` — request smuggling primitives; reqwest
///   sets these correctly from the body and must remain the sole source of truth.
/// - `Connection` — could downgrade/upgrade connection semantics reqwest relies on.
/// - `Proxy-*` (e.g. `Proxy-Authorization`) — proxy-tunnel headers with no meaning
///   on a direct request; historically used to smuggle credentials to an upstream
///   proxy the caller doesn't control here.
///
/// `Authorization`/`Cookie` are deliberately NOT on this list — `http_request` is
/// permanently `RiskTier::IrreversibleWrite` (see `HttpRequestTool::risk_tier`
/// and its accompanying test), so a human has already reviewed the exact headers
/// being sent before every call.
const DENIED_HEADERS: &[&str] = &["host", "content-length", "transfer-encoding", "connection"];

/// Returns `true` if `header_name` must be rejected by `http_request` — either an
/// exact match against [`DENIED_HEADERS`] or any `Proxy-*` prefix (case-insensitive).
pub fn is_denied_header(header_name: &str) -> bool {
    let lower = header_name.to_ascii_lowercase();
    DENIED_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-")
}

/// Strip common HTML tags to produce plain text. Simple O(n) state machine — no
/// allocation overhead from a full parser. Preserves newline structure.
///
/// # Contract
/// Input is hard-capped at [`HTML_MAX_INPUT_BYTES`] (1MB) *before* parsing — a
/// pathologically large page is truncated (at a UTF-8 char boundary) rather than
/// fully scanned, bounding worst-case parse time and the `out` buffer allocation
/// regardless of how much the server actually sent.
pub fn html_to_text(html: &str) -> String {
    let html = if html.len() > HTML_MAX_INPUT_BYTES {
        let mut end = HTML_MAX_INPUT_BYTES;
        while end > 0 && !html.is_char_boundary(end) {
            end -= 1;
        }
        &html[..end]
    } else {
        html
    };

    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let mut last_was_space = false;

    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'<' => {
                in_tag = true;
                // Detect <script and <style — skip until closing tag.
                let rest = &html[i..];
                if rest.len() > 7
                    && (rest[1..7].eq_ignore_ascii_case("script")
                        || rest[1..6].eq_ignore_ascii_case("style"))
                {
                    in_script = true;
                }
            }
            b'>' => {
                if in_script {
                    // look for </script> or </style> already handled by closing tag logic
                    let behind = &html[..i + 1];
                    if behind.ends_with("</script>") || behind.ends_with("</style>") {
                        in_script = false;
                    }
                }
                in_tag = false;
                if !in_script {
                    out.push(' ');
                    last_was_space = true;
                }
            }
            _ if in_tag || in_script => {}
            b'\n' | b'\r' => {
                if !last_was_space {
                    out.push('\n');
                    last_was_space = true;
                }
            }
            _ => {
                out.push(b as char);
                last_was_space = b == b' ';
            }
        }
        i += 1;
    }

    // Collapse multiple blank lines.
    let mut result = String::with_capacity(out.len());
    let mut blank_count = 0u8;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(trimmed);
            result.push('\n');
        }
    }
    result
}

/// Reject inputs containing shell metacharacters — defense in depth for any code
/// path that might interpolate tool arguments into a shell command.
///
/// # Caller audit (2026-07, Phase 9 red team)
/// Grepped every `Command::new` in the workspace: all invocations use
/// `.args([...])` (argv passed directly to the OS, never through `/bin/sh -c` or
/// `cmd /C`), so no current code path is vulnerable to shell metacharacter
/// injection. This guard currently has NO callers. Kept (not deleted) because it is
/// the designated gate for any *future* shell-interpolating path (e.g. a
/// user-configurable shell-command tool) — deleting it would remove the one place
/// future authors are told to wire in before adding such a path. If no such path
/// exists by the next audit, delete this function instead of re-justifying it again.
pub fn shell_injection_guard(input: &str) -> Result<()> {
    const DANGEROUS: &[char] = &[
        ';', '&', '|', '`', '$', '(', ')', '{', '}', '<', '>', '!', '#', '\\',
    ];
    for ch in DANGEROUS {
        if input.contains(*ch) {
            bail!("shell injection guard: input contains forbidden character '{ch}'");
        }
    }
    Ok(())
}

/// Validate a git-reported relative path before it is joined onto a filesystem root.
///
/// Rejects absolute paths and any path containing a `..` component — git itself
/// never emits these from `diff`/`ls-files` output, so a match here indicates either
/// a compromised worktree or a bug upstream. Shared by `worktree_apply`'s execute()
/// (copies files into the main workspace) and `compute_diff` (reads untracked files
/// for the preview) so both enforce identical rules.
pub fn validate_rel_path(rel_path: &str) -> Result<()> {
    let rel = Path::new(rel_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("path traversal detected in worktree output: {rel_path}");
    }
    Ok(())
}

/// Checks whether `path` is a symlink without following it — callers should skip
/// (not read through) any path where this returns `Ok(true)`, since following a
/// symlink could read or write outside the intended root.
pub async fn is_symlink(path: &PathBuf) -> Result<bool> {
    let meta = tokio::fs::symlink_metadata(path).await?;
    Ok(meta.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // ---- ssrf_guard / check_ip -------------------------------------------------

    #[tokio::test]
    async fn ssrf_guard_blocks_loopback_ipv4() {
        let result = ssrf_guard("http://127.0.0.1/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_private_10_range() {
        let result = ssrf_guard("http://10.0.0.5/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_private_192_168_range() {
        let result = ssrf_guard("http://192.168.1.1/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_private_172_16_range() {
        let result = ssrf_guard("http://172.16.0.1/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_link_local_169_254() {
        let result = ssrf_guard("http://169.254.1.2/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_cloud_metadata_ip_literal() {
        let result = ssrf_guard("http://169.254.169.254/latest/meta-data/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_cloud_metadata_hostname() {
        let result = ssrf_guard("http://metadata.google.internal/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_ipv6_loopback() {
        let result = ssrf_guard("http://[::1]/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_ipv6_ula_fc00() {
        let result = ssrf_guard("http://[fc00::1]/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_ipv4_mapped_metadata() {
        // ::ffff:169.254.169.254 — an IPv4-mapped V6 literal that the OS routes to the
        // V4 metadata endpoint. Must be decoded and blocked, not vetted as a safe V6.
        assert!(
            ssrf_guard("http://[::ffff:169.254.169.254]/latest/meta-data/")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_ipv4_mapped_loopback_and_private() {
        assert!(ssrf_guard("http://[::ffff:127.0.0.1]/").await.is_err());
        assert!(ssrf_guard("http://[::ffff:10.0.0.1]/").await.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_cgnat_and_reserved_v4() {
        assert!(ssrf_guard("http://100.64.0.1/").await.is_err()); // CGNAT 100.64/10
        assert!(ssrf_guard("http://240.0.0.1/").await.is_err()); // reserved 240/4
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_ipv6_ula_fd00() {
        // fd00::/8 is the commonly-assigned half of the fc00::/7 ULA block.
        let result = ssrf_guard("http://[fd12:3456::1]/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_blocks_ipv6_link_local() {
        let result = ssrf_guard("http://[fe80::1]/").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_non_http_scheme() {
        let result = ssrf_guard("file:///etc/passwd").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_malformed_url() {
        let result = ssrf_guard("not a url").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_guard_allows_public_ip_literal_and_pins_addr() {
        // 93.184.216.34 was example.com's long-standing public IP; used here purely
        // as a syntactically valid public-range literal (no network call is made
        // for the IP-literal fast path).
        let vetted = ssrf_guard("http://93.184.216.34:8080/").await.unwrap();
        assert_eq!(
            vetted.addr,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 8080)
        );
    }

    #[test]
    fn check_ip_blocks_alibaba_metadata_range() {
        let ip = IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200));
        assert!(check_ip(ip, "100.100.100.200").is_err());
    }

    // ---- ssrf_guard_with_allowance (C3) -----------------------------------------

    #[tokio::test]
    async fn allowance_permits_pinned_private_cidr() {
        // A private RFC1918 host is blocked by the default guard but PERMITTED when it
        // matches a pinned CIDR (an on-prem ERP the human approved).
        let allow = vec!["10.0.0.0/8".to_string()];
        let vetted = ssrf_guard_with_allowance("http://10.20.30.40:8069/", &allow)
            .await
            .expect("pinned private CIDR must be permitted");
        assert_eq!(
            vetted.addr,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40)), 8069)
        );
    }

    #[tokio::test]
    async fn allowance_blocks_same_ip_when_not_pinned() {
        // The SAME private IP resolving from a host NOT on the allowance stays blocked —
        // the allowance is scoped to the pinned CIDR, not to the IP universally.
        let allow = vec!["192.168.1.0/24".to_string()]; // different subnet
        let res = ssrf_guard_with_allowance("http://10.20.30.40:8069/", &allow).await;
        assert!(
            res.is_err(),
            "a private IP off the pinned allowance must stay blocked"
        );
        // And with an EMPTY allowance the private IP is blocked exactly like the default.
        let res2 = ssrf_guard_with_allowance("http://10.20.30.40/", &[]).await;
        assert!(res2.is_err());
    }

    #[tokio::test]
    async fn allowance_permits_public_ip_unaffected() {
        // A public IP is permitted with or without any allowance — the default guard
        // already lets it through; the allowance path must not change that.
        let vetted = ssrf_guard_with_allowance("http://93.184.216.34/", &[])
            .await
            .expect("public IP unaffected by allowance");
        assert_eq!(
            vetted.addr,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 80)
        );
    }

    #[tokio::test]
    async fn allowance_never_permits_metadata_even_if_listed() {
        // 169.254.169.254 is on the never-allowable surface — listing it in the allowance
        // must NOT permit it. This is the core C3 invariant: the allowance re-permits
        // private hosts, NEVER the metadata/link-local surface.
        let allow = vec!["169.254.0.0/16".to_string()];
        let res =
            ssrf_guard_with_allowance("http://169.254.169.254/latest/meta-data/", &allow).await;
        assert!(res.is_err(), "metadata IP is never allowable even if listed");

        // The IPv4-mapped V6 form must also be denied (canonicalization preserved).
        let res2 =
            ssrf_guard_with_allowance("http://[::ffff:169.254.169.254]/", &allow).await;
        assert!(res2.is_err(), "mapped metadata V6 never allowable");

        // Alibaba metadata /24 too.
        let allow_ali = vec!["100.100.100.0/24".to_string()];
        assert!(
            ssrf_guard_with_allowance("http://100.100.100.200/", &allow_ali)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn validate_base_url_rejects_metadata_at_approval() {
        // A base_url resolving to 169.254.169.254 is REJECTED at approval time, even if a
        // (poisoned) allowance would list it.
        assert!(
            validate_manifest_base_url("http://169.254.169.254/")
                .await
                .is_err()
        );
        // A public base_url passes approval.
        assert!(
            validate_manifest_base_url("https://93.184.216.34/")
                .await
                .is_ok()
        );
        // A private base_url passes approval (the allowance decides call-time access; the
        // approval check only rejects the never-allowable metadata surface).
        assert!(validate_manifest_base_url("http://10.20.30.40/").await.is_ok());
        // Non-http scheme rejected.
        assert!(validate_manifest_base_url("file:///etc/passwd").await.is_err());
    }

    #[test]
    fn cidr_contains_matches_and_rejects() {
        let ip = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40));
        assert!(cidr_contains("10.0.0.0/8", ip));
        assert!(cidr_contains("10.20.30.40/32", ip));
        assert!(!cidr_contains("10.20.31.0/24", ip));
        assert!(!cidr_contains("192.168.0.0/16", ip));
        // Bare IP literal is treated as an exact match.
        assert!(cidr_contains("10.20.30.40", ip));
        // Malformed entries can never widen the allowance (fail-closed).
        assert!(!cidr_contains("not-a-cidr", ip));
        assert!(!cidr_contains("10.0.0.0/99", ip));
        // Cross-family never matches.
        let v6 = IpAddr::V6("::1".parse().unwrap());
        assert!(!cidr_contains("10.0.0.0/8", v6));
    }

    #[test]
    fn is_never_allowable_covers_metadata_surface() {
        assert!(is_never_allowable("169.254.169.254".parse().unwrap()));
        assert!(is_never_allowable("169.254.1.1".parse().unwrap())); // link-local /16
        assert!(is_never_allowable("100.100.100.200".parse().unwrap())); // alibaba
        assert!(is_never_allowable("127.0.0.1".parse().unwrap())); // loopback
        assert!(is_never_allowable("fe80::1".parse().unwrap())); // v6 link-local
        assert!(is_never_allowable("::ffff:169.254.169.254".parse().unwrap()));
        // An ordinary private host is NOT never-allowable — the allowance may permit it.
        assert!(!is_never_allowable("10.20.30.40".parse().unwrap()));
        assert!(!is_never_allowable("192.168.1.1".parse().unwrap()));
        // A public IP is not never-allowable.
        assert!(!is_never_allowable("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn check_ip_allows_public_ipv4() {
        let ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        assert!(check_ip(ip, "8.8.8.8").is_ok());
    }

    // ---- shell_injection_guard --------------------------------------------------

    #[test]
    fn shell_injection_guard_allows_plain_text() {
        assert!(shell_injection_guard("hello world 123").is_ok());
    }

    #[test]
    fn shell_injection_guard_rejects_semicolon_chaining() {
        assert!(shell_injection_guard("ls; rm -rf /").is_err());
    }

    #[test]
    fn shell_injection_guard_rejects_pipe() {
        assert!(shell_injection_guard("cat file | sh").is_err());
    }

    #[test]
    fn shell_injection_guard_rejects_backtick_substitution() {
        assert!(shell_injection_guard("echo `whoami`").is_err());
    }

    #[test]
    fn shell_injection_guard_rejects_dollar_paren_substitution() {
        assert!(shell_injection_guard("echo $(whoami)").is_err());
    }

    #[test]
    fn shell_injection_guard_rejects_redirect_operators() {
        assert!(shell_injection_guard("cmd > /etc/passwd").is_err());
        assert!(shell_injection_guard("cmd < secret.txt").is_err());
    }

    #[test]
    fn shell_injection_guard_rejects_ampersand_backgrounding() {
        assert!(shell_injection_guard("sleep 10 &").is_err());
    }

    // ---- html_to_text ------------------------------------------------------------

    #[test]
    fn html_to_text_strips_basic_tags() {
        let html = "<p>Hello <b>world</b></p>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn html_to_text_strips_script_and_style_content() {
        let html = "<style>.a{color:red}</style><p>visible</p><script>alert(1)</script>";
        let text = html_to_text(html);
        assert!(text.contains("visible"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn html_to_text_collapses_multiple_blank_lines() {
        let html = "<p>a</p>\n\n\n\n<p>b</p>";
        let text = html_to_text(html);
        // No run of more than one consecutive blank line should survive.
        assert!(!text.contains("\n\n\n"));
    }

    #[test]
    fn html_to_text_handles_empty_input() {
        assert_eq!(html_to_text(""), "");
    }

    #[test]
    fn html_to_text_caps_oversized_input_without_panicking() {
        // 2MB of input — well past HTML_MAX_INPUT_BYTES — must not panic and must
        // produce output bounded by the cap rather than the full input length.
        let huge = "a".repeat(HTML_MAX_INPUT_BYTES * 2);
        let text = html_to_text(&huge);
        assert!(text.len() <= HTML_MAX_INPUT_BYTES + 1);
    }

    #[test]
    fn html_to_text_cap_respects_utf8_char_boundaries() {
        // Multi-byte UTF-8 chars ('à' = 2 bytes) placed right at the cap boundary
        // must not cause a panic from slicing mid-character.
        let mut huge = "a".repeat(HTML_MAX_INPUT_BYTES - 1);
        huge.push('à');
        huge.push_str(&"b".repeat(1000));
        let text = html_to_text(&huge); // must not panic
        assert!(!text.is_empty());
    }

    // ---- validate_rel_path --------------------------------------------------------

    #[test]
    fn validate_rel_path_accepts_normal_relative_path() {
        assert!(validate_rel_path("src/main.rs").is_ok());
    }

    #[test]
    fn validate_rel_path_rejects_parent_dir_escape() {
        assert!(validate_rel_path("../escape.txt").is_err());
        assert!(validate_rel_path("a/../../escape.txt").is_err());
    }

    #[test]
    fn validate_rel_path_rejects_absolute_path() {
        #[cfg(unix)]
        assert!(validate_rel_path("/etc/passwd").is_err());
        #[cfg(windows)]
        assert!(validate_rel_path("C:\\Windows\\System32").is_err());
    }

    // ---- is_denied_header ---------------------------------------------------------

    #[test]
    fn is_denied_header_rejects_host_case_insensitively() {
        assert!(is_denied_header("Host"));
        assert!(is_denied_header("host"));
        assert!(is_denied_header("HOST"));
    }

    #[test]
    fn is_denied_header_rejects_smuggling_primitives() {
        assert!(is_denied_header("Content-Length"));
        assert!(is_denied_header("Transfer-Encoding"));
        assert!(is_denied_header("Connection"));
    }

    #[test]
    fn is_denied_header_rejects_any_proxy_prefixed_header() {
        assert!(is_denied_header("Proxy-Authorization"));
        assert!(is_denied_header("proxy-connection"));
    }

    #[test]
    fn is_denied_header_allows_authorization_and_cookie() {
        // Permitted only because http_request is permanently RequireApproval — see
        // the doc comment on DENIED_HEADERS and the paired approval-class test in
        // v1::web::tests.
        assert!(!is_denied_header("Authorization"));
        assert!(!is_denied_header("Cookie"));
    }

    #[test]
    fn is_denied_header_allows_ordinary_headers() {
        assert!(!is_denied_header("Accept"));
        assert!(!is_denied_header("X-Custom-Header"));
    }
}
