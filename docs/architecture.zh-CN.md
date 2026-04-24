# 架构

[English](./architecture.md) · [简体中文](./architecture.zh-CN.md)

scriptorium 的设计说明：为什么独立成一个服务、隔离模型实际保证什么、
边界在哪儿。动 `runtime.rs` 或 proto 之前先读这份。

## 为什么独立成一个服务

Agent runtime 迟早要代表用户去跑任意脚本、浏览器自动化、RPA 流程。
把这层能力塞进 agent 进程有三个问题：

1. **爆炸半径**。用户脚本泄内存或者崩了，不能把 agent runtime 带
   着一起死。
2. **发版节奏**。沙箱镜像的演进节奏跟 agent 代码不一样（Chromium
   安全补丁、新 Python 库之类的）。耦合在一起就得 lockstep 发版。
3. **可复用**。下一个 agent —— 不只是第一个 —— 应该能通过 gRPC 直接
   用这套中间件，不用把 Docker 客户端、镜像定义、workspace 状态管
   理这些东西都吞下去。

所以 scriptorium 是独立的二进制，独立的仓库，独立的部署产物，对外
靠稳定的 gRPC 契约。

## 核心模型

### 计算：每次调用一次性

每个 `Exec` RPC 起一个新容器，用 `bash -lc` 跑请求里的 shell 命令，
调用返回时把容器拆掉。没有服务端 session，也没有 idle 容器池。

上游的 chat session 本来就没有固定过期时间，按 session 绑容器会无
限堆积。一次性起容器模型换来的代价大概是 1-3 秒 docker-run 开销，
换来的是 idle 占用归零。

### 状态：按 workspace 持久化

调用之间必须保留的状态 —— `pip --user` 装的包、Chromium cookie、LLM
写下的脚本、exec 跑出来的产物 —— 放在宿主上的一个目录里，按
`workspace_id` 把同一批调用 bind-mount 到容器里：

```
宿主：  {workspace_root}/{workspace_id}/home/
挂载：  /home/agent                     （容器内）
```

容器内用户是固定 UID/GID（默认 `1000:1000`）。首次访问时把宿主目录
chown 到这对 uid/gid，这样 bind-mount 写出来的文件归属是对的。如果
chown 返回 `EPERM`（macOS 上服务非 root 跑的场景），就退回到在这个
单独的 workspace 目录上 `chmod 0777`，让容器用户还能写。生产 Linux
部署应该让服务以 root 身份跑，或者给它 `CAP_CHOWN`，走更紧的 chown
路径。

### workspace_id 对服务不透明

服务不理解"chat session"、"品牌"、"tenant"这些概念。调用方传一个
`workspace_id` 字符串，服务就拿这个 key 存状态。契约保持窄，
scriptorium 不用去学上游的业务概念。

`workspace_id` 走 `[A-Za-z0-9_-]{1,128}` 的白名单校验。用白名单不用
黑名单，是因为我见过的每一种"拒 `..` 和 `/`"的黑名单都早晚会被绕
（NUL 字节、Unicode 形近字符、冷门路径分隔符等）。

调用方按自己想要的状态生命周期来选粒度：session id、task id、job
id 都可以。

`tenant_id` 随每个 `Exec` 请求带过来，只用来记审计日志，不参与目录
寻址。

### 两类状态，刻意分开

- **Agent 工作态**（这个服务管）：临时文件、下载的数据、LLM 写的
  脚本、LLM 装的包。按 `workspace_id` 范围保存，生命周期跟调用方的
  workspace 一致。
- **业务凭据**（不归这个服务管）：平台登录 cookie、API token、用
  户私有资产。调用方自己用加密存储保管，每次 `Exec` 通过 `env`
  传进去，或者先用 `FetchIntoWorkspace` 拉一个短时效的凭据文件再
  exec。scriptorium 永远不存长期秘密。

混在一起 = 任何沙箱脚本都能读到任何凭据，而且凭据生命周期会被沙箱
GC 绑架。硬边界。

### 数据流：用户侧走 URL/object_key，受信桥接走直接流

入口偏好 URL。`FetchIntoWorkspace(url, target_path)` 是宿主侧的
`reqwest`，带 SSRF 守卫，流式写到 workspace。

出口偏好对象存储。`UploadToOSS(source_path, compress?)` 需要的话会
自动 tar + gzip 目录，上传到 S3 兼容存储（默认适配火山引擎 TOS），
返回永久的 `object_key` 加上 size / content-type / sha256 / basename。
文件超过 `multipart_threshold_bytes` 走 multipart 上传，峰值 RAM 卡
在 `part_size_bytes`，不管 artifact 多大都稳。

scriptorium 刻意不返回预签名 URL。调用方用自己的附件系统把
`object_key` 解析成用户能看到的 URL，一般是一个形如
`https://{host}/api/v1/public/attachments/{id}?access_key={secret}`
的稳定链接，每次访问时后端再签一个新的短 TTL 下载链接透传。用户收
藏的那个 URL 永不过期 —— 这才是实际产品要满足的需求，预签名 URL 做
不到。

对于受信任的宿主集成调用方，scriptorium 还暴露 workspace import /
export 两个 RPC，直接走 gRPC 流式传字节。这是给宿主应用自己的
workspace 桥接流程用的本地握手路径，不是对外默认的数据进出通道。

带来的效果：

- 调用方不用代理 artifact 字节，不会堵在 I/O 热路径上。
- 预签名 TTL 对终端用户完全不可见，永久句柄的寿命远长于任何一次
  签发。
- 第三方凭据留在调用方的 vault 里，单次 exec 通过 `env` 或者短时
  效的 `FetchIntoWorkspace` 注入。

## 隔离模型

### 内核层

容器隔离靠的是内核级边界：

- **macOS（OrbStack / Docker Desktop / Colima）** 在
  `Virtualization.framework` 里跑 Linux VM。沙箱容器跟其他容器共用
  VM 内核，但跟 macOS 宿主内核是完全隔开的。
- **Linux 宿主** 用标准的 namespace + cgroup。

### 容器内

- 非 root 用户（UID 1000）。
- rootfs 只读；只有 `/home/agent`（bind-mount 的 workspace）和
  `/tmp`（有上限的 tmpfs）能写。
- `tini` 当 PID 1，回收僵尸子进程。对 Chromium 这种会 fork 大量
  helper 的场景很关键。
- 每容器资源封顶：CPU 毫核、内存字节、PID 数、tmpfs 大小。默认值
  在配置里，调用方可以在写死的天花板内覆盖。
- 硬 wall-clock 超时。到期的容器强杀，调用返回
  `DeadlineExceeded`。
- 中途取消：`ExecStream` 的 caller 没把 `Finished` 事件读完就断开
  时，容器会被 SIGKILL 掉并移除，而不是让它空耗到命令自然结束。

### 网络

当前行为：bridge 网络 + 出站无限制。这是合理的默认 —— 主要工作负载
（爬虫、RPA、媒体处理、API 调用）都需要公网。

`FetchIntoWorkspace` 在宿主侧带 SSRF 守卫：解析出的 IP 不能落在
loopback、RFC1918、link-local、广播、文档段、CGNAT（100.64/10），或
IPv6 对应段（loopback、ULA、link-local）里。`fetch.allow_private_network
= true` 关掉这个守卫，给那些确实需要访问内网主机的部署用。

下一期硬化会把容器出站接一个服务自己管的 mitmproxy，记录每个请求
的 host + path 用于审计，可选地按 tenant 做出站白名单。

### 准入控制

每个 `Exec` / `ExecStream` 都要从一个 tokio semaphore 里拿 permit，
容量是 `concurrency.max_concurrent_execs`（默认 4）。池子满了，请
求排队最多 `concurrency.exec_queue_timeout_seconds`（默认 30 秒），
然后带重试提示返回 `RESOURCE_EXHAUSTED`。这样一波 N 个用户同时打进
来时，不会各自吃掉 N × 8 GiB 的内存上限把宿主 OOM 掉。

`FetchIntoWorkspace`、`UploadToOSS`、`ImportWorkspaceObject`、
`ExportWorkspaceObject`、`ListFiles`、`DeleteWorkspace` 都不占
permit，因为都是宿主侧 I/O，不起容器。`CallTool` 里
`execute_shell` 和 `deliver` 走跟 primitive RPC 同一个 helper，所
以不管调用方从哪个表面进来，`execute_shell` 都被一致地挡在
semaphore 后面。剩下两个 workspace-sandbox 交换 descriptor 在
scriptorium 这一层只做 catalog，真正实现留给上面的宿主桥接。

`Health` 里会报 `exec_permits_available` 方便观察。

## 镜像设计

- **故意做胖**。Python、Node、Chromium / Playwright、FFmpeg、常用
  CLI 全预装，agent 不用每次 `apt install`。其实它也装不了：非 root
  用户、只读 rootfs。
- **Playwright 浏览器钉在 `/opt/ms-playwright`**。Playwright 默认下
  载到 `$HOME/.cache/ms-playwright`，但 `$HOME` 被 workspace bind
  盖住了，默认装法第一次 exec 浏览器就丢了。build 时把
  `PLAYWRIGHT_BROWSERS_PATH` 设好，浏览器就落在只读层里。
- **不装 apt 版 Chromium**。Debian 包和 Playwright 自带的浏览器放
  一起会版本漂移，以 Playwright 那份为准。

## 两个对外表面，一套实现

Primitive RPC（`Exec`、`ExecStream`、`FetchIntoWorkspace`、
`UploadToOSS`、`ListFiles`、`DeleteWorkspace`、`Health`）是协议层
的真相。LLM 工具层（`ListTools`、`CallTool`）盖在它们上面，发
OpenAI function-call / MCP 风格的 descriptor，四个工具：
`execute_shell`、`deliver`、两个 workspace-sandbox 交换工具。
`execute_shell` 和 `deliver` 走跟 primitive handler 同一个 `do_*`
helper。两个交换工具 descriptor 对外宣布的是宿主桥接契约，裸跑
scriptorium 直接调会显式报错。

之所以分两层：引擎侧 caller 想要可以直接流式的 primitive，LLM 侧
caller 想要一个带 JSON Schema 的薄 catalog。两边都能用，而且不会
drift。

## 为什么选 Rust + tonic + bollard

- **Rust + tonic**。scriptorium 本质是 gRPC 调用跟 Docker API 调用
  之间的低延时代理。Rust 给我可预测的延时和内存，没有 GC 停顿，基于
  tokio 的 async 也是这个生态的标准选择。tonic 是成熟的 gRPC 栈。
- **bollard**。活跃维护的 Rust Docker 客户端。attach / logs 原生流
  式，这是 `ExecStream` 能跑起来的前提。

## 不做的事

- **跨机编排**。单机 daemon。一台不够就多跑几个实例在前面挂
  router。这里不会做集群管理器。
- **镜像构建流水线**。沙箱镜像由 CI 或手工 out-of-band build 出
  来，按 tag 引用。scriptorium 启动时不 build 也不拉镜像，不过
  如果 tag 缺了，Docker daemon 自己的镜像缓存会在首次 exec 时拉
  一次。
- **凭据保管**。参见"两类状态"。有意为之的硬边界。

## 遗留问题

1. **Warm pool**。按镜像维护一小批预创建好的停止容器，帮高频调用
   方省掉 spawn 成本。先测了再说。实际测下来冷启动 ~1.5 s，等 LLM
   round trip 进入调用链，延时预算根本不花在这儿。
2. **定时 workspace GC**。手动 `DeleteWorkspace` 已经有了。按可配
   置 TTL 驱逐超过时限没活动的 workspace，算下一步要做的。
3. **mitmproxy 出站审计**。规划里的硬化动作（见"网络"那节）：把
   容器出站接到服务自己管的 mitmproxy 上做逐请求日志，再做按
   tenant 的出站白名单。
