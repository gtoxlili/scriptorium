use std::{fmt, path::Path, time::Duration};

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client,
    config::Region,
    presigning::PresigningConfig,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

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
    part_size: u64,
    multipart_threshold: u64,
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
            part_size: cfg.part_size_bytes.max(5 * 1024 * 1024),
            multipart_threshold: cfg.multipart_threshold_bytes,
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
    ///
    /// Small files (≤ `multipart_threshold`) take a single-shot
    /// `put_object` — that path loads the whole body into RAM, which is
    /// fine for a few dozen MiB. Larger files go through streaming
    /// multipart upload so peak memory stays bounded by `part_size`
    /// regardless of file size.
    pub async fn upload_file(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
        label: Option<&str>,
    ) -> Result<UploadOutcome> {
        let size_bytes = tokio::fs::metadata(path).await?.len();

        let outcome_fut = async {
            if size_bytes > self.multipart_threshold {
                self.upload_file_multipart(key, path, content_type, label, size_bytes)
                    .await
            } else {
                self.upload_file_single(key, path, content_type, label, size_bytes)
                    .await
            }
        };

        tokio::time::timeout(self.upload_timeout, outcome_fut)
            .await
            .map_err(|_| {
                Error::Other(format!(
                    "upload timed out after {}s",
                    self.upload_timeout.as_secs()
                ))
            })?
    }

    /// Single-shot `put_object`. Body is fully materialised in memory —
    /// only call this for files comfortably smaller than what the host
    /// can afford to buffer.
    async fn upload_file_single(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
        label: Option<&str>,
        size_bytes: u64,
    ) -> Result<UploadOutcome> {
        let data = tokio::fs::read(path).await?;
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
        put.send()
            .await
            .map_err(|e| Error::Other(format!("put_object: {e}")))?;

        Ok(UploadOutcome {
            key: key.to_string(),
            size_bytes,
            content_type: content_type.to_string(),
            sha256_hex,
        })
    }

    /// Multipart upload. Reads `path` sequentially in `part_size` chunks
    /// (never more than one part in memory at a time), hashes as it goes
    /// so the final sha256 matches what TOS stored, and aborts the
    /// in-progress upload on any error to avoid stranding parts that
    /// would otherwise be billed as incomplete storage.
    async fn upload_file_multipart(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
        label: Option<&str>,
        size_bytes: u64,
    ) -> Result<UploadOutcome> {
        // 1. Open the multipart upload.
        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type);
        if let Some(l) = label {
            create = create.metadata("label", l);
        }
        let created = create
            .send()
            .await
            .map_err(|e| Error::Other(format!("create_multipart_upload: {e}")))?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| Error::Other("create_multipart_upload: missing upload_id".into()))?
            .to_string();

        // 2. Stream parts. Wrap the whole loop so that on ANY error we can
        //    abort the upload before bubbling up.
        let part_size = usize::try_from(self.part_size).unwrap_or(usize::MAX);
        let upload_result = self
            .stream_parts(key, path, &upload_id, part_size)
            .await;

        let (completed_parts, sha256_hex) = match upload_result {
            Ok(v) => v,
            Err(e) => {
                // Best-effort abort; ignore the response because we're
                // already returning the original error.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                return Err(e);
            }
        };

        // 3. Finalise. S3 requires parts sorted by part_number — we emit
        //    them sequentially so they're already in order.
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| {
                // Complete can fail after all parts uploaded OK (e.g. network
                // blip on the final RPC). Abort to keep the bucket tidy.
                let client = self.client.clone();
                let bucket = self.bucket.clone();
                let key = key.to_string();
                let upload_id = upload_id.clone();
                tokio::spawn(async move {
                    let _ = client
                        .abort_multipart_upload()
                        .bucket(bucket)
                        .key(key)
                        .upload_id(upload_id)
                        .send()
                        .await;
                });
                Error::Other(format!("complete_multipart_upload: {e}"))
            })?;

        Ok(UploadOutcome {
            key: key.to_string(),
            size_bytes,
            content_type: content_type.to_string(),
            sha256_hex,
        })
    }

    async fn stream_parts(
        &self,
        key: &str,
        path: &Path,
        upload_id: &str,
        part_size: usize,
    ) -> Result<(Vec<CompletedPart>, String)> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut completed_parts: Vec<CompletedPart> = Vec::new();
        let mut part_number: i32 = 1;
        let mut buf = vec![0u8; part_size];

        loop {
            // Fill the buffer (may span multiple `read` calls for large
            // parts on slow filesystems).
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = file.read(&mut buf[filled..]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                // Exactly at EOF on a part boundary — part_number-1 was
                // the last part, nothing more to send.
                break;
            }

            let chunk = buf[..filled].to_vec();
            hasher.update(&chunk);

            let part = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(chunk))
                .send()
                .await
                .map_err(|e| Error::Other(format!("upload_part({part_number}): {e}")))?;

            completed_parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(part.e_tag().map(String::from))
                    .build(),
            );

            part_number += 1;
            if filled < buf.len() {
                break; // Short read = last part, no need for another iteration.
            }
        }

        Ok((completed_parts, hex::encode(hasher.finalize())))
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
