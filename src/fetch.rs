use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::Path,
    time::Duration,
};

use futures::StreamExt;
use reqwest::{Client, redirect};
use tokio::{fs, io::AsyncWriteExt};

use crate::{
    config::FetchConfig,
    error::{Error, Result},
};

#[derive(Debug)]
pub struct FetchOutcome {
    pub bytes_written: u64,
    pub content_type: String,
    pub http_status: u16,
}

/// Download an HTTP(S) URL into a workspace path. Applies SSRF defences
/// unless `cfg.allow_private_network` is on, honours `cfg.max_body_bytes`,
/// and caps the whole operation with `timeout`.
#[allow(clippy::implicit_hasher)] // `headers` comes straight from prost (default hasher).
pub async fn fetch_to_file(
    cfg: &FetchConfig,
    url: &str,
    target: &Path,
    headers: &HashMap<String, String>,
    timeout: Duration,
) -> Result<FetchOutcome> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Error::Other(format!("bad url: {e}")))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::Other(format!(
                "unsupported url scheme: {other} — only http/https allowed"
            )));
        }
    }

    if !cfg.allow_private_network {
        assert_public_host(&parsed).await?;
    }

    let client = Client::builder()
        .timeout(timeout)
        .redirect(redirect::Policy::limited(5))
        .build()
        .map_err(|e| Error::Other(format!("http client build: {e}")))?;

    let mut request = client.get(parsed);
    for (k, v) in headers {
        request = request.header(k, v);
    }

    let resp = request
        .send()
        .await
        .map_err(|e| Error::Other(format!("fetch: {e}")))?;

    let http_status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "fetch returned non-2xx status: {http_status}"
        )));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).await?;
    }
    // Match put_file semantics: delete any pre-existing file so a container
    // UID that previously created it doesn't block the server (running as
    // a different UID) from rewriting.
    let _ = fs::remove_file(target).await;
    let mut file = fs::File::create(target).await?;

    let mut bytes_written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Other(format!("fetch body: {e}")))?;
        bytes_written += chunk.len() as u64;
        if bytes_written > cfg.max_body_bytes {
            return Err(Error::Other(format!(
                "fetch body exceeded max_body_bytes ({bytes_written} > {})",
                cfg.max_body_bytes
            )));
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;

    Ok(FetchOutcome {
        bytes_written,
        content_type,
        http_status,
    })
}

async fn assert_public_host(url: &reqwest::Url) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| Error::Other("url has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(80);

    // If the URL's host is already an IP literal, check directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_disallowed(&ip) {
            return Err(Error::Other(format!(
                "refusing to fetch disallowed literal address: {host}"
            )));
        }
        return Ok(());
    }

    // Otherwise resolve DNS and ensure every record is public-routable.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{host}:{port}"))
        .await
        .map_err(|e| Error::Other(format!("dns resolve {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::Other(format!("dns: no records for {host}")));
    }
    for sa in &addrs {
        if is_disallowed(&sa.ip()) {
            return Err(Error::Other(format!(
                "refusing to fetch {host}: resolved to disallowed address {}",
                sa.ip()
            )));
        }
    }
    Ok(())
}

fn is_disallowed(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // RFC 6598 CGNAT range 100.64.0.0/10
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
                // "this network" 0.0.0.0/8
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // ULA fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn rejects_ipv4_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.5.5",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
        ] {
            let v4: Ipv4Addr = ip.parse().unwrap();
            assert!(is_disallowed(&IpAddr::V4(v4)), "should reject {ip}");
        }
    }

    #[test]
    fn allows_public_ipv4() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            let v4: Ipv4Addr = ip.parse().unwrap();
            assert!(!is_disallowed(&IpAddr::V4(v4)), "should allow {ip}");
        }
    }

    #[test]
    fn rejects_ipv6_loopback_and_ula() {
        let cases: &[Ipv6Addr] = &[
            "::1".parse().unwrap(),
            "fc00::1".parse().unwrap(),
            "fd12:3456:789a::1".parse().unwrap(),
            "fe80::1".parse().unwrap(),
        ];
        for ip in cases {
            assert!(is_disallowed(&IpAddr::V6(*ip)), "should reject {ip}");
        }
    }
}
