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
客户端）里指向一个 Perfetto trace 文件（`.pftrace` / `.perfetto-trace` /
`.bin` 等格式都行——按内容嗅探），直接用 PerfettoSQL 查询分析。

底层依赖 `trace_processor_shell`，首次运行自动下载，无需手动安装 Perfetto。

最适合搭配支持多轮工具调用的 agentic MCP 客户端使用（Claude Code、Codex、
Claude Desktop、Cursor 等）。这类客户端会顺着错误消息里的提示，自动走完
`load_trace` → `list_tables` → `list_table_structure` → `execute_sql` 这套
常规流程。非 agentic 客户端虽然也能看到全部工具，但拿不到这些引导性提示
带来的便利。

> 帮 agent 找到对的 PerfettoSQL stdlib 模块——分析 SQL 始终由 agent 自己来写。

## 快速安装

**一键脚本（所有平台首选）**——下载预编译二进制，放进 `~/.local/bin`
（Windows 是 `%USERPROFILE%\.local\bin`），需要的话顺手把它加进用户
PATH；如果系统里已经装了 Claude Code 或 Codex，还会自动完成注册。重启
Claude Code，或新开一个 Codex session，就能用上。如果检测到 Qoder，会
打印一段可直接粘贴的 JSON（Qoder 暂无程序化注册 MCP 的 API，需手动打开
Qoder Settings → MCP → + Add 粘贴）。

Linux / macOS / Windows（Git Bash、MSYS2、Cygwin）：

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh
```

Windows（PowerShell）：

```powershell
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

**也可以用包管理器装**：

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

**Claude scope**：默认按 `--scope user` 注册（在任意目录都可见）。如果想
装成项目本地（`local` / `project` scope），把 `SCOPE=local` 带上，并
**在目标项目目录里**运行脚本：

```sh
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh'
```

PowerShell 写法：`$env:SCOPE = 'local'; irm ... | iex`。Codex 没有 scope
概念，会自动忽略这个变量。

支持平台：linux amd64/arm64、macOS amd64/arm64、Windows amd64。
不想跑脚本，也可以直接到 [releases 页面](https://github.com/tooluse-labs/perfetto-mcp-rs/releases)
下载对应平台的二进制。Release 资产名形如 `perfetto-mcp-rs-<platform>`
（例如 `perfetto-mcp-rs-linux-amd64`），下载后 **Unix 上记得先 `chmod +x`**
——`install` 子命令会拒绝没有执行位的路径，避免写入一个根本启动不起来的
MCP 条目。示例：

```sh
# Linux amd64 示例 —— 其它平台替换资产名。
curl -fsSL -o perfetto-mcp-rs \
  https://github.com/tooluse-labs/perfetto-mcp-rs/releases/latest/download/perfetto-mcp-rs-linux-amd64
chmod +x perfetto-mcp-rs
./perfetto-mcp-rs install --scope user --binary-path "$PWD/perfetto-mcp-rs"
```

## 升级

重跑一遍安装命令即可——脚本会拉取最新 release，安全地覆盖原有二进制
（Windows 下带文件锁重试），并幂等地重新注册到 Claude Code / Codex。

要锁定到具体版本，推荐用 `--version` flag（顺便避开 shell 管道的环境变量
陷阱）：

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh -s -- --version v0.7.0
```

也可以用 `VERSION` 环境变量，但**必须紧挨着 `sh` 写**——POSIX 的
`VAR=value cmd` 只把变量传给紧跟的那条命令，写成
`VERSION=v0.7.0 curl ... | sh` 实际是把 `VERSION` 给了 `curl`，管道后面的
`sh` 拿不到：

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | VERSION=v0.7.0 sh
```

PowerShell 把 `$env:VERSION` 写在同一行就行，`iex` 在当前 session 里执行
能直接读到：

```powershell
$env:VERSION = 'v0.7.0'; irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

不会有后台自动更新——什么时候升级完全由你决定。

## 检查更新

```sh
perfetto-mcp-rs check-update
```

退出码：当前已是最新（或本地是开发版，超前于 release）返回 0；有新版本
返回 2；网络或解析出错返回 1。适合放到 shell 提示符集成或 CI 预检里。

## 卸载

和安装对称的一键脚本，会从 Claude Code 和 Codex 注销、删除二进制、清空
缓存的 `trace_processor_shell`。幂等设计——之前手动清过一部分也能安全
重跑。

**Linux / macOS / Windows（Git Bash、MSYS2、Cygwin）：**

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh
```

**Windows（PowerShell）—— 先关掉 Claude Code、Codex 或任何正在占用 `.exe` 的进程：**

```powershell
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.ps1 | iex
```

**Scoped 安装（local / project）**：Claude 的 local/project 注册是按项目目录
索引的，所以卸载时必须沿用同一个 `SCOPE`，并且**回到原来那个项目目录里
执行**。漏掉这一步的话，wrapper 会照常删掉二进制和缓存，但 Claude 里的
scoped 注册条目会留下不被清理：

```sh
# 之前在 ~/work/foo 跑过 `SCOPE=local bash install.sh`？回到那个目录再卸载：
cd ~/work/foo
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh'
```

PowerShell 写法：`cd <原项目目录>; $env:SCOPE = 'local'; irm ... | iex`。

`$INSTALL_DIR`（默认 `~/.local/bin`）**不会**被自动从 PATH 中移除：

- **Linux / macOS**——安装脚本只是*提示*你把它加进 `PATH`；如果你当时照做
  了，需要自己回 shell rc 里删掉那行。
- **Windows**——安装脚本是*真的把* `$INSTALL_DIR` 写入了用户 PATH
  （HKCU\Environment）；要清掉请走 系统属性 → 环境变量。

这个目录里可能还放着别的工具，所以卸载脚本不会主动动它。

## 工具

| 工具 | 用途 |
|---|---|
| `load_trace` | 打开一个 Perfetto trace 文件，并返回轻量路由摘要（类型/profile、时长、平台、进程/线程数、能力标签、当前脱敏策略、推荐下一步工具） |
| `list_tables` | 列出 trace 里的表和视图，支持 GLOB 过滤 |
| `list_table_structure` | 查看某张表的列名和类型 |
| `execute_sql` | 执行 PerfettoSQL 查询（最多 5000 行）；可选输出塑形支持 `head`/`limit`、`summary`、`columns_only`、`include_row_count`、`max_string_len`；服务端默认会对 URL/header/cookie/路径中的敏感值做隐私遮蔽 |
| `list_processes` | 列出 trace 里的进程（pid、名称、起止时间戳） |
| `list_threads_in_process` | 列出指定进程名下的线程（最多 2000 条） |
| `chrome_scroll_jank_summary` | 按原因汇总最严重的 Chrome 滚动卡顿帧；元信息标记行/字符串是否截断（仅 Chrome trace） |
| `chrome_page_load_summary` | 页面加载的 URL / FCP / LCP / DCL / load 耗时；元信息标记行/字符串是否截断（仅 Chrome trace） |
| `chrome_main_thread_hotspots` | 主线程任务按耗时排序，带 ts 和 cpu_pct；元信息标记行/字符串是否截断（仅 Chrome trace） |
| `chrome_startup_summary` | 浏览器启动事件与首次可见内容时间；元信息标记行/字符串是否截断（仅 Chrome trace） |
| `chrome_web_content_interactions` | Web 内容交互（点击、触摸、INP）按耗时排序；元信息标记行/字符串是否截断（仅 Chrome trace） |
| `list_stdlib_modules` | 列出 PerfettoSQL stdlib 模块及用法示例（不需要先加载 trace） |

隐私提示：MCP tool 的结果通常会进入 LLM 上下文，而真实 trace 里可能包含 URL、header、cookie、本地路径等个人或凭据相关信息。`execute_sql` 和专用 Chrome 工具默认遮蔽这类敏感字符串，同时保留诊断结构。需要原始取证数据时，可在启动服务端前设置 `PERFETTO_MCP_REDACT_STRINGS_DEFAULT=false`；`load_trace` 会在摘要里报告当前策略。

典型流程按 trace 类型走：

- **Chrome trace**：`load_trace` → 直接用专用的 `chrome_*` 工具
  （`chrome_scroll_jank_summary`、`chrome_page_load_summary`、
  `chrome_main_thread_hotspots`、`chrome_startup_summary`、
  `chrome_web_content_interactions`），要深入分析时再用 `execute_sql`
  对返回的行做下一步查询。
- **其他 trace**：`load_trace` → 用 `list_tables` / `list_table_structure`
  探索 schema → `execute_sql` 查询。如果分析涉及到 stdlib 模块（Android、
  `slices.with_context` 这类通用模块），可以调 `list_stdlib_modules` 辅助。

## 示例

可以这样问 Claude Code 或 Codex：

> 加载 `~/traces/scroll_jank.pftrace`，告诉我滚动卡顿的主要原因。

Claude 会先调 `load_trace`，然后发一条类似这样的查询：

```sql
INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3;
SELECT cause_of_jank, COUNT(*) AS n
FROM chrome_janky_frames
GROUP BY cause_of_jank
ORDER BY n DESC;
```

## 手动配置 MCP 客户端

如果安装脚本没帮你自动注册：

**Codex：**

```sh
codex mcp add perfetto-rs -- /absolute/path/to/perfetto-mcp-rs
```

**基于 JSON 配置的客户端（比如 Claude Code、Claude Desktop、Cursor）：**

```json
{
  "mcpServers": {
    "perfetto-rs": {
      "command": "/absolute/path/to/perfetto-mcp-rs"
    }
  }
}
```

## 配置项

| 环境变量 | 作用 |
|---|---|
| `PERFETTO_TP_PATH` | 已有的 `trace_processor_shell` 路径，设了就不自动下载 |
| `PERFETTO_STARTUP_TIMEOUT_MS` | 覆盖 `trace_processor_shell` 启动超时，单位毫秒 |
| `PERFETTO_QUERY_TIMEOUT_MS` | 覆盖 `/status` 和 `/query` 的 HTTP 超时，单位毫秒 |
| `RUST_LOG` | `tracing-subscriber` 日志过滤，例如 `RUST_LOG=debug` 打开详细日志（写到 stderr） |

命令行参数：

| 参数 | 默认 | 说明 |
|---|---|---|
| `--max-instances` | 3 | 最多缓存几个 `trace_processor_shell` 进程，超过按 LRU 淘汰 |
| `--startup-timeout-ms` | 20000 | 等待新启动 `trace_processor_shell` 就绪的最长时间 |
| `--query-timeout-ms` | 30000 | `/status` 和 `/query` 请求的 HTTP 超时 |

## 从源码构建

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

## 开发

```sh
cargo test          # 跑测试
cargo clippy        # lint
cargo fmt           # 格式化
```

## 许可证

双协议授权：[Apache 2.0](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-APACHE) 或 [MIT](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-MIT)，任选其一
即可。向本仓库提交的代码默认按同样的双协议发布。
