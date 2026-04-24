//! Filesystem walkers that back `ListFiles`.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tokio::fs;

use crate::{error::Result, pb::FileInfo};

/// Recursive (or single-level) directory walk, yielding `FileInfo` entries
/// with paths relative to the workspace root.
pub(super) async fn collect_files(
    workspace_root: &Path,
    base: &Path,
    recursive: bool,
    out: &mut Vec<FileInfo>,
) -> Result<()> {
    let mut stack: Vec<PathBuf> = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            let is_dir = meta.is_dir();
            let rel = path
                .strip_prefix(workspace_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0i64, |d| d.as_secs() as i64);
            out.push(FileInfo {
                path: rel,
                size_bytes: meta.len(),
                mode: meta.permissions().mode() & 0o7777,
                is_dir,
                modified_unix,
            });
            if is_dir && recursive {
                stack.push(path);
            }
        }
    }
    Ok(())
}
