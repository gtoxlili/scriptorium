# syntax=docker/dockerfile:1.6
#
# Scriptorium sandbox runtime image — the "Swiss army knife Debian" used by
# the agent to run Python / Node / Chromium / FFmpeg / RPA workloads.
#
# This image is intentionally fat: pre-installed tools are cheaper than
# letting the agent `apt install` on every call (it cannot — non-root user,
# read-only rootfs at runtime).
#
# Build:
#   docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian13-v1 .

FROM debian:13-slim

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Asia/Shanghai \
    LANG=C.UTF-8 \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    NODE_ENV=production \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    # Globally-installed Node modules live here. Exposing via NODE_PATH
    # lets ad-hoc `node script.js` find `puppeteer-core` / `sharp`
    # without the agent having to `npm install` them locally again.
    NODE_PATH=/usr/lib/node_modules \
    # Pin Playwright browser cache to a system path so it survives the
    # bind-mount over /home/agent at runtime.
    PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright \
    # uv is the ONLY Python package manager shipped in this image. Its
    # cache lives under $HOME so ephemeral envs persist across
    # execute_shell calls within the same chat session (the bind-mount
    # survives the per-call container reap). `copy` link mode avoids
    # hardlink failures when the cache and target live on different
    # filesystems (bind-mount vs tmpfs).
    UV_CACHE_DIR=/home/agent/.cache/uv \
    UV_LINK_MODE=copy \
    # Let uv use the system Python instead of downloading its own — the
    # image already ships with Debian's python3.
    UV_PYTHON_PREFERENCE=system \
    # ─── China-local mirrors (build-time only; agents at runtime can \
    # still hit any public registry via outbound network). Chosen for \
    # reliability from the Shanghai region: TUNA for PyPI + Debian, \
    # npmmirror for npm + Node + Playwright, ghproxy for GitHub \
    # releases.
    PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    UV_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple \
    NPM_CONFIG_REGISTRY=https://registry.npmmirror.com
    # Note: intentionally NOT setting PLAYWRIGHT_DOWNLOAD_HOST. npmmirror's
    # Playwright binary mirror lags upstream by multiple builds, so pinning
    # to it breaks whenever `pip install playwright` pulls a version newer
    # than the mirror has synced. The official Microsoft CDN is fast enough
    # through any standard outbound proxy / direct connection.

# --- Debian apt mirror (TUNA) ---------------------------------------------
# Debian 13 (trixie) ships sources as deb822 under /etc/apt/sources.list.d/.
# Rewrite deb.debian.org/{debian,debian-security} to TUNA over plain HTTP
# — apt metadata is GPG-signed, so the transport doesn't need TLS and we
# sidestep any TLS-termination quirks on the local network path. This
# must come before the first `apt-get update` so every subsequent install
# benefits.
RUN sed -i \
      -e 's|http://deb.debian.org/debian|http://mirrors.tuna.tsinghua.edu.cn/debian|g' \
      -e 's|http://security.debian.org/debian-security|http://mirrors.tuna.tsinghua.edu.cn/debian-security|g' \
      /etc/apt/sources.list.d/debian.sources

# --- system packages -------------------------------------------------------
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates tzdata locales \
      tini \
      curl wget jq git unzip zip openssh-client \
      ripgrep fd-find tree \
      ffmpeg imagemagick exiftool \
      fonts-noto-cjk fonts-noto-color-emoji \
      python3 python3-venv \
      gnupg build-essential xz-utils \
 && ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime \
 && sed -i 's/# *\(zh_CN.UTF-8\)/\1/' /etc/locale.gen \
 && locale-gen \
 # Debian's `fd-find` binary is `fdfind`; expose it as `fd` so the
 # AI-facing prompt's "prefer fd over find" advice actually works.
 && ln -s /usr/bin/fdfind /usr/local/bin/fd \
 && rm -rf /var/lib/apt/lists/*

# --- uv (Astral) -----------------------------------------------------------
# Copied straight from the official Astral image; tracks latest stable
# between image builds. `uv` + `uvx` land at /usr/local/bin so they are on
# PATH for both root (build steps) and the non-root `agent` user.
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /usr/local/bin/

# --- Standalone tool binaries ---------------------------------------------
# duckdb: CLI for running ad-hoc SQL over CSV / Parquet / JSON. No PyPI/npm
# distribution for the CLI binary, only GitHub releases. We try a list of
# known-working GitHub proxies in order; the `|| curl ...` chain falls
# through on first success. Mirrors verified 2026-04 from a China-region
# network — if they all go dark, swap in a fresh list.
#
# (yt-dlp is installed from PyPI in the Python section below, no GitHub
# dependency needed.)
RUN set -eux \
 && ARCH=$(dpkg --print-architecture) \
 && case "$ARCH" in \
      amd64) DUCKDB_ARCH=amd64 ;; \
      arm64) DUCKDB_ARCH=arm64 ;; \
      *) echo "unsupported arch for duckdb: $ARCH" >&2; exit 1 ;; \
    esac \
 && DUCKDB_PATH="duckdb/duckdb/releases/latest/download/duckdb_cli-linux-${DUCKDB_ARCH}.zip" \
 && { curl -fsSL --retry 2 --connect-timeout 15 \
        "https://ghfast.top/https://github.com/${DUCKDB_PATH}" -o /tmp/duckdb.zip \
      || curl -fsSL --retry 2 --connect-timeout 15 \
           "https://gh-proxy.com/https://github.com/${DUCKDB_PATH}" -o /tmp/duckdb.zip \
      || curl -fsSL --retry 2 --connect-timeout 15 \
           "https://gh.ddlc.top/https://github.com/${DUCKDB_PATH}" -o /tmp/duckdb.zip \
      || { echo "all GitHub proxies failed for duckdb" >&2; exit 1; }; } \
 && unzip /tmp/duckdb.zip -d /usr/local/bin/ \
 && chmod +x /usr/local/bin/duckdb \
 && rm /tmp/duckdb.zip

# --- Bundled documentation indexes ----------------------------------------
# Plain-markdown API-doc indexes (llms.txt format) that the agent can
# grep offline to discover which endpoints exist; linked sub-pages are
# still fetched live (outbound network is open). Living under /opt/docs
# keeps them out of $HOME so the bind-mount does not shadow them at
# runtime.
#
# All are hosted by Apifox and share the same shape: markdown bullets
# `- <category> [<title>](<url>.md): <summary>`.
#
#   - Feishu (Lark) OpenAPI — messaging, docs, bitable, sheets, calendar,
#     approvals, workspace events, …
#   - DingTalk OpenAPI — enterprise IM, mini-program, workbench,
#     contacts, OA suite.
#   - Douyin / TikTok (CN) OpenAPI — auth, video, commerce, live.
#   - Bilibili OpenAPI — creator + webhook endpoints.
#   - Xiaohongshu (小红书) OpenAPI — brand/marketing critical platform.
#   - Kuaishou OpenAPI — short-video ecosystem.
RUN mkdir -p /opt/docs \
 && curl -fsSL --retry 3 https://feishu.apifox.cn/llms.txt \
      -o /opt/docs/feishu-apifox-index.md \
 && curl -fsSL --retry 3 https://dingtalk.apifox.cn/llms.txt \
      -o /opt/docs/dingtalk-apifox-index.md \
 && curl -fsSL --retry 3 https://douyin.apifox.cn/llms.txt \
      -o /opt/docs/douyin-apifox-index.md \
 && curl -fsSL --retry 3 https://bilibili.apifox.cn/llms.txt \
      -o /opt/docs/bilibili-apifox-index.md \
 && curl -fsSL --retry 3 https://xiaohongshu.apifox.cn/llms.txt \
      -o /opt/docs/xiaohongshu-apifox-index.md \
 && curl -fsSL --retry 3 https://kuaishou.apifox.cn/llms.txt \
      -o /opt/docs/kuaishou-apifox-index.md

# --- Node 24 LTS via npmmirror tarball -------------------------------------
# NodeSource's apt repo isn't mirrored in China; npmmirror is. Version
# discovery and file download use DIFFERENT paths:
#   - Directory listing: registry.npmmirror.com/-/binary/node/ serves a
#     JSON index (the cdn path 404s on OSS list-bucket). We pick the
#     latest v24.x patch release so the image doesn't go stale.
#   - Actual tarball: cdn.npmmirror.com/binaries/node/vX.Y.Z/…tar.xz
#     is the direct-200 endpoint (the `/mirrors/node/` and
#     `/-/binary/node/` paths both 302 back to here, so we skip the hop).
RUN set -eux \
 && ARCH=$(dpkg --print-architecture) \
 && case "$ARCH" in \
      amd64) NODE_ARCH=x64 ;; \
      arm64) NODE_ARCH=arm64 ;; \
      *) echo "unsupported arch for node: $ARCH" >&2; exit 1 ;; \
    esac \
 && NODE_VERSION=$(curl -fsSL --retry 3 --connect-timeout 30 \
      https://registry.npmmirror.com/-/binary/node/ \
      | jq -r '.[] | select(.name | startswith("v24.")) | .name' \
      | tr -d '/' | sort -V | tail -1) \
 && [ -n "$NODE_VERSION" ] || { echo "failed to resolve latest Node v24 from npmmirror" >&2; exit 1; } \
 && echo "installing Node ${NODE_VERSION} (${NODE_ARCH})" \
 && curl -fsSL --retry 3 --connect-timeout 30 \
      "https://cdn.npmmirror.com/binaries/node/${NODE_VERSION}/node-${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
      -o /tmp/node.tar.xz \
 && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 --no-same-owner \
 && rm /tmp/node.tar.xz \
 && node --version && npm --version

# --- Python packages (global baseline) -------------------------------------
# Installed via `uv pip install --system` into /usr/lib/python3/dist-packages
# (readable by every python3 invocation). --break-system-packages is
# required at build time to bypass Debian's PEP 668 guard on the system
# interpreter; at runtime the whole system site is read-only anyway, so
# the agent's additions go elsewhere (ephemeral uv envs under
# $HOME/.cache/uv or a user-created venv under $HOME).
RUN uv pip install --system --break-system-packages --no-cache \
      requests httpx \
      pandas numpy matplotlib \
      pillow \
      beautifulsoup4 lxml \
      openpyxl \
      weasyprint \
      playwright \
      jinja2 pyyaml tiktoken \
      yt-dlp

# Install Chromium via Playwright so the browser lives at
# $PLAYWRIGHT_BROWSERS_PATH (/opt/ms-playwright), not in the bind-mounted
# $HOME. install-deps pulls the Debian packages Chromium needs at runtime.
RUN python3 -m playwright install-deps chromium \
 && python3 -m playwright install chromium \
 && rm -rf /var/lib/apt/lists/*

# --- Node packages (global baseline) ---------------------------------------
RUN npm install -g --omit=dev \
      puppeteer-core \
      sharp

# --- Non-root user ---------------------------------------------------------
# UID/GID must match the values the service advertises in its config so the
# bind-mounted home directory ownership aligns with this in-container user.
RUN groupadd -g 1000 agent \
 && useradd  -u 1000 -g 1000 -m -s /bin/bash agent \
 && chown -R agent:agent /home/agent

USER agent
WORKDIR /home/agent

# tini reaps zombie processes spawned by Chromium — without it a long-running
# sandbox that launches a browser will slowly accumulate defunct subprocesses.
ENTRYPOINT ["/usr/bin/tini", "--"]

# Callers supply the actual command per-exec via `docker run ... bash -lc "…"`.
