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
#   docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian12-v1 .

FROM debian:12-slim

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=Asia/Shanghai \
    LANG=C.UTF-8 \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    # Debian 12's system Python is PEP 668 "externally-managed"; without
    # this flag `pip install --user <pkg>` fails — which is exactly the
    # recipe the AI-facing execute_shell prompt advertises.
    PIP_BREAK_SYSTEM_PACKAGES=1 \
    NODE_ENV=production \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    # Globally-installed Node modules live here. Exposing via NODE_PATH
    # lets ad-hoc `node script.js` find `puppeteer-core` / `sharp`
    # without the agent having to `npm install` them locally again.
    NODE_PATH=/usr/lib/node_modules \
    # Pin Playwright browser cache to a system path so it survives the
    # bind-mount over /home/agent at runtime.
    PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright

# --- system packages -------------------------------------------------------
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates tzdata locales \
      tini \
      curl wget jq git unzip zip openssh-client \
      ripgrep fd-find \
      ffmpeg imagemagick exiftool \
      fonts-noto-cjk fonts-noto-color-emoji \
      python3 python3-pip python3-venv \
      gnupg build-essential \
 && ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime \
 && sed -i 's/# *\(zh_CN.UTF-8\)/\1/' /etc/locale.gen \
 && locale-gen \
 # Debian's `fd-find` binary is `fdfind`; expose it as `fd` so the
 # AI-facing prompt's "prefer fd over find" advice actually works.
 && ln -s /usr/bin/fdfind /usr/local/bin/fd \
 && rm -rf /var/lib/apt/lists/*

# --- Node 20 via NodeSource -------------------------------------------------
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/*

# --- Python packages (global baseline) --------------------------------------
# pip --break-system-packages is required on Debian 12 to install into /usr.
# Agent-added pip packages at runtime land under ~/.local via `pip install --user`.
RUN pip3 install --break-system-packages --no-cache-dir \
      requests httpx \
      pandas numpy \
      pillow \
      beautifulsoup4 lxml \
      openpyxl xlrd \
      weasyprint \
      playwright selenium

# Install Chromium via Playwright so the browser lives at
# $PLAYWRIGHT_BROWSERS_PATH (/opt/ms-playwright), not in the bind-mounted
# $HOME. install-deps pulls the Debian packages Chromium needs at runtime.
RUN python3 -m playwright install-deps chromium \
 && python3 -m playwright install chromium \
 && rm -rf /var/lib/apt/lists/*

# --- Node packages (global baseline) ----------------------------------------
RUN npm install -g --omit=dev \
      puppeteer-core \
      sharp

# --- Non-root user ----------------------------------------------------------
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
