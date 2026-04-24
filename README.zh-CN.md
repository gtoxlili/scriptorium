# scriptorium

[English](./README.md) · [简体中文](./README.zh-CN.md)

[![CI](https://github.com/gtoxlili/scriptorium/actions/workflows/ci.yml/badge.svg)](https://github.com/gtoxlili/scriptorium/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![Transport](https://img.shields.io/badge/Transport-gRPC-lightgrey.svg)](proto/sandbox.proto)

给 LLM 类 agent 用的沙箱执行中间件。按需拉起隔离容器，把任意脚本、
RPA 流程、浏览器自动化挡在 agent 主进程外面跑。

底下接任意 OCI 兼容的 Docker daemon（macOS 上的 OrbStack、Docker
Desktop、Colima，或者 Linux 原生的 dockerd）。scriptorium 负责的是
编排那一层：容器生命周期、workspace bind-mount、资源限额、并发控
制、URL 拉文件、产物上传。

## 架构图

```
                                         ┌──── URLs ────┐
                                         │              │
 caller (agent)              scriptorium            Docker          OSS (S3-compat)
───────────────            ─────────────────     ──────────       ─────────────────
      │                            │                 │                   │
      │  Exec(workspace_id, cmd)   │  bollard: 拉镜像起容器,           │
      │                        ──▶ │  bind-mount $HOME,                  │
      │                            │  drop 到 uid 1000,  ──▶ ┌─────────┐ │
      │                            │  CPU/内存/PID 限额,     │container│ │
      │                            │  wall-clock 超时.       │bash -lc │ │
      │ ◀── stdout/stderr/exit ─── │                         └─────────┘ │
      │                            │                                     │
      │  FetchIntoWorkspace(url)──▶│  宿主侧 reqwest,                    │
      │                            │  SSRF 守卫,流式写盘.                │
      │                            │                                     │
      │  UploadToOSS(path) ──────▶ │  目录自动 tar.gz, aws-sdk-s3,       │
      │ ◀── object_key + metadata ─│  过阈值走 multipart. ─────────────▶│
      │                            │                                     │
      │  Import/Export             │                                     │
      │   WorkspaceObject     ──▶  │  字节级流式传输,                    │
      │ ◀── chunks + metadata ──── │  给受信任的宿主桥接流程用.          │
      │                            │                                     │
      │  (调用方自己把 object_key 解析成永久 URL,                         │
      │   scriptorium 不发预签名链接.)
```

计算是一次性的（每次调用起一个新容器）。状态按 workspace 持久化：
装好的 pip 包、Chromium profile、跑出来的产物，都落在一个按
`workspace_id` 索引的宿主机目录里跨调用保留。这个 id 对服务是不透
明的，调用方自己决定粒度（会话 id、任务 id 都行）。

大块字节不走 gRPC。输入从 HTTP URL 进来（`FetchIntoWorkspace`），输
出落在对象存储里，scriptorium 返回一个永久的 `object_key`。调用方
再通过自己的附件系统把这个 key 解析成用户能看到的 URL。scriptorium
故意不返回带 TTL 的预签名 URL —— 用户一周后回来点这个链接会 403，不
合格。调用方负责维护一个稳定的 URL 形态，每次访问时透明地再签一次。

完整设计原因在 [`docs/architecture.zh-CN.md`](docs/architecture.zh-CN.md)。

## 功能

- **gRPC API** + 流式 exec。`proto/sandbox.proto` 是跨语言契约。
- **按调用起容器**，直连本地 Docker daemon，调用间零 idle 占用。
- **按 workspace 持久化状态**，挂在容器里的固定路径下。
- **非 root 用户（UID 1000）**、只读 rootfs、有上限的 `/tmp` tmpfs。
- **每次 exec 的资源封顶**：CPU 毫核、内存字节、PID 数、wall-clock
  超时。并发通过 semaphore 控制（默认 4），超额请求在
  `exec_queue_timeout_seconds` 到期后返回 `RESOURCE_EXHAUSTED`。
- **客户端断开即时清理**：`ExecStream` 的 caller 中途断开时，容器
  在几十 ms 内被 SIGKILL + 清掉，不会空耗 CPU 到 wall timeout。
- **URL 进、object_key 出的文件路径**：`FetchIntoWorkspace` 带 SSRF
  守卫，挡 loopback / RFC1918 / link-local / 广播 / 文档段 / CGNAT
  以及 IPv6 对应段。`UploadToOSS` 能打到任意 S3 兼容存储（默认贴合
  火山引擎 TOS），文件超过 `multipart_threshold_bytes`（默认 64
  MiB）走 multipart，峰值内存卡在 `part_size_bytes`。
- **LLM 工具层**：`ListTools` 发四个 OpenAI function-call 风格的
  descriptor（`execute_shell`、`deliver`，加两个 workspace-sandbox
  交换工具，后者留给 scriptorium 之上的宿主桥接用）。`CallTool`
  里 `execute_shell` / `deliver` 走跟 primitive RPC 同一个实现路径。
- **内容丰富的沙箱镜像**（Debian 13）：Python 3.13 + `uv`、Node 24
  LTS、Playwright 版 Chromium、FFmpeg、ImageMagick、常用 CLI 都预
  装好了。运行时不用 `apt install`。
- **`workspace_id` 白名单校验**（`[A-Za-z0-9_-]{1,128}`），加上宿主
  端的路径逃逸保护。
- **`tini` 当 PID 1**，避免 Chromium 起的子进程变僵尸。

## 不做的事

- 跨机编排。scriptorium 是单机 daemon，一台不够就多跑几个实例在
  前面挂个 router。
- 凭据存储。调用方自己管密钥，每次 `exec` 通过 `env` 注入，或提前
  `FetchIntoWorkspace` 一个短时效凭据文件。
- 镜像构建。沙箱镜像自己用 CI 或手工 build，然后按 tag 引用。

## 目录结构

```
proto/sandbox.proto         gRPC 服务定义（跨语言契约）
src/
  main.rs                   二进制入口，CLI，优雅关停
  lib.rs                    模块树
  config.rs                 TOML 配置加载 + 校验
  error.rs                  错误类型 + 映射到 tonic::Status
  runtime.rs                基于 bollard 的 Docker 容器 runtime
  service.rs                gRPC 服务实现 + 并发 semaphore
  workspace.rs              Workspace 状态目录管理
  fetch.rs                  URL → workspace 下载 + SSRF 守卫
  oss.rs                    S3 兼容上传（支持 multipart）
  tools.rs                  LLM 工具 descriptor
docker/
  service.Dockerfile        服务多阶段 Rust 构建
  sandbox.Dockerfile        沙箱镜像
docker-compose.yml          参考部署
deploy/config.example.toml  服务配置样例
docs/architecture.md        设计决策和边界（英文）
docs/architecture.zh-CN.md  设计决策和边界（中文）
.github/workflows/ci.yml    CI：fmt + clippy + check
```

## 依赖

- Rust 1.85+（2024 edition）。`rust-toolchain.toml` 钉了 toolchain。
- PATH 里有 `protoc`。macOS：`brew install protobuf`。Debian/Ubuntu：
  `apt-get install -y protobuf-compiler`。
- Docker 兼容 daemon，Unix socket 可达。
- S3 兼容对象存储凭据（火山引擎 TOS、AWS S3、腾讯 COS、MinIO、
  Cloudflare R2 等）配在 `[tos]` 下。

## 部署

macOS 我自己跑在 Docker 里。这样服务继承 OrbStack 已经授权过的 TCC
权限，不会每次启动都弹"scriptorium 想访问可移除卷"的弹窗。Linux
直接用 systemd + 原生二进制通常更简单。

### Docker（macOS 推荐）

```bash
# 1. 准备配置
cp deploy/config.example.toml deploy/config.toml
# 编辑 deploy/config.toml：
#   - [tos].access_key / .secret_key
#   - [workspace].root  — 宿主绝对路径，比如 /Volumes/SSD/scriptorium-state
# [docker].socket 保持 "/var/run/docker.sock"；下面的 run 命令会把
# OrbStack 的真实 socket 挂到这个路径。
chmod 600 deploy/config.toml

# 2. 构建服务镜像（首次 1-2 分钟，增量几秒）
docker build -f docker/service.Dockerfile -t scriptorium:latest .

# 3. 跑起来。WS_ROOT 必须跟 [workspace].root 完全一致 —— bind-mount
# 两侧路径必须相同，这样沙箱容器的 bind 路径在宿主 daemon 那边才能
# 解析到正确位置。
WS_ROOT=/Volumes/SSD/scriptorium-state
docker run -d \
  --name scriptorium \
  --restart=unless-stopped \
  -p 127.0.0.1:50051:50051 \
  -v "$HOME/.orbstack/run/docker.sock:/var/run/docker.sock" \
  -v "$WS_ROOT:$WS_ROOT" \
  -v "$(pwd)/deploy/config.toml:/etc/scriptorium/config.toml:ro" \
  -e RUST_LOG="info,bollard=warn" \
  scriptorium:latest
```

日常操作：

```bash
docker logs -f scriptorium
docker restart scriptorium        # 改完 config.toml 重启
docker rm -f scriptorium          # 停掉并删除

# 改了 Rust 代码后，重建镜像并重跑：
docker build -f docker/service.Dockerfile -t scriptorium:latest .
docker rm -f scriptorium
docker run -d … scriptorium:latest   # 参数同上
```

### Docker Compose（备选）

```bash
cp .env.example .env
# 编辑 .env 里的 SCRIPTORIUM_WORKSPACE_ROOT
docker compose up -d --build
docker compose logs -f
docker compose down
```

### 原生二进制

```bash
cargo build --release
cp deploy/config.example.toml deploy/config.toml
# 编辑 deploy/config.toml：设置 docker.socket 指向你的 daemon，
# workspace.root 指向宿主路径，填 [tos] 凭据。
./target/release/scriptorium --config deploy/config.toml
```

macOS 上 launchd 管的原生进程访问外置卷会触发 TCC 弹窗，Docker 那
条路径绕过这个问题。

## 构建沙箱镜像

```bash
docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian13-v1 .
```

镜像大概 3 GB。这是换来运行时不用 `apt install` 的代价。把 tag 钉
在 `deploy/config.toml` 里，沙箱镜像升级按自己的节奏来，跟服务升级
分开。

## 用其他语言消费

`proto/sandbox.proto` 是跨语言契约。用你喜欢的语言生成 stub，指向
`grpc://<host>:<port>`。

Go：

```bash
protoc --go_out=. --go-grpc_out=. proto/sandbox.proto
```

## 集成测试

`tests/e2e.rs` 对着真实 Docker daemon 跑每一个 RPC：

- `Health`：可达性 + 剩余 permit 数
- `Exec`：stdout/stderr 抓取、非零退出码、wall-clock 超时、
  workspace 状态跨调用持久化
- `ExecStream`：`Started` / chunk / `Finished` 顺序
- `FetchIntoWorkspace`：SSRF 守卫拒 loopback
- `ListFiles`：递归遍历反映 exec 产出的文件
- `DeleteWorkspace`：宿主目录删除 + 重复调用幂等
- `ListTools` / `CallTool`：descriptor 数量、schema 合法性、
  `execute_shell` 路由、未知工具的错误形态
- 非法 `workspace_id`：`InvalidArgument`

这批用 `#[ignore]` 门闸拦住，CI 里没 Docker 也不会挂。本地跑（先
把沙箱镜像 build 出来）：

```bash
cargo test --test e2e -- --ignored --nocapture
```

镜像 tag 通过 `SCRIPTORIUM_TEST_IMAGE=…` 覆盖，socket 通过
`DOCKER_HOST=unix:///path/to/docker.sock` 覆盖。要同时跑
`UploadToOSS` 的真实桶测试，设 `SCRIPTORIUM_TEST_TOS_ENDPOINT` /
`_REGION` / `_BUCKET` / `_ACCESS_KEY` / `_SECRET_KEY`。

## 当前状态

`proto/sandbox.proto` 里的每个 RPC —— `Exec`、`ExecStream`、
`FetchIntoWorkspace`、`UploadToOSS`、`ListFiles`、`DeleteWorkspace`、
`ImportWorkspaceObject`、`ExportWorkspaceObject`、`ListTools`、
`CallTool`、`Health` —— 都实现了，也都被 e2e 覆盖在 OrbStack 上跑
过。待办事项见 [`docs/architecture.zh-CN.md`](docs/architecture.zh-CN.md)：
为缩短冷启动的 warm pool、定时 workspace GC、基于 mitmproxy 的出站
审计。欢迎提 issue 和 PR。

## License

GPL-3.0-or-later，见 [`LICENSE`](LICENSE)。
