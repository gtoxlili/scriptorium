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
    UV_PYTHON_PREFERENCE=system

# --- system packages -------------------------------------------------------
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates tzdata locales \
      tini \
      curl wget jq git unzip zip openssh-client \
      ripgrep fd-find tree \
      ffmpeg imagemagick exiftool \
      fonts-noto-cjk fonts-noto-color-emoji \
      python3 python3-venv \
      gnupg build-essential \
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
# yt-dlp: social-media / streaming video + audio downloader. Installed from
# the upstream self-contained zipapp binary (much fresher than Debian's
# apt package — yt-dlp cuts new releases weekly as sites change).
#
# duckdb: CLI for running ad-hoc SQL over CSV / Parquet / JSON. Download
# URL is arch-specific, so detect the Debian arch.
RUN set -eux \
 && curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux \
      -o /usr/local/bin/yt-dlp \
 && chmod +x /usr/local/bin/yt-dlp \
 && ARCH=$(dpkg --print-architecture) \
 && case "$ARCH" in \
      amd64) DUCKDB_ARCH=amd64 ;; \
      arm64) DUCKDB_ARCH=arm64 ;; \
      *) echo "unsupported arch for duckdb: $ARCH" >&2; exit 1 ;; \
    esac \
 && curl -fsSL "https://github.com/duckdb/duckdb/releases/latest/download/duckdb_cli-linux-${DUCKDB_ARCH}.zip" \
      -o /tmp/duckdb.zip \
 && unzip /tmp/duckdb.zip -d /usr/local/bin/ \
 && chmod +x /usr/local/bin/duckdb \
 && rm /tmp/duckdb.zip

# --- Bundled documentation indexes ----------------------------------------
# Plain-markdown API-doc indexes that the agent can grep offline to
# discover which endpoints exist; linked sub-pages are still fetched live
# (outbound network is open). Living under /opt/docs keeps them out of
# $HOME so the bind-mount does not shadow them at runtime.
#
# - Feishu (Lark) OpenAPI — published as llms.txt by Apifox; ~400 endpoint
#   + schema doc URLs. Use when building any Feishu integration
#   (messaging, docs, bitable, sheets, calendar, approvals, events, …).
RUN mkdir -p /opt/docs \
 && curl -fsSL --retry 3 https://feishu.apifox.cn/llms.txt \
      -o /opt/docs/feishu-apifox-index.md

# --- Node 24 LTS via NodeSource --------------------------------------------
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/*

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
      jinja2 pyyaml tiktoken

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
