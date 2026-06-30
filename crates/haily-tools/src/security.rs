/// SSRF and injection guards for all outbound/tool activity.
use anyhow::{bail, Result};
use std::net::IpAddr;
use url::Url;

// Known cloud metadata endpoints to block regardless of IP classification.
const BLOCKED_HOSTS: &[&str] = &[
    "169.254.169.254",       // AWS/GCP/Azure IMDS
    "metadata.google.internal",
    "100.100.100.200",       // Alibaba Cloud metadata
    "fd00:ec2::254",         // AWS IPv6 IMDS
];

/// Block requests to private networks, loopback, link-local, and cloud metadata.
///
/// Call before every outbound HTTP request from tools.
pub async fn ssrf_guard(raw_url: &str) -> Result<()> {
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

    // Try parsing the host as a bare IP first (fast path — no DNS).
    if let Ok(ip) = host.parse::<IpAddr>() {
        check_ip(ip, host)?;
        return Ok(());
    }

    // DNS resolution: check every resolved address.
    let port = url.port_or_known_default().unwrap_or(80);
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<IpAddr> = tokio::net::lookup_host(addr_str)
        .await
        .map_err(|e| anyhow::anyhow!("DNS lookup failed for '{host}': {e}"))?
        .map(|s| s.ip())
        .collect();

    if addrs.is_empty() {
        bail!("SSRF: host '{host}' resolved to no addresses");
    }

    for ip in addrs {
        check_ip(ip, host)?;
    }
    Ok(())
}

fn check_ip(ip: IpAddr, host: &str) -> Result<()> {
    let blocked = match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // Alibaba Cloud metadata (100.100.100.200)
                || v4.octets().starts_with(&[100, 100, 100])
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // ULA fc00::/7
                || (v6.octets()[0] & 0xfe) == 0xfc
                // Link-local fe80::/10
                || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80)
        }
    };

    if blocked {
        bail!("SSRF: request to private/reserved IP '{ip}' (host '{host}') is blocked");
    }
    Ok(())
}

/// Strip common HTML tags to produce plain text. Simple O(n) state machine — no allocation
/// overhead from a full parser. Preserves newline structure.
pub fn html_to_text(html: &str) -> String {
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
