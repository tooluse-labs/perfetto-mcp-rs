<p align="center">
  <img src="https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/assets/brand/logo-wordmark.svg" width="820" alt="perfetto-mcp-rs logo">
</p>

<p align="center">
  <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/tooluse-labs/perfetto-mcp-rs/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/releases"><img alt="Release" src="https://img.shields.io/github/v/release/tooluse-labs/perfetto-mcp-rs"></a>
  <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-MIT"><img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
</p>

<p align="center">
  <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/README.md">English</a> | <strong>简体中文</strong>
</p>

---

# perfetto-mcp-rs

让 LLM 读懂 [Perfetto](https://perfetto.dev) trace 的
[MCP](https://modelcontextprotocol.io) 服务器。在 Claude Code（或任意 MCP
客户端）里指向一个 trace 文件（`.pftrace` / `.perfetto-trace` / `.bin` 等都行
——按内容嗅探），直接用自然语言提问。服务端底层用 PerfettoSQL 查询，依赖
`trace_processor_shell`——首次运行自动下载，无需手动安装 Perfetto。

> 专用工具用内置的 SQL 模板；需要自定义分析时，agent 自己写 PerfettoSQL——并被引导到对的 stdlib 模块。

## 快速上手

perfetto-mcp-rs 要通过 MCP 客户端来用（Claude Code、Claude Desktop、Codex、
Cursor 等）——还没有的话先装一个。

**1. 安装**——下载预编译二进制；如果系统里已装了 Claude Code 或 Codex，还会
自动完成 MCP 注册：

```sh
# Linux / macOS / Windows（Git Bash、MSYS2、Cygwin）
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh
```
```powershell
# Windows（PowerShell）
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

重启 Claude Code，或新开一个 Codex session 即可用上。Homebrew、Cargo、项目
scope、直接下载二进制、手动注册等，见 [安装选项](#安装选项)。

**2. 用自然语言提问：**

> 加载 `~/traces/scroll_jank.pftrace`，告诉我滚动卡顿的主要原因。

把路径换成你自己的任意 Perfetto trace（用 Perfetto UI、`chrome://tracing`
或 `record_android_trace` 抓取）。

agent 会先调 `load_trace`，识别出这是 Chrome trace，然后直接用专用的
`chrome_scroll_jank_summary` 工具——不用手写 SQL。遇到专用工具覆盖不了的问题，
它才会降级到 `execute_sql`，对同一个 trace 写原始 PerfettoSQL。

最适合搭配支持多轮工具调用的 agentic 客户端（Claude Code、Codex、Claude
Desktop、Cursor），它们会顺着服务端错误消息里的提示自动走完整个流程。非
agentic 客户端虽然也能看到全部工具和错误提示，但不会自动串起这套引导流程。

## 工具

分析单个 trace 时，其它工具默认作用于最近一次加载的 trace。分析多个 trace 时，
用 `load_trace` 的 `paths` 一次加载多个文件，保留每个文件返回的 `trace_id`，并在
后续每个 trace 工具调用中传入目标 id。

MCP tool annotations 只是给客户端展示/路由用的意图与安全提示，不是服务端授权
或执行边界。

**核心（Essential）**

| 工具 | 用途 |
|---|---|
| `load_trace` | 用 `path` 打开一个 trace，或用 `paths` 打开多个；为每个文件返回不透明的 `trace_id` 和轻量路由摘要（类型/profile、时长、平台、进程/线程数、能力标签、脱敏策略、推荐下一步工具） |
| `execute_sql` | 执行 PerfettoSQL 查询（最多返回 5000 行）。标准分析优先用专用的 `chrome_*` / `list_*` 工具；只有它们没暴露的自定义 join/聚合才用本工具。输出塑形：`head`/`limit`、`summary`、`columns_only`、`include_row_count`、`max_string_len`。默认遮蔽 URL/header/cookie/路径中的敏感值 |

**探索（Exploration）**

| 工具 | 用途 |
|---|---|
| `list_tables` | 列出 trace 里的表和视图，支持 GLOB 过滤 |
| `list_table_structure` | 查看某张表的列名和类型 |
| `list_processes` | 列出进程（pid、名称、起止时间戳） |
| `list_threads_in_process` | 列出指定进程名下的线程（最多 2000 条） |
| `slice_descendants_breakdown` | 汇总长 slice id 下面的子 slice，免去手写 recursive CTE |
| `list_stdlib_modules` | 列出 PerfettoSQL stdlib 模块，支持 `domain` / `query` / `limit` 过滤（不需先加载 trace） |

**Chrome trace**——专用工具，让 agent 不必手写 SQL。每个工具都会在元信息里标记行/字符串是否截断。

| 工具 | 用途 |
|---|---|
| `chrome_scroll_jank_summary` | 按原因汇总最严重的滚动卡顿帧，带 cause、sub-cause、delay_since_last_frame |
| `chrome_page_load_summary` | 页面加载：URL、原始边界时间戳、FCP、LCP、DCL、load 耗时（ms） |
| `chrome_page_load_resource_summary` | 页面加载窗口内按 URL 紧凑聚合资源/请求，按最大 overlap 排序，并带规范化 origin、导航/renderer 相关性和归因范围证据 |
| `chrome_page_load_resource_pipeline` | 钻取单个 URL，将资源生命周期/request span 与后台解析、脚本执行、style/layout 信号合并，并标明 DNS/TLS/TTFB/cache/download 的推断边界 |
| `chrome_page_load_resource_hotspots` | thread/process/async track 上带 URL 的资源/请求类 slice 按页面加载/窗口 overlap 排序，尽量带进程/线程归属 |
| `chrome_page_load_script_hotspots` | Renderer 主线程脚本执行按 URL/slice/进程聚合，带 style/layout 子树信号 |
| `chrome_main_thread_hotspots` | 主线程任务按耗时排序，带 ts、upid/pid、cpu_pct，支持页面加载/时间窗口过滤 |
| `chrome_startup_summary` | 浏览器启动事件与首次可见内容时间 |
| `chrome_web_content_interactions` | Web 内容交互（点击、触摸、INP）按耗时排序 |

**资源（Resources）**

| Resource | 用途 |
|---|---|
| `resource://perfetto-mcp/stdlib-quickref` | 按需读取 Chrome、Android、通用 trace 的 PerfettoSQL stdlib 速查表 |

## 分析一个 trace

具体路径取决于 trace 类型：

- **Chrome trace**——`load_trace` → 直接用专用 `chrome_*` 工具 → 需要深入时再用
  `execute_sql` 对返回的行做下一步查询。分析慢 FCP/load 时，先看
  `chrome_page_load_resource_summary`，再用 `chrome_page_load_resource_pipeline`
  钻取单个慢 URL，或用 `chrome_page_load_resource_hotspots` 做 slice drilldown，
  之后再把主线程 `ResourceLoad*` slice 当成完整请求时间来解读。summary 的
  `resource_timing_evidence` 会说明是否存在 DNS/TLS/TTFB/download/cache 阶段
  线索；缺少 phase breakdown 时，结论应停留在 URL lifecycle span 层级。资源回来
  后的 JS / style / layout 开销看 `chrome_page_load_script_hotspots`；遇到长任务
  `id` 用 `slice_descendants_breakdown` 展开它的子 slice。
- **其他 trace（Android、通用）**——`load_trace` → 先用 `list_stdlib_modules`
  （或读 `resource://perfetto-mcp/stdlib-quickref`）看有没有现成模块（Android、
  `slices.with_context` 这类通用模块），有就用 `execute_sql` + `INCLUDE PERFETTO
  MODULE`。没有合适模块时，再用 `list_tables` / `list_table_structure` 探索
  schema，然后 `execute_sql`。

### 分析多个 trace

用 `load_trace(paths=[...])` 一次加载所有对比目标；响应会为每个文件返回一个稳定的
`trace_id`。对每个 id 调用相同的 trace 工具（MCP 客户端支持并行工具调用时可以并发
执行），再比较各自返回的证据。省略 `trace_id` 仍兼容旧用法，作用于最近一次加载的
trace。如果已加载文件在磁盘上发生变化，旧 id 会被明确拒绝，并提示重新加载。

**隐私**——tool 结果会进入 LLM 上下文，而真实 trace 里可能含 URL、header、
cookie、本地路径。`execute_sql` 和专用 Chrome 工具默认遮蔽这类敏感字符串，同时
保留诊断结构。需要原始取证数据时，在启动服务端前设
`PERFETTO_MCP_REDACT_STRINGS_DEFAULT=false`；`load_trace` 会在摘要里报告当前策略。

**精度**——专用 Chrome 工具默认保留完整字符串单元格。只有在明确想用细节换取更小
响应时，才用 `max_string_len`。

## 内部机制：专用工具 vs. 原始 SQL

上面那个滚动卡顿的问题，最终只对应一次 `chrome_scroll_jank_summary` 调用——
不用写 SQL。需要专用工具没暴露的切片时，agent 才会降级到 `execute_sql` 写
PerfettoSQL；同样的拆解手写出来是这样：

```sql
INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3;
SELECT cause_of_jank, COUNT(*) AS n
FROM chrome_janky_frames
GROUP BY cause_of_jank
ORDER BY n DESC;
```

## 配置项

服务端配置在启动时读取；同一项同时支持命令行参数和环境变量时，命令行参数优先。

| 配置（flag / env） | 默认 | 作用 |
|---|---|---|
| `PERFETTO_TP_PATH` | — | 已有的 `trace_processor_shell` 路径，设了就不自动下载 |
| `--startup-timeout-ms` / `PERFETTO_STARTUP_TIMEOUT_MS` | `20000` | 等待新启动 `trace_processor_shell` 就绪的最长时间（ms） |
| `--query-timeout-ms` / `PERFETTO_QUERY_TIMEOUT_MS` | `30000` | `/status` 和 `/query` 请求的 HTTP 超时（ms） |
| `--max-instances` | `3` | idle LRU 最多保留几个 `trace_processor_shell` 进程；正在查询的实例不会被淘汰 |
| `--max-active-instances` | `10` | 同时处于查询中的 `trace_processor_shell` 实例上限；更多不同 trace 的请求会等待 semaphore permit |
| `--span-timings` / `PERFETTO_MCP_SPAN_TIMINGS` | 关 | 输出 tracing span close 计时，用于性能热点诊断（`1` / `true` / `yes` / `on`） |
| `--artifacts-base-url` / `PERFETTO_ARTIFACTS_BASE_URL` | LUCI bucket | 缓存未命中时覆盖 `trace_processor_shell` 的下载源（镜像/代理；版本仍为固定 pin） |
| `PERFETTO_MCP_REDACT_STRINGS_DEFAULT` | `true` | 遮蔽 tool 输出里 URL/header/cookie/路径的敏感字符串；设 `false` 走原始取证 |
| `PERFETTO_MCP_FULL_TRACE_FINGERPRINT` | 关 | trace 缓存身份使用全文件 SHA-256，而非 head/middle/tail 采样（`1` / `true` / `yes` / `on`） |
| `RUST_LOG` | — | `tracing-subscriber` 日志过滤，例如 `RUST_LOG=debug`（写到 stderr） |

## 安装选项

<details>
<summary>包管理器、Claude scope、直接下载二进制、手动注册</summary>

**包管理器**——不想跑安装脚本的话：

```sh
# macOS / Linux via Homebrew
brew tap tooluse-labs/tap
brew install perfetto-mcp-rs
# brew 会打印一段 caveats；照着跑下面这条注册到 Claude Code / Codex：
perfetto-mcp-rs install --binary-path "$(brew --prefix)/bin/perfetto-mcp-rs"

# Rust 开发者用 cargo
cargo install --locked perfetto-mcp-rs
perfetto-mcp-rs install --binary-path "$(which perfetto-mcp-rs)"
```

脚本安装时如果检测到 Qoder，会打印一段可直接粘贴的 JSON（Qoder 暂无程序化注册
MCP 的 API——手动打开 Qoder Settings → MCP → + Add 粘贴）。

**Claude scope**——默认按 `--scope user` 注册（任意目录可见）。要装成项目本地
（`local` / `project`），把 `SCOPE=local` 带上，并**在目标项目目录里**运行脚本：

```sh
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh'
```

PowerShell 写法：`$env:SCOPE = 'local'; irm ... | iex`。Codex 没有 scope 概念，
会忽略这个变量。

**直接下载二进制**——支持平台：linux amd64/arm64、macOS amd64/arm64、Windows
amd64。到 [releases 页面](https://github.com/tooluse-labs/perfetto-mcp-rs/releases)
下载对应平台二进制。资产名形如 `perfetto-mcp-rs-<platform>`（例如
`perfetto-mcp-rs-linux-amd64`），调用 `install` 时显式指定路径，且 **Unix 上先
`chmod +x`**——`install` 子命令会拒绝没有执行位的路径，避免写入启动不起来的 MCP
条目。示例：

```sh
# Linux amd64 示例——其它平台替换资产名。
curl -fsSL -o perfetto-mcp-rs \
  https://github.com/tooluse-labs/perfetto-mcp-rs/releases/latest/download/perfetto-mcp-rs-linux-amd64
chmod +x perfetto-mcp-rs
./perfetto-mcp-rs install --scope user --binary-path "$PWD/perfetto-mcp-rs"
```

**手动配置 MCP 客户端**——安装脚本没帮你自动注册时。

Codex：

```sh
codex mcp add perfetto-rs -- /absolute/path/to/perfetto-mcp-rs
```

基于 JSON 配置的客户端（Claude Code、Claude Desktop、Cursor 等）：

```json
{
  "mcpServers": {
    "perfetto-rs": {
      "command": "/absolute/path/to/perfetto-mcp-rs"
    }
  }
}
```

</details>

## 升级与卸载

<details>
<summary>升级、锁定版本、检查更新、卸载</summary>

**升级**——重跑一遍安装命令即可。脚本会拉取最新 release，安全覆盖原有二进制
（Windows 下带文件锁重试），并幂等地重新注册到 Claude Code / Codex。没有后台
自动更新——什么时候升级完全由你决定。

要锁定具体版本，用 `--version` flag：

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh -s -- --version v0.7.0
```

也可用 `VERSION` 环境变量，但**必须紧挨着 `sh` 写**——POSIX 的 `VAR=value cmd`
只把变量传给紧跟的那条命令，写成 `VERSION=v0.7.0 curl ... | sh` 实际是把
`VERSION` 给了 `curl`，管道后面的 `sh` 拿不到：

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | VERSION=v0.7.0 sh
```

PowerShell 把 `$env:VERSION` 写在同一行即可，`iex` 在当前 session 里执行能读到：

```powershell
$env:VERSION = 'v0.7.0'; irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

**检查更新：**

```sh
perfetto-mcp-rs check-update
```

退出码：已是最新（或本地是开发版，超前于 release）返回 0；有新版本返回 2；
网络或解析出错返回 1。适合放到 shell 提示符集成或 CI 预检里。

**卸载**——和安装对称的一键脚本，会从 Claude Code 和 Codex 注销、删除二进制、
清空缓存的 `trace_processor_shell`。幂等设计——之前手动清过一部分也能安全重跑。

```sh
# Linux / macOS / Windows（Git Bash、MSYS2、Cygwin）
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh
```
```powershell
# Windows（PowerShell）——先关掉 Claude Code、Codex 或任何正在占用 .exe 的进程
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.ps1 | iex
```

**Scoped 安装（local / project）**——Claude 的 local/project 注册按项目目录索引，
所以卸载时必须沿用同一个 `SCOPE`，并**回到原来那个项目目录里执行**。漏掉这步的
话，wrapper 会照常删二进制和缓存，但 Claude 里的 scoped 注册条目会留下：

```sh
# 之前在 ~/work/foo 跑过 `SCOPE=local bash install.sh`？回到那个目录再卸载：
cd ~/work/foo
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh'
```

PowerShell 写法：`cd <原项目目录>; $env:SCOPE = 'local'; irm ... | iex`。

`$INSTALL_DIR`（默认 `~/.local/bin`）**不会**被自动从 PATH 中移除：

- **Linux / macOS**——安装脚本只是*提示*你把它加进 `PATH`；如果照做了，需自己回
  shell rc 里删掉那行。
- **Windows**——安装脚本是*真的把* `$INSTALL_DIR` 写入了用户 PATH
  （HKCU\Environment）；要清掉走 系统属性 → 环境变量。

这个目录里可能还放着别的工具，所以卸载脚本不会主动动它。

</details>

## 从源码构建

<details>
<summary>protoc、cargo build、测试</summary>

需要 Rust 工具链和 `protoc`（Protocol Buffers 编译器）：

```sh
# Ubuntu/Debian
sudo apt install -y protobuf-compiler
# macOS
brew install protobuf
# Windows
choco install protoc
```

然后：

```sh
git clone https://github.com/tooluse-labs/perfetto-mcp-rs
cd perfetto-mcp-rs
cargo build --release
# 二进制在 target/release/perfetto-mcp-rs
```

开发：

```sh
cargo test          # 跑测试
cargo clippy        # lint
cargo fmt           # 格式化
```

</details>

## 许可证

双协议授权：[Apache 2.0](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-APACHE) 或 [MIT](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-MIT)，任选其一
即可。向本仓库提交的代码默认按同样的双协议发布。
