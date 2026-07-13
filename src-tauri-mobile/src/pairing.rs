//! `POST /pair` — plain HTTP request/response, deliberately NOT a WS frame (see
//! `docs/mobile-protocol.md` §4), redeemed EXACTLY once per scan/manual-entry with the
//! desktop's out-of-band confirm gating token issuance (M4). Reuses the identical pinned-
//! fingerprint TLS verifier the WS client uses (`haily_mobile_client::pinned_client_config`) —
//! the loopback dev loop (`adb reverse`, M12) talks plain `http://`, which never touches TLS at
//! all, so one client handles both cases correctly with no special-casing here.
use anyhow::{bail, Context, Result};
use haily_types::{MobileError, PairRequest, PairResponse, PairingQr};

fn base_url(qr: &PairingQr) -> String {
    // `bind::requires_tls` (P2a) ties TLS to whether the bound address is loopback — the QR
    // itself doesn't carry a scheme, so this mirrors that same rule: loopback is the ONLY
    // plain-http case (the `adb reverse` dev loop), everything else is `https://` with the
    // pinned cert.
    let scheme = if qr.host == "127.0.0.1" || qr.host == "localhost" || qr.host == "::1" {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{}:{}", qr.host, qr.port)
}

fn build_client(qr: &PairingQr) -> Result<reqwest::Client> {
    let tls_config = haily_mobile_client::pinned_client_config(qr.cert_fingerprint.clone());
    reqwest::Client::builder()
        .use_preconfigured_tls((*tls_config).clone())
        .build()
        .context("building the pinned-TLS pairing HTTP client")
}

/// Redeems `qr`'s pairing code, returning the desktop-issued device token. Blocks (server-side)
/// until the desktop user approves the out-of-band confirm prompt (M4) or the code expires —
/// callers should expect this to take up to the code's TTL, not return immediately.
pub async fn redeem(qr: &PairingQr, device_name: &str) -> Result<PairResponse> {
    let client = build_client(qr)?;
    let url = format!("{}/pair", base_url(qr));
    let response = client
        .post(&url)
        .json(&PairRequest {
            pairing_code: qr.pairing_code.clone(),
            device_name: device_name.to_string(),
        })
        .send()
        .await
        .context("sending POST /pair")?;

    if response.status().is_success() {
        return response
            .json::<PairResponse>()
            .await
            .context("parsing PairResponse");
    }

    let status = response.status();
    match response.json::<MobileError>().await {
        Ok(err) => bail!("pairing rejected ({status}): {err:?}"),
        Err(_) => bail!("pairing rejected with status {status} (unparseable error body)"),
    }
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
    fn loopback_hosts_use_plain_http() {
        assert!(base_url(&qr("127.0.0.1")).starts_with("http://"));
        assert!(base_url(&qr("localhost")).starts_with("http://"));
    }

    #[test]
    fn non_loopback_hosts_use_pinned_https() {
        assert!(base_url(&qr("haily.tailnet.ts.net")).starts_with("https://"));
        assert!(base_url(&qr("192.168.1.50")).starts_with("https://"));
    }

    #[tokio::test]
    async fn redeem_against_an_unreachable_host_surfaces_an_error_not_a_panic() {
        let result = redeem(&qr("127.0.0.1"), "test-phone").await;
        assert!(result.is_err());
    }
}
