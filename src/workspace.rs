use std::{
    os::unix::fs::{PermissionsExt, chown},
    path::PathBuf,
};

use path_clean::PathClean;
use tokio::fs;

use crate::{
    config::WorkspaceConfig,
    error::{Error, Result},
};

/// `WorkspaceManager` owns the on-disk layout for per-workspace persistent state.
///
/// Layout: `{root}/{workspace_id}/home/` — bind-mounted as `/home/agent`
/// inside the container.
///
/// `workspace_id` is opaque to the service — callers are free to use
/// `session_id`, `task_id`, etc. The manager enforces that `workspace_id`
/// is a single path segment (no slashes, no traversal) drawn from
/// `[A-Za-z0-9_-]`.
#[derive(Clone, Debug)]
pub struct WorkspaceManager {
    cfg: WorkspaceConfig,
}

impl WorkspaceManager {
    #[must_use]
    pub fn new(cfg: WorkspaceConfig) -> Self {
        Self { cfg }
    }

    pub async fn ensure_root(&self) -> Result<()> {
        fs::create_dir_all(&self.cfg.root).await?;
        Ok(())
    }

    /// Resolve the host-side `home` directory for a workspace, creating it
    /// on first use and giving the in-container non-root user write access.
    ///
    /// The preferred path is a host-side `chown` to `uid:gid` so ownership
    /// aligns with the container user. That requires the service to be
    /// running as root (or to have `CAP_CHOWN`), which is typical in Linux
    /// deployments but not on macOS dev setups. When the chown is rejected
    /// with `EPERM` we fall back to `chmod 0777` — the directory is a
    /// per-workspace leaf inside a service-owned root, so the broader
    /// permission bit is acceptable and Docker's user namespace still
    /// confines what the container itself can do.
    pub async fn ensure_home(&self, workspace_id: &str, uid: u32, gid: u32) -> Result<PathBuf> {
        validate_workspace_id(workspace_id)?;
        let home = self.cfg.root.join(workspace_id).join("home");
        fs::create_dir_all(&home).await?;

        let home_clone = home.clone();
        let chown_result =
            tokio::task::spawn_blocking(move || chown(&home_clone, Some(uid), Some(gid)))
                .await
                .map_err(|e| Error::Other(format!("chown join: {e}")))?;

        if let Err(e) = chown_result {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                tracing::debug!(
                    path = %home.display(),
                    "chown denied — falling back to 0777 on workspace home"
                );
                fs::set_permissions(&home, std::fs::Permissions::from_mode(0o777)).await?;
            } else {
                return Err(e.into());
            }
        }

        Ok(home)
    }

    pub async fn delete(&self, workspace_id: &str) -> Result<bool> {
        validate_workspace_id(workspace_id)?;
        let dir = self.cfg.root.join(workspace_id);
        match fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve a user-supplied relative path inside the workspace home,
    /// rejecting anything that would escape the root.
    pub fn resolve_path(&self, workspace_id: &str, relative: &str) -> Result<PathBuf> {
        validate_workspace_id(workspace_id)?;
        let home = self.cfg.root.join(workspace_id).join("home");
        let joined = home.join(relative).clean();
        if !joined.starts_with(&home) {
            return Err(Error::PathEscape(relative.to_string()));
        }
        Ok(joined)
    }
}

/// Accept only `[A-Za-z0-9_-]+`, length 1..=128. Anything else is an error.
///
/// This is a whitelist on purpose — historically every "reject slashes and
/// `..`" blacklist has leaked (NUL bytes, Unicode lookalikes, rare path
/// separators on exotic filesystems). A tight whitelist is the right default.
fn validate_workspace_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        return Err(Error::InvalidWorkspaceId);
    }
    let ok = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(Error::InvalidWorkspaceId);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_sane_ids() {
        for id in ["abc", "A-B_C", "01234567890abcdef", "x"] {
            validate_workspace_id(id).unwrap_or_else(|_| panic!("should accept `{id}`"));
        }
    }

    #[test]
    fn rejects_bad_ids() {
        for id in [
            "",
            "..",
            "/a",
            "a/b",
            "a\\b",
            "a..b",
            "a\0b",
            "a b",
            "a.b",
            // 129 chars — just over the cap.
            &"x".repeat(129),
        ] {
            assert!(validate_workspace_id(id).is_err(), "should reject `{id}`");
        }
    }
}
