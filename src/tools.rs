//! LLM-facing tool catalog.
//!
//! `ListTools` returns descriptors that fit the OpenAI function-call / MCP
//! shape. `CallTool` is a thin router over the same primitives that
//! `Exec`, `FetchIntoWorkspace`, and `UploadToOSS` expose, so any consumer
//! can skip the primitives entirely and drive the service through these
//! tools instead.
//!
//! Descriptions below are deliberately verbose. They are the *only*
//! context the LLM has when deciding whether and how to call a tool, so
//! every relevant capability and limit is spelled out; implicit
//! assumptions end up as failed tool calls and wasted turns.

use serde::{Deserialize, Serialize};

use crate::pb::ToolDescriptor;

pub const TOOL_EXECUTE_SHELL: &str = "execute_shell";
pub const TOOL_DELIVER: &str = "deliver";
pub const TOOL_COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX: &str =
    "copy_workspace_sandbox_to_execution_sandbox";
pub const TOOL_COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX: &str =
    "copy_execution_sandbox_to_workspace_sandbox";

/// The catalog returned by `ListTools`.
pub fn descriptors() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: TOOL_EXECUTE_SHELL.to_string(),
            description: EXECUTE_SHELL_DESCRIPTION.to_string(),
            parameters_schema: EXECUTE_SHELL_SCHEMA.to_string(),
        },
        ToolDescriptor {
            name: TOOL_DELIVER.to_string(),
            description: DELIVER_DESCRIPTION.to_string(),
            parameters_schema: DELIVER_SCHEMA.to_string(),
        },
        ToolDescriptor {
            name: TOOL_COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX.to_string(),
            description: COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX_DESCRIPTION.to_string(),
            parameters_schema: COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX_SCHEMA.to_string(),
        },
        ToolDescriptor {
            name: TOOL_COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX.to_string(),
            description: COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX_DESCRIPTION.to_string(),
            parameters_schema: COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX_SCHEMA.to_string(),
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
- Debian 13, UTF-8 + zh_CN.UTF-8 locales, Asia/Shanghai tz, Noto CJK + \
color-emoji fonts.
- Non-root user `agent` (UID 1000). cwd is $HOME = /home/agent.
- Root filesystem is READ-ONLY; only $HOME (persistent across calls in \
this session) and /tmp (tmpfs, 2 GiB, per-call ephemeral) are writable.
- Outbound network is open; there is no inbound connectivity.
- Resource caps: 4 vCPU, 8 GiB RAM, 512 PIDs, 5-minute wall-clock \
default (override via `timeout_seconds`).

Pre-installed tools
───────────────────
- Python 3.13 — pandas, numpy, matplotlib, pillow, requests, httpx, \
beautifulsoup4, lxml, openpyxl, weasyprint, playwright, jinja2, pyyaml, \
tiktoken.
- `uv` / `uvx` (Astral) — the ONLY Python package manager on this image. \
`pip` is intentionally NOT installed; every Python install path below \
goes through `uv`. uv's cache at $HOME/.cache/uv persists for the whole \
session, so re-running the same `uv run --with <pkg>` is near-instant \
after the first install.
- Node 24 LTS — puppeteer-core, sharp. Globally installed under \
`/usr/lib/node_modules` and exposed via `NODE_PATH`, so ad-hoc \
`node script.js` with `require('sharp')` / `require('puppeteer-core')` \
works without a local `npm install`.
- Browsers: ONLY headless Chromium is preinstalled (via Playwright, \
cached at `/opt/ms-playwright`). Do NOT attempt `playwright install \
firefox`/`webkit` or `puppeteer`-style browser downloads — the cache \
path is root-owned and the installer will fail.
- Media — ffmpeg, imagemagick (`convert`), exiftool.
- CLI — bash, curl, wget, jq, git, unzip, zip, openssh-client, tree, \
ripgrep (`rg`), fd, openssl, gnupg.
- Data / media utilities — `yt-dlp` (resilient video/audio downloader \
covering YouTube, Bilibili, Douyin/TikTok, Xiaohongshu, and most \
streaming sites; prefer over hand-rolled scraping), `duckdb` (SQL CLI \
for ad-hoc querying of CSV / Parquet / JSON — `duckdb -c \"SELECT ... \
FROM read_csv_auto('file.csv')\"`).
- Bundled doc indexes under `/opt/docs/` (all llms.txt-format — a \
markdown bullet list of doc URLs; grep for the endpoint, then \
`curl -fsSL <matched url>` to fetch the full page). Prefer these over \
web-searching — they are faster, quieter, and already curated:
  - `/opt/docs/feishu-apifox-index.md` — Feishu / Lark: messaging, \
    docs, bitable, sheets, calendar, approvals, workspace events, …
  - `/opt/docs/dingtalk-apifox-index.md` — DingTalk: enterprise IM, \
    mini-program, workbench, contacts, OA suite.
  - `/opt/docs/douyin-apifox-index.md` — Douyin / TikTok (CN): auth, \
    video, commerce, live.
  - `/opt/docs/bilibili-apifox-index.md` — Bilibili creator + webhook \
    endpoints.
  - `/opt/docs/xiaohongshu-apifox-index.md` — Xiaohongshu (小红书): \
    brand / marketing integrations.
  - `/opt/docs/kuaishou-apifox-index.md` — Kuaishou short-video \
    ecosystem.
  Example: `rg -i 'bitable\\|多维表格' /opt/docs/feishu-apifox-index.md \
  | head` → pick the URL → `curl -fsSL <url>` → write the integration.
- `build-essential` (gcc, g++, make) is present so uv can compile \
C-extension wheels on the fly.
- Prefer `rg` over `grep -r` and `fd` over `find`: they are an order of \
magnitude faster and respect sensible ignore rules by default.

Installing extra Python packages (uv-only)
──────────────────────────────────────────
There is no `pip` on PATH. `python3 -m pip` will also fail. Use uv for \
everything Python-package-shaped:

- Ephemeral, one-shot scripts (the default — use this unless you have a \
reason not to): `uv run --with <pkg> --with <pkg2> python script.py`, \
or for inline code `uv run --with <pkg> python - <<'PY' ... PY`. uv \
creates / reuses a cached env under $HOME/.cache/uv; the second \
invocation with the same package set is effectively free.
- Persistent across calls this session: create a venv once with `uv \
venv $HOME/.venv --system-site-packages` (the flag lets the venv see \
the baseline packages like pandas / playwright), then `uv pip install \
--python $HOME/.venv/bin/python <pkg>`. Run scripts with \
`$HOME/.venv/bin/python script.py`, or `source \
$HOME/.venv/bin/activate` inside a single `execute_shell` command.
- Project-scoped with a lockfile: `cd $HOME/<proj> && uv init && uv \
add <pkg>` — pyproject.toml + uv.lock + .venv/ all live in $HOME and \
survive for the rest of the session.

If you see a README that tells you to `pip install foo`, mentally \
translate it to `uv pip install --python $HOME/.venv/bin/python foo` \
(after creating that venv) or `uv run --with foo python ...`.

Installing extra Node packages
──────────────────────────────
`npm install --prefix $HOME/.local <pkg>` for persistent installs. \
Globally-installed packages (puppeteer-core, sharp) already resolve via \
NODE_PATH — no re-install needed.

Composing commands
──────────────────
- INDEPENDENT shell tasks (no ordering / data dependency) should be \
issued as SEPARATE parallel tool calls in a single turn, not fused \
into one chained command.
- DEPENDENT commands must be chained with `&&` inside ONE call so a \
failure short-circuits — e.g. `mkdir -p $HOME/out && python3 \
render.py > $HOME/out/report.md`.
- Use `;` only when you explicitly want the next command to run even \
if the previous one failed.
- Do NOT use bare newlines to separate commands in the `command` \
argument; chain with `&&` / `;`, or pass a heredoc to `bash -c`.
- Quote paths containing spaces, CJK characters, or other special \
characters with double quotes, e.g. `cat \"$HOME/inputs/年度报告.pdf\"`.
- Each call starts a fresh shell at $HOME. `cd` only affects THAT \
call's cwd; it never persists to the next call — use absolute paths \
across calls.

State persistence
─────────────────
$HOME contents survive between `execute_shell` calls within the SAME \
chat session — uv's ephemeral-env cache ($HOME/.cache/uv), any venv you \
create under $HOME (e.g. $HOME/.venv or $HOME/<proj>/.venv), \
user-installed npm packages under $HOME/.local, Playwright cookies, \
intermediate files, and ad-hoc scripts all stick. BEFORE installing or \
downloading, check what is already there so you do not repeat work: \
`ls $HOME/inputs $HOME/outputs 2>/dev/null`, `which <cli>`, or `python3 \
-c 'import <pkg>'` first. When the session ends the workspace is \
eventually garbage-collected.

Polling, waiting, and long output
─────────────────────────────────
- Do NOT `sleep` between commands that can run immediately — just run \
them.
- When polling an external process, use a check command (e.g. `gh run \
view`, `curl -fsS <url>/status`, a short `until ... ; do sleep 2; done` \
with a bounded counter) rather than a long blind `sleep`.
- If a command is likely to produce a lot of output, redirect it to a \
file under `$HOME/outputs/...` first and then print only a small slice \
(`head`, `tail`, `jq`) so the tool response stays compact — this also \
avoids triggering large-output spill (see below).

What you CANNOT do
──────────────────
- `apt install` / `apt-get` / `dpkg` (root FS is read-only and you are \
not root).
- Write outside $HOME or /tmp.
- Escalate to root, open raw sockets, mount filesystems.

Bringing inputs in / handing outputs out
────────────────────────────────────────
- Inputs: download them inside the command itself (`curl`, `wget`, \
Python `requests`, Node `fetch`) or work on files already present in \
$HOME.
- Outputs the user should see: hand them to the `deliver` tool \
($HOME → object storage → PERMANENT URL). NEVER paste raw in-sandbox \
paths (like `$HOME/outputs/foo.pdf`) to the user — they cannot reach \
that path; only the delivered URL is user-visible.

Large output handling
─────────────────────
Some hosts may spill oversized stdout/stderr into a host-side workspace \
file under `sandbox/...` instead of inlining the entire streams. In \
that case the result contains `output_mode=\"workspace_spill\"`, \
`workspace_path`, `char_count`, `line_count`, `preview`, and `message` \
instead of full stdout/stderr. Use the host's normal workspace \
read/search tools on `workspace_path` to inspect the full spill. To \
avoid hitting spill at all, write large output to a file under \
`$HOME/outputs/...` yourself and only print a small summary.";

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

Batching multiple related outputs
─────────────────────────────────
When the user should receive several related files as ONE deliverable \
(e.g. a report plus its assets), write them into a single directory \
like `$HOME/outputs/report-bundle/` and deliver the directory — the \
user gets one URL instead of a wall of links, and file layout is \
preserved inside the tar.gz.

Typical flow: `execute_shell` produces $HOME/outputs/summary.pdf → \
deliver(path=\"outputs/summary.pdf\", label=\"Q3 revenue summary\") → \
receives {url: \"https://...\", size_bytes: 245_760, ...} → reply to \
the user with just the URL.";

const COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX_DESCRIPTION: &str = "\
Copy a file or directory from the brand workspace `sandbox/` exchange \
folder into the execution sandbox workspace. Use this when a prior \
workspace tool created or edited inputs under `sandbox/...` and shell work \
now needs them inside $HOME.

Rules
─────
- `source_path` MUST point to `sandbox/...` inside the brand workspace. \
`extracts/...` is not valid here and stays read-only.
- Files are copied as-is to `target_path` inside the execution sandbox.
- Directories are packaged as tar.gz, transferred, then extracted so \
`target_path` becomes the copied directory root.
- Existing targets are replaced.

Typical flow: write or edit `sandbox/prompt.txt` with the host's \
workspace tools → copy_workspace_sandbox_to_execution_sandbox(\
source_path=\"sandbox/prompt.txt\", target_path=\"inputs/prompt.txt\") → \
execute_shell reads $HOME/inputs/prompt.txt.";

const COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX_DESCRIPTION: &str = "\
Copy a file or directory from the execution sandbox back into the brand \
workspace `sandbox/` exchange folder. Use this when shell work produced \
an output that should be inspected, searched, or edited with the \
host's normal workspace tools.

Rules
─────
- `target_path` MUST point to `sandbox/...` inside the brand workspace.
- `source_kind` must be `file` or `directory` because the execution \
sandbox does not expose a separate stat tool.
- Files are copied straight to `target_path`; directories are \
extracted so `target_path` becomes the copied directory root.
- Existing targets at `target_path` are replaced.

Typical flow: execute_shell writes $HOME/outputs/report.md → \
copy_execution_sandbox_to_workspace_sandbox(\
source_path=\"outputs/report.md\", source_kind=\"file\", \
target_path=\"sandbox/report.md\") → inspect or rewrite \
`sandbox/report.md` with the host's workspace tools.";

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

const COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "source_path": {
      "type": "string",
      "description": "Workspace-relative source path under `sandbox/`. May point to a file or a directory."
    },
    "target_path": {
      "type": "string",
      "description": "Destination path inside the execution sandbox workspace. Files land exactly here; directories are extracted so this path becomes the copied directory root."
    }
  },
  "required": ["source_path", "target_path"]
}"#;

const COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "source_path": {
      "type": "string",
      "description": "Path inside the execution sandbox to copy from."
    },
    "source_kind": {
      "type": "string",
      "enum": ["file", "directory"],
      "description": "Whether source_path points to a file or a directory."
    },
    "target_path": {
      "type": "string",
      "description": "Workspace-relative destination path under `sandbox/`."
    }
  },
  "required": ["source_path", "source_kind", "target_path"]
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_schemas_are_valid_json() {
        for s in [
            EXECUTE_SHELL_SCHEMA,
            DELIVER_SCHEMA,
            COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX_SCHEMA,
            COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX_SCHEMA,
        ] {
            serde_json::from_str::<serde_json::Value>(s)
                .unwrap_or_else(|e| panic!("invalid schema: {e}\n{s}"));
        }
    }

    #[test]
    fn descriptors_list_four_tools() {
        let d = descriptors();
        assert_eq!(d.len(), 4);
        let names: Vec<_> = d.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&TOOL_EXECUTE_SHELL));
        assert!(names.contains(&TOOL_DELIVER));
        assert!(names.contains(&TOOL_COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX));
        assert!(names.contains(&TOOL_COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX));
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
            shell.contains("uv run --with"),
            "exec must point at uv for extra Python deps (pip is not installed)"
        );
        assert!(
            !shell.contains("pip install --user"),
            "exec must not advertise `pip install --user` — pip is not installed in the image"
        );
        assert!(
            shell.contains("UID 1000") || shell.contains("Non-root"),
            "exec must flag non-root"
        );
        assert!(
            shell.contains("workspace_path"),
            "exec must mention host-side spill handling"
        );

        let deliver = by_name(TOOL_DELIVER);
        assert!(
            deliver.contains("PERMANENT") || deliver.contains("does not expire"),
            "deliver must flag URL persistence"
        );
        assert!(
            deliver.contains("ONLY"),
            "deliver must tell LLM which field to paste"
        );

        let push = by_name(TOOL_COPY_WORKSPACE_SANDBOX_TO_EXECUTION_SANDBOX);
        assert!(
            push.contains("sandbox/..."),
            "workspace->execution bridge must constrain source_path"
        );

        let pull = by_name(TOOL_COPY_EXECUTION_SANDBOX_TO_WORKSPACE_SANDBOX);
        assert!(
            pull.contains("source_kind"),
            "execution->workspace bridge must explain source_kind"
        );
    }
}
