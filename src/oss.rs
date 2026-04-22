use std::{fmt, path::Path, time::Duration};

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client, config::Region, presigning::PresigningConfig, primitives::ByteStream};
use sha2::{Digest, Sha256};

use crate::{
    config::TosConfig,
    error::{Error, Result},
};

/// Thin wrapper around `aws-sdk-s3` configured for Volcano Engine TOS
/// (S3-compatible endpoint). Also handles object-key construction and
/// signed-download-URL presigning.
#[derive(Clone)]
pub struct OssClient {
    client: Client,
    bucket: String,
    key_prefix: String,
    default_expires: Duration,
    max_expires: Duration,
    upload_timeout: Duration,
}

impl fmt::Debug for OssClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OssClient")
            .field("bucket", &self.bucket)
            .field("key_prefix", &self.key_prefix)
            .field("default_expires", &self.default_expires)
            .finish_non_exhaustive()
    }
}

impl OssClient {
    pub fn connect(cfg: &TosConfig) -> Result<Self> {
        if cfg.access_key.is_empty() || cfg.secret_key.is_empty() {
            return Err(Error::Other(
                "tos.access_key and tos.secret_key must not be empty".into(),
            ));
        }
        let creds = Credentials::new(
            cfg.access_key.clone(),
            cfg.secret_key.clone(),
            None,
            None,
            "scriptorium",
        );
        let s3_cfg = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .endpoint_url(&cfg.endpoint)
            .credentials_provider(creds)
            .force_path_style(false)
            .build();
        let client = Client::from_conf(s3_cfg);
        Ok(Self {
            client,
            bucket: cfg.bucket.clone(),
            key_prefix: cfg.key_prefix.clone(),
            default_expires: Duration::from_secs(u64::from(cfg.signed_url_expires_seconds)),
            max_expires: Duration::from_secs(u64::from(cfg.signed_url_max_seconds)),
            upload_timeout: Duration::from_secs(cfg.upload_timeout_seconds),
        })
    }

    pub fn default_expires(&self) -> Duration {
        self.default_expires
    }

    pub fn max_expires(&self) -> Duration {
        self.max_expires
    }

    /// Build the canonical object key for a scriptorium artifact.
    ///
    /// Layout: `{key_prefix}{tenant}/{workspace}/{yyyy-mm-dd}/{nonce}-{basename}`
    ///
    /// Segments are sanitized to `[A-Za-z0-9._-]`. The nonce is an 8-hex-digit
    /// slice of a nanosecond counter — good enough uniqueness for sequential
    /// uploads without pulling in a RNG crate.
    pub fn build_key(&self, tenant_id: &str, workspace_id: &str, basename: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let ymd = ymd_from_unix(now.as_secs());
        let nonce = format!("{:08x}", now.as_nanos() as u64 & 0xffff_ffff);
        format!(
            "{prefix}{tenant}/{ws}/{ymd}/{nonce}-{base}",
            prefix = self.key_prefix,
            tenant = sanitize_segment(tenant_id),
            ws = sanitize_segment(workspace_id),
            base = sanitize_segment(basename),
        )
    }

    /// Upload a file from disk to the given object key and return the
    /// sha256 + size + effective content-type.
    pub async fn upload_file(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
        label: Option<&str>,
    ) -> Result<UploadOutcome> {
        // Read full file into memory; sufficient for workspace artifacts
        // bounded by fetch.max_body_bytes (default 1 GiB). Move to the
        // aws-sdk transfer manager if we ever need >1 GiB uploads.
        let data = tokio::fs::read(path).await?;
        let size_bytes = data.len() as u64;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let sha256_hex = hex::encode(hasher.finalize());

        let body = ByteStream::from(data);
        let mut put = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .content_type(content_type);
        if let Some(l) = label {
            put = put.metadata("label", l);
        }

        tokio::time::timeout(self.upload_timeout, put.send())
            .await
            .map_err(|_| {
                Error::Other(format!(
                    "upload timed out after {}s",
                    self.upload_timeout.as_secs()
                ))
            })?
            .map_err(|e| Error::Other(format!("put_object: {e}")))?;

        Ok(UploadOutcome {
            key: key.to_string(),
            size_bytes,
            content_type: content_type.to_string(),
            sha256_hex,
        })
    }

    /// Presign a GET URL for the given key.
    pub async fn signed_url(&self, key: &str, ttl: Duration) -> Result<String> {
        let effective = ttl.min(self.max_expires);
        let psc = PresigningConfig::expires_in(effective)
            .map_err(|e| Error::Other(format!("presigning config: {e}")))?;
        let presigned = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(psc)
            .await
            .map_err(|e| Error::Other(format!("presign: {e}")))?;
        Ok(presigned.uri().to_string())
    }
}

#[derive(Debug, Clone)]
pub struct UploadOutcome {
    pub key: String,
    pub size_bytes: u64,
    pub content_type: String,
    pub sha256_hex: String,
}

/// Guess an HTTP content-type from a path extension. Falls back to
/// `application/octet-stream` for anything unrecognized — clients that care
/// about a specific type for unknown extensions should pass it explicitly.
pub fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "application/javascript",
        Some("csv") => "text/csv; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("xml") => "application/xml",
        Some("yaml" | "yml") => "application/yaml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// Convert a Unix timestamp (seconds) to a `yyyy-mm-dd` UTC string without
/// pulling in `chrono` / `time`. Uses Howard Hinnant's civil-from-days
/// algorithm — correct for any Gregorian date since epoch.
fn ymd_from_unix(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn sanitize_segment(s: &str) -> String {
    if s.is_empty() {
        return "_".into();
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ymd_is_iso_formatted() {
        // 2026-04-22 UTC ≈ 1 777 708 800
        let s = ymd_from_unix(1_777_708_800);
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }

    #[test]
    fn sanitize_segment_keeps_safe_chars() {
        assert_eq!(sanitize_segment("abc-123_xyz.txt"), "abc-123_xyz.txt");
        assert_eq!(sanitize_segment("a/b/../c"), "a_b_.._c");
        assert_eq!(sanitize_segment(""), "_");
    }
}
