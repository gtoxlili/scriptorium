//! AI-facing tool catalog.
//!
//! `ListTools` returns descriptors that fit the OpenAI function-call / MCP
//! shape. `CallTool` is a thin router over the same primitives that
//! `Exec`, `FetchIntoWorkspace`, and `UploadToOSS` expose — agent-core (or
//! any other consumer) can skip the primitives entirely and drive the
//! service through these three tools.

use serde::{Deserialize, Serialize};

use crate::pb::ToolDescriptor;

pub const TOOL_EXECUTE_SHELL: &str = "execute_shell";
pub const TOOL_FETCH: &str = "fetch";
pub const TOOL_DELIVER: &str = "deliver";

/// The catalog returned by `ListTools`.
pub fn descriptors() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: TOOL_EXECUTE_SHELL.to_string(),
            description: "Run a shell command inside an isolated sandbox container \
                (Debian 12 + Python + Node + Chromium/Playwright + ffmpeg). \
                Stdout, stderr and exit code are returned synchronously. \
                Home directory state persists across calls within the same workspace_id. \
                Use this for any scripting, data processing, or RPA work."
                .to_string(),
            parameters_schema: EXECUTE_SHELL_SCHEMA.to_string(),
        },
        ToolDescriptor {
            name: TOOL_FETCH.to_string(),
            description: "Download an HTTP(S) URL directly into the workspace. \
                Use this to bring user-provided inputs into the sandbox rather than \
                piping bytes through the client. Loopback / private IPs are rejected."
                .to_string(),
            parameters_schema: FETCH_SCHEMA.to_string(),
        },
        ToolDescriptor {
            name: TOOL_DELIVER.to_string(),
            description: "Upload a workspace file or directory to object storage and \
                return a signed download URL you can hand to the end user. \
                Directories are tar.gz'd automatically."
                .to_string(),
            parameters_schema: DELIVER_SCHEMA.to_string(),
        },
    ]
}

// ─── Tool argument types (serde) ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExecuteShellArgs {
    pub command: String,
    #[serde(default)]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct FetchArgs {
    pub url: String,
    pub target_path: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub timeout_seconds: u32,
}

#[derive(Debug, Deserialize)]
pub struct DeliverArgs {
    pub path: String,
    #[serde(default)]
    pub compress: bool,
    #[serde(default)]
    pub ttl_seconds: u32,
    #[serde(default)]
    pub label: String,
}

// ─── Tool result shapes ───────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ExecuteShellResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}

#[derive(Debug, Serialize)]
pub struct FetchResult {
    pub target_path: String,
    pub bytes_written: u64,
    pub content_type: String,
    pub http_status: i32,
}

#[derive(Debug, Serialize)]
pub struct DeliverResult {
    pub url: String,
    pub object_key: String,
    pub size_bytes: u64,
    pub content_type: String,
    pub sha256_hex: String,
}

// ─── JSON Schema constants ────────────────────────────────────────────────
// Shaped to match the OpenAI function-call / MCP tool descriptor format.

const EXECUTE_SHELL_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "command": {
      "type": "string",
      "description": "Shell command to run, executed via `bash -lc`."
    },
    "timeout_seconds": {
      "type": "integer",
      "minimum": 1,
      "description": "Hard wall-clock timeout. Omit or 0 to use the server default."
    },
    "env": {
      "type": "object",
      "additionalProperties": { "type": "string" },
      "description": "Environment variables to inject for this call."
    }
  },
  "required": ["command"]
}"#;

const FETCH_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "url": {
      "type": "string",
      "description": "HTTP(S) URL to download. Private / loopback addresses are rejected."
    },
    "target_path": {
      "type": "string",
      "description": "Path inside the workspace (relative to $HOME) to write the body to."
    },
    "headers": {
      "type": "object",
      "additionalProperties": { "type": "string" },
      "description": "Extra request headers (auth, user-agent, ...)."
    },
    "timeout_seconds": {
      "type": "integer",
      "minimum": 1,
      "description": "Total wall-clock cap. Omit or 0 to use the server default."
    }
  },
  "required": ["url", "target_path"]
}"#;

const DELIVER_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "description": "Workspace-relative path (file or directory) to upload."
    },
    "compress": {
      "type": "boolean",
      "description": "When true, single files are gzipped. Directories are always tar.gz'd."
    },
    "ttl_seconds": {
      "type": "integer",
      "minimum": 1,
      "description": "Signed URL lifetime. Clamped to the server max (default 24h)."
    },
    "label": {
      "type": "string",
      "description": "Optional human-readable tag attached as object metadata."
    }
  },
  "required": ["path"]
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_schemas_are_valid_json() {
        for s in [EXECUTE_SHELL_SCHEMA, FETCH_SCHEMA, DELIVER_SCHEMA] {
            serde_json::from_str::<serde_json::Value>(s)
                .unwrap_or_else(|e| panic!("invalid schema: {e}\n{s}"));
        }
    }

    #[test]
    fn descriptors_list_three_tools() {
        let d = descriptors();
        assert_eq!(d.len(), 3);
        let names: Vec<_> = d.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&TOOL_EXECUTE_SHELL));
        assert!(names.contains(&TOOL_FETCH));
        assert!(names.contains(&TOOL_DELIVER));
    }
}
