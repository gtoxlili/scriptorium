//! AI-facing tool catalog.
//!
//! `ListTools` returns descriptors that fit the OpenAI function-call / MCP
//! shape. `CallTool` is a thin router over the same primitives that
//! `Exec`, `FetchIntoWorkspace`, and `UploadToOSS` expose — agent-core (or
//! any other consumer) can skip the primitives entirely and drive the
//! service through these three tools.
//!
//! Descriptions below are deliberately verbose. They are the **only**
//! context the LLM has when deciding whether/how to call a tool, so every
//! relevant capability and limit is spelled out — implicit assumptions
//! end up as failed tool calls and wasted turns.

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
            description: EXECUTE_SHELL_DESCRIPTION.to_string(),
            parameters_schema: EXECUTE_SHELL_SCHEMA.to_string(),
        },
        ToolDescriptor {
            name: TOOL_FETCH.to_string(),
            description: FETCH_DESCRIPTION.to_string(),
            parameters_schema: FETCH_SCHEMA.to_string(),
        },
        ToolDescriptor {
            name: TOOL_DELIVER.to_string(),
            description: DELIVER_DESCRIPTION.to_string(),
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
    pub object_key: String,
    pub basename: String,
    pub size_bytes: u64,
    pub content_type: String,
    pub sha256_hex: String,
}

// ─── Tool descriptions ────────────────────────────────────────────────────
//
// Kept as module-level consts (not inlined) so they can grow without
// making `descriptors()` unreadable.

const EXECUTE_SHELL_DESCRIPTION: &str = "\
Run a shell command inside an isolated, ephemeral sandbox container. \
Stdout, stderr, and exit code are returned synchronously. Use this for \
scripting, data processing, web scraping, browser automation, media \
conversion, or any shell work.

Environment
───────────
- Debian 12, UTF-8 + zh_CN.UTF-8 locales, Asia/Shanghai tz, Noto CJK + \
color-emoji fonts.
- Non-root user `agent` (UID 1000). cwd is $HOME = /home/agent.
- Root filesystem is READ-ONLY; only $HOME (persistent across calls in \
this session) and /tmp (tmpfs, 2 GiB, per-call ephemeral) are writable.
- Outbound network is open; there is no inbound connectivity.
- Resource caps: 4 vCPU, 8 GiB RAM, 512 PIDs, 5-minute wall-clock \
default (override via `timeout_seconds`).

Pre-installed tools
───────────────────
- Python 3.12 — pandas, numpy, pillow, requests, httpx, beautifulsoup4, \
lxml, openpyxl, xlrd, weasyprint, playwright, selenium.
- Node 20 — puppeteer-core, sharp.
- Headless Chromium with Playwright drivers (use Python `playwright` or \
Node `puppeteer-core`).
- Media — ffmpeg, imagemagick (`convert`), exiftool.
- CLI — bash, curl, wget, jq, git, unzip, zip, openssh-client, ripgrep, \
fd, openssl, gnupg.

State persistence
─────────────────
$HOME contents survive between `execute_shell` calls within the SAME \
chat session — installed pip/npm packages, Playwright cookies, \
intermediate files, ad-hoc scripts all stick. When the session ends the \
workspace is eventually garbage-collected.

What you CANNOT do
──────────────────
- `apt install` / `apt-get` / `dpkg` (root FS is read-only and you are \
not root).
- Write outside $HOME or /tmp.
- Escalate to root, open raw sockets, mount filesystems.

For missing Python deps: `pip install --user <pkg>` (lands in \
$HOME/.local, persists with the session). For Node: \
`npm install --prefix $HOME/.local <pkg>`.

Bring inputs in via the `fetch` tool (HTTP → $HOME) and hand outputs \
to the user via the `deliver` tool ($HOME → object storage → URL).";

const FETCH_DESCRIPTION: &str = "\
Download an HTTP(S) URL directly into the sandbox workspace. Use this to \
pull any user-supplied file (chat attachment URL, user-shared link, \
third-party resource) into $HOME where `execute_shell` can work on it.

Constraints
───────────
- Schemes: http, https only. No file://, ftp, ssh, data:, etc.
- Max response body: 1 GiB — exceeding it fails fast mid-stream.
- Follows up to 5 HTTP redirects.
- Target IP must be public-routable. Rejected as SSRF: loopback (127/8), \
RFC1918 (10/8, 172.16/12, 192.168/16), link-local (169.254/16), CGNAT \
(100.64/10), IPv4 broadcast / docs / unspecified, IPv6 loopback (::1), \
ULA (fc00::/7), IPv6 link-local (fe80::/10).
- Default wall-clock: 60 s (override via `timeout_seconds`).

Filesystem semantics
────────────────────
- `target_path` is resolved relative to $HOME and rejected if it \
escapes via `..` or absolute paths.
- Missing parent directories are created.
- Existing files at that path are overwritten atomically.

Returns `bytes_written`, server-declared `content_type`, and the final \
`http_status`. Non-2xx statuses are reported as tool errors.

Typical flow: user shares a PDF URL → fetch(url=<url>, \
target_path=\"inputs/report.pdf\") → execute_shell(\"python3 \
~/analyze.py ~/inputs/report.pdf > ~/outputs/summary.md\") → \
deliver(path=\"outputs/summary.md\").";

const DELIVER_DESCRIPTION: &str = "\
Package a file or directory produced in the sandbox, upload it to \
durable object storage, and return a PERMANENT public URL you can paste \
to the end user in your chat response. The URL does not expire — each \
access transparently re-signs a short-lived presign under the hood, so \
the user can revisit it days or weeks later.

Behavior
────────
- File, compress=false (default): uploaded as-is with a MIME type \
inferred from the file extension.
- File, compress=true: gzipped first, then uploaded as \
application/gzip.
- Directory: always tar.gz'd into a single archive before upload \
(regardless of `compress`).

Size cap: 1 GiB post-compression.

Response fields
───────────────
- `url` — the PERMANENT public URL. This is the ONLY field you should \
paste into your reply to the user.
- `size_bytes`, `content_type`, `sha256_hex` — metadata for your own \
reasoning (e.g. deciding whether to mention the size to the user). \
Do NOT paste these fields to the user.

When to use
───────────
- For anything the user asked you to produce and wants to see / \
download: generated reports, charts, spreadsheets, rendered PDFs, \
edited images, zipped project outputs, etc.
- With `label=\"<short description>\"` so the attachment carries a \
human-readable tag in storage-side metadata.

When NOT to use
───────────────
- Throwaway intermediate files — they live in $HOME and are available \
to later `execute_shell` calls for free.
- Agent-internal state or scratch data.
- Anything containing secrets you wouldn't show the user.

Typical flow: `execute_shell` produces $HOME/outputs/summary.pdf → \
deliver(path=\"outputs/summary.pdf\", label=\"Q3 revenue summary\") → \
receives {url: \"https://...\", size_bytes: 245_760, ...} → reply to \
the user with just the URL.";

// ─── JSON Schema constants ────────────────────────────────────────────────
// Per-parameter descriptions stay concise; the big-picture capability +
// constraints live in the tool descriptions above.

const EXECUTE_SHELL_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "command": {
      "type": "string",
      "description": "Shell command to run, executed via `bash -lc` in $HOME. Multi-line commands and pipelines are fine."
    },
    "timeout_seconds": {
      "type": "integer",
      "minimum": 1,
      "description": "Hard wall-clock timeout. Omit or 0 for the server default (300s)."
    },
    "env": {
      "type": "object",
      "additionalProperties": { "type": "string" },
      "description": "Extra environment variables for this call only. Layered on top of the container's base env."
    }
  },
  "required": ["command"]
}"#;

const FETCH_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "url": {
      "type": "string",
      "description": "HTTP(S) URL to download. Private / loopback / link-local / CGNAT addresses are rejected as SSRF."
    },
    "target_path": {
      "type": "string",
      "description": "Path inside the workspace (relative to $HOME, e.g. \"inputs/report.pdf\") to write the body to. Parent dirs auto-created; existing files overwritten."
    },
    "headers": {
      "type": "object",
      "additionalProperties": { "type": "string" },
      "description": "Extra request headers, e.g. {\"Authorization\": \"Bearer ...\"}."
    },
    "timeout_seconds": {
      "type": "integer",
      "minimum": 1,
      "description": "Total wall-clock cap for the whole download. Omit or 0 for the server default (60s)."
    }
  },
  "required": ["url", "target_path"]
}"#;

const DELIVER_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "description": "Workspace-relative path (file or directory) to upload. Directories are tar.gz'd automatically."
    },
    "compress": {
      "type": "boolean",
      "description": "When true, single files are gzipped before upload. Ignored for directories (they are always tar.gz'd)."
    },
    "label": {
      "type": "string",
      "description": "Optional short human-readable tag stored as object metadata; useful for auditing / cataloging in the bucket."
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

    #[test]
    fn descriptions_mention_critical_constraints() {
        let d = descriptors();
        let by_name = |n: &str| {
            d.iter()
                .find(|t| t.name == n)
                .expect("descriptor missing")
                .description
                .clone()
        };

        let shell = by_name(TOOL_EXECUTE_SHELL);
        assert!(
            shell.contains("read-only") || shell.contains("READ-ONLY"),
            "exec must flag read-only rootfs"
        );
        assert!(
            shell.contains("pip install --user"),
            "exec must point at --user for extra deps"
        );
        assert!(
            shell.contains("UID 1000") || shell.contains("Non-root"),
            "exec must flag non-root"
        );

        let fetch = by_name(TOOL_FETCH);
        assert!(fetch.contains("SSRF"), "fetch must mention SSRF");
        assert!(fetch.contains("1 GiB"), "fetch must mention body cap");

        let deliver = by_name(TOOL_DELIVER);
        assert!(
            deliver.contains("PERMANENT") || deliver.contains("does not expire"),
            "deliver must flag URL persistence"
        );
        assert!(
            deliver.contains("ONLY"),
            "deliver must tell LLM which field to paste"
        );
    }
}
