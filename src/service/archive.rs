//! Archive + workspace-transfer helpers.
//!
//! Extracted from `service.rs` because the RPC handlers themselves should
//! be readable without having to scroll past hundreds of lines of tar.gz
//! and directory-replace plumbing. Nothing here is specific to any one
//! RPC; `upload_to_oss`, `import_workspace_object`, and
//! `export_workspace_object` all share this code.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::AsyncReadExt,
};

use crate::error::{Error, Result};

pub(super) const WORKSPACE_TRANSFER_CHUNK_SIZE: usize = 64 * 1024;

/// RAII guard that deletes a temporary file when dropped. Used to clean
/// up the tar.gz staging file produced for directory / compressed uploads
/// so an error on the upload path doesn't leak a stray archive.
pub(super) struct TempFileGuard(pub PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Build a collision-free sibling of `target` for staging an in-flight
/// transfer. Lives inside the workspace so `rename` stays on the same
/// filesystem (atomic on Linux, fast on macOS).
pub(super) fn build_workspace_transfer_temp_path(target: &Path, suffix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u64, |d| d.as_nanos() as u64);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".scriptorium-{suffix}-{nonce:x}.tmp"))
}

pub(super) async fn replace_workspace_file_from_staging(
    staging: PathBuf,
    target: PathBuf,
) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).await?;
    }
    remove_path_if_exists(&target).await?;
    fs::rename(staging, target).await?;
    Ok(())
}

pub(super) async fn remove_path_if_exists(target: &Path) -> Result<()> {
    match fs::metadata(target).await {
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(target).await?;
        }
        Ok(_) => {
            fs::remove_file(target).await?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub(super) async fn replace_workspace_directory_from_archive_path(
    staging: PathBuf,
    target: PathBuf,
) -> Result<()> {
    let target_parent = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&target_parent).await?;
    let extraction_root = build_workspace_transfer_temp_path(&target, "expand");
    let target_clone = target.clone();
    tokio::task::spawn_blocking(move || {
        extract_archive_and_replace(staging, extraction_root, target_clone)
    })
    .await
    .map_err(|e| Error::Other(format!("extract archive join: {e}")))??;
    Ok(())
}

pub(super) async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; WORKSPACE_TRANSFER_CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn extract_archive_and_replace(
    staging: PathBuf,
    extraction_root: PathBuf,
    target: PathBuf,
) -> Result<()> {
    if extraction_root.exists() {
        std::fs::remove_dir_all(&extraction_root)?;
    }
    std::fs::create_dir_all(&extraction_root)?;

    let file = std::fs::File::open(&staging)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.set_overwrite(true);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let rel = entry.path()?;
        let clean = rel.as_ref();
        if clean.is_absolute()
            || clean
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::Other(format!(
                "archive entry escapes target directory: {}",
                rel.display()
            )));
        }
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Directory => {}
            _ => {
                return Err(Error::Other(format!(
                    "unsupported archive entry type: {}",
                    rel.display()
                )));
            }
        }
        entry.unpack_in(&extraction_root)?;
    }

    remove_path_if_exists_blocking(&target)?;

    let mut entries =
        std::fs::read_dir(&extraction_root)?.collect::<std::result::Result<Vec<_>, _>>()?;
    if entries.len() == 1 {
        let source = entries.remove(0).path();
        if source.is_dir() {
            std::fs::rename(source, &target)?;
            let _ = std::fs::remove_dir_all(&extraction_root);
            let _ = std::fs::remove_file(&staging);
            return Ok(());
        }
    }

    std::fs::create_dir_all(&target)?;
    for entry in std::fs::read_dir(&extraction_root)? {
        let entry = entry?;
        std::fs::rename(entry.path(), target.join(entry.file_name()))?;
    }
    let _ = std::fs::remove_dir_all(&extraction_root);
    let _ = std::fs::remove_file(&staging);
    Ok(())
}

fn remove_path_if_exists_blocking(target: &Path) -> Result<()> {
    match std::fs::metadata(target) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(target)?,
        Ok(_) => std::fs::remove_file(target)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Produce a tar.gz of `source` into a unique file under the OS temp dir
/// and return its path. Blocking tar + flate2 work runs on a worker
/// thread so it doesn't stall the tokio reactor on large directories.
pub(super) async fn tar_gz_into_temp(source: &Path, desired_name: &str) -> Result<PathBuf> {
    let source = source.to_path_buf();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u64, |d| d.as_nanos() as u64);
    let tmp = std::env::temp_dir().join(format!("scriptorium-{nonce:x}-{desired_name}"));
    let tmp_clone = tmp.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let out = std::fs::File::create(&tmp_clone)?;
        let encoder = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        if source.is_dir() {
            let root_name = source
                .file_name()
                .map_or_else(|| std::ffi::OsString::from("workspace"), ToOwned::to_owned);
            builder.append_dir_all(&root_name, &source)?;
        } else {
            let basename = source
                .file_name()
                .map_or_else(|| std::ffi::OsString::from("file"), ToOwned::to_owned);
            let mut f = std::fs::File::open(&source)?;
            builder.append_file(&basename, &mut f)?;
        }
        builder.finish()?;
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(format!("tar.gz join: {e}")))?
    .map_err(Error::from)?;
    Ok(tmp)
}
