# ROADMAP

Last updated: 2026-04-16

面向 `perfetto-mcp-rs` 的下一阶段执行清单。目标不是继续堆功能，而是先补齐正确性边界、测试防回归能力，以及高价值分析工具。

> 这是一个意图快照，不是进度看板。逐条任务的跟踪建议使用 GitHub issues，避免本文件里的 `- [ ]` 状态和外部 tracker 各自漂移。

## Principles

- 先修正确性和运行时稳定性，再扩功能
- 先补测试缝隙，再做重构和表面优化
- 优先投资“未来会反复复用”的基础能力
- 区分 correctness bug、runtime hardening 和 feature expansion

## Priority Order

v0.2 已落地正确性修复、回归测试、错误模型收敛和下载硬化。当前优先级针对 v0.3 及之后：

1. 落地 `stdlib-quickref` MCP Resource（若缺失，agent 会退化为 `LIKE '%xxx%'` 扫描，后续所有领域工具的收益都会打折）
2. 落地 `list_stdlib_modules`（列举能力，依赖 stdlib 的 M5 工具都建立在它之上）
3. 落地 Milestone 5 中的 Chrome / Android / CPU 领域工具
4. 待 v0.3 到用户手上后，再推进 Milestone 6 的 `execute_sql` summary 模式和 schema 缓存

## Milestone 1: Correctness And Runtime Hardening

目标：先消除最可能导致“结果不可信”或“长时间运行不稳”的问题。

- [x] 修复 `wait_ready` 的实例身份校验
  - 现状：`/status` 可能误判到外部进程
  - 建议实现：基于 `stderr` 启动标记做 readiness gate，再进入 `/status` 轮询
  - 验收：端口预占用场景下不会把外部进程识别为自己的实例

- [x] 持续消费并记录子进程 `stderr`
  - 现状：`stderr` 已 `piped()`，但没有后台 drain
  - 目标：避免 pipe 堵塞，同时提升启动失败可诊断性
  - 验收：启动失败日志可见，长时间运行无 pipe 写满风险

- [x] 为启动超时和查询超时增加可配置项
  - 现状：超时策略写死
  - 目标：支持 CLI flag 或 env
  - 验收：能显式配置启动超时和 HTTP 查询超时

- [x] 评估是否对同一路径并发 spawn 做串行化
  - 现状：同一 trace 并发 `get_client` 会重复 spawn，再丢掉一个实例
  - 目标：避免无意义重复启动
  - 验收：并发同一路径请求时，最多只启动一个实例

## Milestone 2: Test Hardening

目标：把“现在能跑”提升为“后续不容易回归”。

- [x] 为自定义 `starting_port` 回绕补纯单元测试
  - 目标：验证回绕后回到 `starting_port`
  - 建议：提取纯端口分配逻辑，便于边界测试
  - 验收：覆盖 `u16::MAX -> starting_port`

- [x] 为 LRU 淘汰与实例复用补测试
  - 验收：超过容量后只淘汰最旧未使用实例

- [x] 为子进程异常退出后的自动恢复补测试
  - 验收：旧实例退出后，下一次查询可恢复

- [x] 增加同一 trace / 不同 trace 的并发访问测试
  - 验收：不死锁、不误复用、不重复 spawn

- [x] 增加失败路径测试
  - 场景：trace 不存在、binary 不可执行、下载失败、端口冲突
  - 已落地：trace 不存在（`get_client_returns_clear_error_for_missing_trace`）、binary 不可执行（Unix-only `get_client_surfaces_spawn_error_for_non_executable_binary`，通过 `new_with_binary` 注入 `0o644` 文件）、下载 HTTP 失败（`download_binary_surfaces_http_5xx_status` 用本地 500 响应器驱动 `error_for_status` 分支，同时再次验证 URL 脱敏）、端口冲突（`preflight_port_free_rejects_real_bound_listener` + `allocate_next_port_skips_real_bound_listener` 用真实 bound listener 驱动真实 probe）
  - 验收：错误消息清晰且可定位

- [x] 为 server 层提示逻辑补回归测试
  - 现状：依赖 `msg.contains(...)`
  - 目标：锁定当前“missing table / missing module”提示行为
  - 验收：错误文案或分类变化时测试会报警

- [x] 扩充 e2e fixture
  - 现状：单个 smoke fixture 只能证明主链路成立
  - 已落地（随后在 v0.6 pivot 中部分调整）：初始 ship 了 `scroll_jank.pftrace`、`page_loads.pftrace`、`event_latency.perfetto-trace`、`histogram.perfetto-trace`，以及用 `chrome_scroll_jank_summary` 真实 SQL 端到端驱动 `scroll_jank.pftrace` 的 `tests/e2e_chrome_scroll_jank.rs`。v0.6 后，`page_loads.pftrace` 和 Chrome 域工具测试被移除；`scroll_jank.pftrace` 保留，现由 `tests/e2e_stdlib_include.rs::e2e_stdlib_include_chrome_scroll_jank` 继续跑 `chrome.scroll_jank.scroll_jank_v3` → `chrome_janky_frames`（即 `README.md` 推荐给用户的迁移 SQL）。
  - 验收：领域工具有代表性 e2e 覆盖（M2 关闭时达成；v0.6 重新分配覆盖，Chrome stdlib 路径未中断）

## Milestone 3: Error Model Tightening

目标：减少脆弱的字符串匹配，提高提示逻辑稳定性和可测试性。

- [x] 设计 `QueryErrorKind`
  - 已落地：`MissingTable`、`MissingModule`、`Other`（标注 `#[non_exhaustive]`；`SyntaxError` 等到首个消费者出现时再加）
  - 验收：形成清晰枚举，不破坏原始错误显示

- [x] 在更靠近 client/decode 的层面做错误分类
  - 分类统一发生在 `decode_query_result`；两个 server 提示 formatter 改为对 `QueryErrorKind` 的穷尽 match
  - 验收：提示逻辑改为 match 枚举

- [x] 清理或收敛未使用错误语义
  - `PerfettoError::NoTraceLoaded` 已删除；`QueryError` 重构为 struct variant `{ kind, message }`
  - 验收：错误枚举和实际语义一致

## Milestone 4: Download And Distribution Hardening

目标：让安装、升级和缓存恢复更稳。

- [x] 将下载逻辑改为“临时文件 + 原子重命名”
  - 已落地：`NamedTempFile::new_in(cache_dir)` 流式写入 + `persist` 原子 rename；Windows 下 `PermissionDenied` 做 5 次退避重试抵御 AV 占用句柄；单次下载挂 10 分钟 wall-clock 上限
  - 验收：下载中断不会污染缓存

- [x] 增加 checksum 或等效校验
  - 已落地：下载时流式计算 SHA-256，写入 `trace_processor_shell.sha256` sidecar；缓存命中时重新校验，不一致即重下；pre-sidecar 缓存在本地原地哈希自愈以支持气隙环境升级
  - 验收：损坏 binary 可被识别并重新下载

- [x] 增加下载源 / 镜像配置能力
  - 已落地：`--artifacts-base-url` / `PERFETTO_ARTIFACTS_BASE_URL` 贯通到 `DownloadConfig`；日志与错误链中 userinfo/query 经 `redact_url` + `reqwest::Error::without_url` 剥离，镜像 token 不泄露
  - 验收：网络受限环境可切换源

- [x] 增强跨平台 CI 覆盖
  - 已落地：CI 拆成 `lint` + `test`；test 跑 `[ubuntu, macos, windows]` 矩阵 `fail-fast: false`，刻意不缓存 `trace_processor_shell`，每次 PR × OS 都冷跑完整下载路径
  - 至少覆盖：Linux、macOS、Windows
  - 验收：构建、测试、release 资产一致可用

## Milestone 5: Productized Analysis Tools

目标：从“通用 SQL 执行器”升级为“常见 Perfetto 场景分析工具”。

### Conventions

- **工具命名**：通用工具使用 `{verb}_{noun}`（`list_*`、`load_*`、`execute_*`）；分析工具使用 `{domain}_{metric}_summary`；`_suspects` / `_hotspots` / `_breakdown` 在 `_summary` 不贴切时可接受。
- **描述规范**：每个 M5 新增工具描述包含一句 "USE THIS WHEN"（agent 何时该选它）和一句 "NEXT STEPS"（调完之后该做什么）。借鉴自 antarikshc/perfetto-mcp，按 single-signal 规则保持精简。
- **Fixture 来源**：Android 样本从 `chromium/.../third_party/perfetto/test/data/` 取，该目录 GCS-backed——`.sha256` 指针在树里，二进制通过 `https://storage.googleapis.com/perfetto/test_data/{filename}-{digest}` 公开分发。按工具落地时再单独拷贝，避免仓库膨胀。

### Foundation

- [x] 落地 `list_stdlib_modules`（v0.7.0 已发布）
  - 返回 10 个 stdlib 模块的 JSON 数组（chrome / android / generic），含模块名、views、description、示例 usage SQL
  - 无参数，可在 load_trace 前调用，定位为辅助发现工具（覆盖专用 `chrome_*` 工具之外的场景）

### Chrome Tools

> **v0.7 状态：五个 Chrome 域工具已落地，形态为 row-preserving thin wrapper。**
> v0.5 时尝试过预聚合的"答案形"工具；v0.6 因答案形批评撤回；v0.7 恢复工具
> 但改为行级输出，agent 可在工具返回的行上继续做 group / filter / correlate。
> 设计原则：
>
> 1. **精选展示，不是纯 SELECT \***：工具选定常用排序 + LIMIT + 派生列
>    （ms 单位换算），有主张但不锁死分析。
> 2. **保持行级，不预聚合**：工具 SQL 里没有 `GROUP BY`——聚合由 agent
>    决定。
> 3. **稳定公开 stdlib 子集**：只暴露经 vendored Perfetto stdlib 源码
>    验证的 view。
>
> v0.7.0 已发布（均需要 Chrome trace）：
>
> - [x] `chrome_scroll_jank_summary` — 行级 `chrome_janky_frames`
>       （cause、sub_cause、delay_since_last_frame、event_latency_id、
>       scroll_id、vsync_interval），按 delay 降序取 100
> - [x] `chrome_page_load_summary` — 每个 navigation 的
>       FCP / LCP / DCL / load（`chrome.page_loads`）
> - [x] `chrome_main_thread_hotspots` — 主线程任务 > 16ms 含 cpu_pct，
>       用 `thread.is_main_thread = 1`（caveat：线程 metadata 缺失时可能为空）
> - [x] `chrome_startup_summary` — 启动事件与首次可见内容时间
>       （`chrome.startups`）
> - [x] `chrome_web_content_interactions` — 点击/触摸/键盘按耗时排序，
>       用于 INP 分析

- [ ] 新增 `chrome_frame_timeline_summary`
  - 基于 stdlib 的 frame-timeline / jank 聚合——模块名须查外部 Chromium checkout `~/chromium/src/third_party/perfetto/src/trace_processor/perfetto_sql/stdlib/chrome/`（见 `docs/plans/m5-stdlib-quickref-resource.md:439`），`chrome.frame_times` 未经验证；已知相关表名包括 `expected_frame_timeline_slice` / `actual_frame_timeline_slice`
  - 验收：按 v0.7 设计原则，行级 thin wrapper
  - Fixture：复用 `tests/fixtures/scroll_jank.pftrace`

- [ ] 新增 `chrome_blocking_calls_summary`
  - 汇总 `ScopedBlockingCallWithBaseSyncPrimitives`（Chrome 同步 IO / 同步等待埋点）——按线程、进程、频次、累计阻塞时间排序。实测 session 中 Worker / I/O 线程上出现 15K+ 次，是 file-mapping 和字体加载卡顿的直接来源。
  - 验收：给出排序结果，并明确标记哪些线程上阻塞可接受（Utility、ThreadPool\*），哪些线程上阻塞是延迟来源（Worker、Renderer）
  - Fixture：自录一个带同步文件流量的 Chrome trace；或在 `trace_file_mapping_small_file` 的脱敏副本就绪后复用

### Android Tools

- [ ] 新增 `android_startup_summary`
  - 聚焦冷启动 / 温启动关键阶段
  - 验收：给出启动总耗时与主要阶段拆解
  - Fixture：`api31_startup_cold.perfetto-trace`（体积小、可复现，冷启动信号最干净）

- [ ] 新增 `anr_suspects`
  - 单次产出主线程卡顿、binder 等待、锁竞争的怀疑对象排序；多信号根因关联留到后续 milestone
  - 验收：给出排名后的怀疑对象，而不是原始表
  - Fixture：`android_anr.pftrace.gz`

- [ ] 新增 `list_macrobenchmark_slices` **(fixture blocked)**
  - 列出 `measureBlock` slice 并关联 app / test，对标 `com.google.PerfettoMcp` 的 `perfetto-list-macrobenchmark-slices`
  - 验收：输出结构与上游工具一致
  - Fixture：chromium 树无现成样本，需要自录或取自 AndroidX Benchmark。fixture 未就绪前，v0.3 不可执行。

### Thread-Level Tools（跨平台）

- [ ] 新增 `main_thread_hotspots`
  - 按进程列出主线程 top-N 最长 slice；ANR / jank 排查的常规起点。凡是有 thread track 的 trace 都适用（Android、Chrome、纯 Linux），不局限于 Android。
  - 验收：排名 slice 列表，包含进程、时长、时间戳
  - Fixture：任一 Android 启动 trace（复用 `api31_startup_cold.perfetto-trace`）

### CPU / Memory Tools

- [ ] 新增 `cpu_hot_threads`
  - 高 CPU 占用线程及其所属进程
  - 验收：适用于常见 Android / Linux trace
  - Fixture：`android_sched_and_ps.pb` 或 `example_android_trace_30s.pb`

- [ ] 新增 `process_cpu_breakdown`
  - 进程级 CPU 时间分布，补齐 `cpu_hot_threads`
  - 验收：最重进程排在前面
  - Fixture：同 `cpu_hot_threads`

- [ ] 新增 `memory_growth_summary`
  - 内存增长显著的进程或计数器
  - 验收：用作内存异常粗筛
  - Fixture：通用 Android trace（无专用候选；stretch-goal）

### Supplementary Tools (v0.3 可选)

- [ ] 新增 `thread_contention_summary`
  - 汇总 `monitor_contention` 事件——Android ANR / jank 的头号根因
  - 验收：排名 contention 事件，含 holder / waiter 详情
  - Fixture：`android_monitor_contention_trace.atr`

- [ ] 新增 `binder_transaction_summary`
  - 按接口统计 binder IPC 延迟和事务数
  - 验收：客户端 / 服务端延迟百分位
  - Fixture：`android_binder_metric_trace.atr`

### MCP Resource

- [x] 以 MCP Resource 形式暴露 stdlib 速查表 **(已在 v0.15.6-dev 落地)**
  - URI：`resource://perfetto-mcp/stdlib-quickref`
  - 精选最实用的 stdlib 模块，附一句话领域提示；与 `list_stdlib_modules` 互补——tool 负责枚举，resource 负责教学
  - 灵感来自 antarikshc/perfetto-mcp 的 MCP Resources 模式
  - 为什么 P0：实测 session 反复出现——agent 不知道 stdlib 模块的存在时，会退化为 `SELECT DISTINCT cat FROM slice` + `LIKE '%xxx%'` 扫描。这个 resource 是后续所有 M5 领域工具的放大器。
  - 验收：agent 无需 `execute_sql` 即可获取

## Milestone 6: Performance And Context Efficiency

目标：提升大 trace 和复杂 agent 工作流下的使用体验。

- [ ] 为 `execute_sql` 增加 limit / summary 模式
  - 目标：减少上下文消耗
  - 验收：可返回列信息、前 N 行、总行数或摘要

- [ ] 评估分页或流式结果输出
  - 现状：结果先完整解码为 `Vec<Value>`
  - 验收：形成明确方案，决定保留硬上限还是升级到分页/流式

- [ ] 为高频 schema 查询加缓存
  - 场景：`list_tables`、`list_table_structure`
  - 验收：重复查询减少不必要 RPC

- [ ] 支持 query cancellation
  - 场景：LLM 发起低质量长查询
  - 验收：长查询可中断，不必等待超时

- [ ] 增加 tracing spans
  - 目标：让慢查询和高频调用更易诊断
  - 验收：可观测 `sql_len`、耗时、row_count 等关键信息

## Suggested Release Plan

### v0.2

聚焦稳定性和正确性。

- [x] 完成 `Milestone 1`
- [x] 完成 `Milestone 2` 中最关键的回归测试
- [x] 至少完成 `Milestone 3` 的测试补强或初步分类方案
- [x] 完成 `Milestone 4`

发布门槛：

- [x] 启动、查询、回收主链路无已知高优先级 correctness bug
- [x] 单元测试稳定
- [x] e2e 在 CI 环境稳定
- [x] 关键错误提示有测试覆盖

### v0.3

聚焦高价值分析能力和工具自发现性。

- [ ] 落地 `stdlib-quickref` MCP Resource（P0——所有领域工具都依赖 agent 能看到应该 `INCLUDE` 哪些 stdlib 模块）
- [ ] 落地 `list_stdlib_modules`（后续领域工具的列举基础）
- [ ] 落地 `Milestone 5` 中至少 3 个领域工具，覆盖至少 2 个场景族（Chrome / Android 启动 / ANR / CPU / 内存）
- [ ] 为每个新增工具补 fixture 和 e2e，fixture 来自 `test/data/` 的 GCS 指针
- [ ] 对工具描述按 `USE THIS WHEN` / `NEXT STEPS` 规范做一轮统一

发布门槛：

- [ ] `stdlib-quickref` resource 可获取，至少覆盖 Chrome 和 Android 的 stdlib 入口
- [ ] 至少 3 个新增领域工具覆盖至少 2 个场景族
- [ ] README 补齐新增工具使用示例

### v1.0

聚焦“稳定可发布”。

- [ ] 完成关键运行时硬化
- [ ] 完成关键回归测试矩阵
- [ ] 完成跨平台分发与诊断文档
- [ ] 明确版本升级与兼容性策略

发布门槛：

- [ ] 核心行为有防回归测试
- [ ] 安装与升级路径清晰
- [ ] 常见故障可诊断
- [ ] 面向典型 Perfetto 使用场景具备足够覆盖

## Reference

- 英文版本：`docs/roadmap.md`
