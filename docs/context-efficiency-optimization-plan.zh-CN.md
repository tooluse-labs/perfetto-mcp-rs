# perfetto-mcp-rs 上下文与 Token 效率优化方案

Last updated: 2026-05-26

## 背景

`perfetto-mcp-rs` 的当前方向是正确的：保持较小 MCP 工具面，用 `load_trace`、`execute_sql`、schema 探索工具和少量高频 Chrome domain tools 覆盖主要分析路径。真正的 token 风险不在工具数量本身，而在三类成本：

1. MCP 客户端可能把 `tools/list` 中的工具描述和参数 schema 注入模型上下文。
2. `ServerInfo.instructions` 和长工具 description 会变成常驻教学文本。
3. PerfettoSQL 查询结果可能一次返回大量行，结果 token 成本高于工具 schema 成本。

本方案目标是在不降低 agent 分析能力的前提下，把“常驻上下文”压缩为路由信息，把“教学与细节”改为按需获取，把“大结果集”改为可塑形输出。

## 目标

- 降低默认 MCP 工具注入和 instructions 的上下文占用。
- 保持现有调用成功率：agent 仍能发现 stdlib、修正 SQL、定位 trace schema。
- 控制 `execute_sql` 返回体，避免无意 dump 大量原始行。
- 减少反复 schema 探索带来的 trace_processor RPC 成本。
- 建立预算测试，防止后续功能扩展悄悄抬高上下文成本。

## 非目标

- 不把每个 Perfetto stdlib view 都做成独立 MCP 工具。
- 不移除 `execute_sql` 这个通用逃生口。
- 不为了省 token 删除错误路径上的诊断 hint；错误 hint 是按需出现的高价值 token。
- 不引入依赖复杂的自然语言总结器；summary 应以确定性结构化摘要为主。

## 当前观察

- 工具数量约十几个，尚未膨胀，继续保持克制即可。
- `STDLIB_INSTRUCTIONS` 和 `execute_sql` description 承载了大量教学内容，能提升 agent 行为，但可能成为固定上下文成本。
- `execute_sql` 已有 `MAX_ROWS = 5000` 的硬上限，但缺少 `head`、`count`、`columns_only`、`summary` 等结果塑形模式。
- roadmap 中已有 Milestone 6：`execute_sql` summary、schema cache、query cancellation，与本方案一致。

## 真实会话观察：`trace5.json`

样本：用户请求“分析性能问题”，agent 使用 `perfetto-rs` 分析
`C:\Users\admin3\Documents\trace5.json`，日志保存在
`C:\Users\admin3\Documents\log`。该 trace 约 204.5 秒、Chrome 130、
24 个进程、289 个线程。

有效信号：

- `load_trace` summary 和 `recommended_next_tools` 起效。agent 先调用
  `chrome_page_load_summary`、`chrome_scroll_jank_summary`、
  `chrome_web_content_interactions`、`chrome_main_thread_hotspots`，没有直接
  退回盲目扫 `slice`。
- Chrome domain tools 给出了正确的一层判断：页面
  `https://www.qianwen.com/chat` 的 FCP / load 约 10.7 秒；没有滚动卡顿和
  Web content interaction；Renderer 主线程存在 500ms 级长任务。
- 后续自定义 SQL 能定位到有价值根因：关键 JS 约导航后 3 秒发起，但
  `EnhanceConfigManager::GetResource` / URL request 相关链路卡住约
  5.8-6.35 秒，之后才进入 V8 解析和执行。

暴露的问题：

- 多处工具结果在会话 UI 中被省略成 `...`，尤其是 `list_table_structure`、
  大 rows、`args.display_value`、headers / URL / 本地路径等长字符串。LLM
  仍能推进，但实际上是在不完整结果上推理。
- 对 `trace5.json` 复跑确认：日志里的 Chrome 专用工具省略不是 server 默认
  字符串截断，而是客户端显示层裁剪；当时返回体里也没有 `string_truncated`
  元数据。输出塑形需要把这类不可观测裁剪前移为 server 端可标记状态。
- agent 反复手写 `WITH RECURSIVE descendants...` 来展开 slice 子树，并出现
  一次 `ambiguous column name: depth`。错误被自动修复，但这是稳定、可工具化
  的重复模式。
- 从 `chrome_page_load_summary` 到“哪个资源或导航阶段卡住”仍需要大量手写
  SQL。已新增 `chrome_page_load_resource_hotspots` 将 URL-bearing
  resource-like slice 按 page-load/raw timestamp 窗口 overlap 排序，覆盖
  trace5 中 `EnhanceConfigManager::GetResource` / URL request 这类关键路径。
  后续更细的 frame/script 归属仍可作为 P1/P2 工具补强。
- `chrome_main_thread_hotspots` 在 v0.15.6 后续补强中增加了 `upid` / `pid`
  输出和 `page_load_id` / `phase` / `start_ts_ns` / `end_ts_ns` 窗口过滤。该项
  缓解了日志里反复手写 `ts >= navigation_start`、`ts >= fcp_ts`、`ms_from_nav`
  的问题；Chrome page-load 二级工具仍需覆盖资源请求与导航阶段归因。
- 输出中包含本地用户目录、请求 headers、User-Agent、URL 参数等敏感或高噪声
  字符串。当前工具没有统一的截断 / 脱敏策略。

优先级影响：

- `execute_sql` 输出塑形应排在上下文优化的第一梯队。它不仅节省 token，也能
  让 agent 明确知道结果是否被截断，而不是依赖客户端 UI 的 `...`。
- 已新增一个小而强的 slice 子树分析工具，避免递归 SQL 重写和 alias 错误。
- Chrome page-load 场景需要二级工具：页面加载慢时，直接汇总导航阻塞、资源
  请求、Renderer 长任务和关键 URL，而不是让 agent 从原始 `slice` / `args`
  反复试探。
- 精度优先于节省上下文。长字符串预算只能作为显式 opt-in；默认应保留
  URL、slice 名称、`posted_from` 等诊断证据。常见敏感字段脱敏仍应作为
  服务端默认隐私策略，并通过 `string_truncated` / `redacted` 明确标记。

## 设计原则

1. **路由短，教学按需**：工具 description 只回答“何时用、何时不用、关键参数是什么”。长 stdlib 指南放入 resource 或 quickref 工具。
2. **少量强工具优于大量窄工具**：优先保留 `execute_sql` + 精选 domain tools，而不是扩展出几十个薄封装。
3. **默认保留诊断证据，摘要和截断需显式选择**：agent 初次探索可以请求少量代表性数据，但精度关键路径不能默认牺牲 URL、slice 名称、时间戳或调用来源。
4. **错误提示比预防性长描述更划算**：保留失败时的具体修复建议，减少每次 `tools/list` 都携带的长说明。
5. **用测试锁住上下文预算**：描述长度、instructions 长度、常见工具输出大小都应有回归测试。

## 优先级方案

### P0: 建立上下文预算基线

新增单元测试或快照测试，统计以下内容的字符数或近似 token 数：

- `STDLIB_INSTRUCTIONS`
- 每个 `#[tool(description = ...)]`
- 全量 `tools/list` 序列化后的大小
- `list_stdlib_modules` 默认输出大小

建议先用字符数预算，避免引入 tokenizer 依赖。验收标准：

- 预算测试在 CI 中运行。
- 新增工具或扩展 description 超预算时，测试失败并要求显式调整预算。
- 测试失败信息指出具体超预算项。

### P1: 压缩常驻上下文

把 `ServerInfo.instructions` 缩短为最小工作流：

```text
Call load_trace first. Use dedicated chrome_* tools for common Chrome questions.
Use list_tables/list_table_structure for schema discovery. Use execute_sql for
custom PerfettoSQL. Prefer stdlib modules; call list_stdlib_modules or read the
stdlib quick reference when unsure.
```

同时重写长工具 description：

- `load_trace`：保留“必须先调用、path 是本地完整文件、重复加载可复用缓存”。
- `execute_sql`：保留“只读、需要已加载 trace、适合自定义查询、结果上限、优先聚合”。
- `chrome_*`：保留输入参数、输出形状、空结果含义和 fallback 的一句提示。
- stdlib URL、长示例、模块列表移到 quickref resource。

验收标准：

- 常驻 instructions 缩短到当前的 30% 以下。
- 每个工具 description 都能在 1-2 个短段落内说明路由用途。
- 现有错误 hint 单元测试继续通过。

### P1: 提供 stdlib quickref Resource

新增 MCP Resource：

- URI: `resource://perfetto-mcp/stdlib-quickref`
- 内容：精选 stdlib 模块、适用场景、公开 view、最小查询示例。
- 与 `list_stdlib_modules` 分工：resource 负责教学，tool 负责结构化枚举。

验收标准：

- agent 可在不调用 `execute_sql` 的情况下获取 stdlib 指南。
- quickref 覆盖 Chrome、Android、generic 三类入口。
- README 和 README.zh-CN 中注明“不了解 stdlib 时先读 quickref”。

### P1: 增强 `execute_sql` 结果塑形

在 `ExecuteSqlParams` 增加可选字段，保持 `sql` 单字段调用兼容：

- `limit`: 对返回 rows 做输出层截断，不重写用户 SQL。
- `offset`: 输出层分页。
- `head`: 等价于 `limit` 的 agent 友好别名，二者同时出现时报错。
- `columns_only`: 只返回列名和类型可得信息。
- `row_count`: 返回实际解码行数，配合截断说明。
- `summary`: 返回 `{columns, row_count, sample_rows}`，默认 sample 10 行。

不要自动改写 SQL 的 `LIMIT`，因为 PerfettoSQL 可以包含 `INCLUDE`、CTE、复杂查询，字符串改写容易出错。第一阶段只做“结果输出层塑形”。后续如果要优化底层执行量，再设计明确的 query wrapper。

验收标准：

- 默认行为保持兼容。
- `summary` 模式不会输出超过固定样本行数。
- 超过输出限制时返回 `truncated: true` 和 `returned_rows`。
- 对大结果集的错误文案继续建议使用聚合查询。

### P2: `list_stdlib_modules` 搜索与筛选

给 `list_stdlib_modules` 增加可选参数：

- `domain`: `chrome | android | generic`
- `query`: 在 module、view、description 中做大小写不敏感搜索
- `limit`: 返回前 N 条

默认仍可返回完整精选列表，但 agent 可以用 `query: "jank"`、`domain: "android"` 等方式少取无关内容。

验收标准：

- 无参数结果与当前兼容。
- 筛选参数支持组合。
- 参数 schema 简短，不把所有模块细节复制进 description。

### P2: 缓存高频 schema 查询

为当前 trace 增加内存缓存：

- `list_tables(pattern)` 按 `trace_id + pattern` 缓存。
- `list_table_structure(table_name)` 按 `trace_id + table_name` 缓存。
- `load_trace` 切换到不同 trace 时不复用旧 trace 的 schema cache。

建议优先做内存级缓存，不先落盘，避免 trace 文件变化和 invalidation 复杂化。

验收标准：

- 重复调用不会重复打到 trace_processor。
- 切换 trace 后不会返回旧 schema。
- 有单元测试覆盖缓存命中、未命中、切换 trace 失效。

### P3: 查询取消与观测

补充运行时硬化能力：

- query cancellation：agent 发起低质量长查询时可中断。
- tracing spans：记录 `sql_len`、duration、row_count、truncated、tool_name。
- 慢查询错误或日志中给出下一步建议。

验收标准：

- 长查询可以在超时前被显式取消。
- 日志足以区分 trace_processor 慢、SQL 慢、结果 decode 慢、序列化输出大。

## 建议实施顺序

1. 实现 `execute_sql` 输出层塑形，优先解决真实会话里结果被 UI 省略的问题。
2. 先加预算测试，锁定当前成本。
3. 压缩 `STDLIB_INSTRUCTIONS` 和工具 description。
4. 新增 stdlib quickref resource，并更新 README。
5. 增强 `list_stdlib_modules` 筛选。
6. 新增 slice 子树分析和 page-load 二级分析工具。
7. 实现 schema 查询内存缓存。
8. 最后做 cancellation 和 tracing spans。

这个顺序的好处是：先降低最大实际 token 来源，即大查询结果，再建立防回归约束并移动教学内容。真实日志显示，大结果和长字符串对会话质量的影响比工具声明体积更直接。缓存和取消属于运行体验优化，可在上下文成本稳定后推进。

## 风险与缓解

- **风险：压缩 description 后 agent 不知道如何开始。**  
  缓解：保留最小工作流在 instructions 中，并在 README、quickref、错误 hint 中提供更长说明。

- **风险：`execute_sql` 塑形让用户误以为 SQL 只执行了部分数据。**  
  缓解：字段命名使用 `returned_rows`、`decoded_rows`、`truncated`，明确这是输出层截断。

- **风险：缓存返回过期 schema。**  
  缓解：第一阶段只做进程内缓存，并绑定当前 trace 路径或内部 trace identity。

- **风险：预算测试过严阻碍正常文档改进。**  
  缓解：预算可以显式调整，但每次调整都必须说明为什么新增常驻上下文值得。

## 成功指标

- 全量工具声明和 instructions 的上下文体积显著下降。
- 常见分析路径仍然是：`load_trace` -> domain tool 或 schema discovery -> `execute_sql`。
- agent 更少生成 raw `slice LIKE '%...%'` 扫描。
- 大结果集默认以摘要或样本形式进入上下文。
- CI 能捕获工具描述膨胀和输出预算回归。
