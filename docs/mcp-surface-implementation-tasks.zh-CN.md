# perfetto-mcp-rs MCP 表面实施任务清单

> 本文把当前 MCP 表面问题拆成可执行任务。重点不是复制 Perfetto UI 的所有能力，而是在保持少量强工具的前提下，提升 LLM 的路由准确率、上下文效率和结果可控性。

## 原则

- **先补导航，再扩工具。** 当前工具数约 12 个，规模健康；不要为了对齐 UI 能力批量增加窄工具。
- **保留 `execute_sql` 逃生口。** 领域工具只覆盖高频路径，复杂分析仍交给 PerfettoSQL。
- **关键路径必须是 Tool。** LLM 需要自主调用的能力不能只放进 Resource 或 Prompt。
- **输出塑形不能改变 SQL 语义。** 只能改变返回给 LLM 的结果量和形态，不能自动改写查询。
- **用测试锁住上下文成本。** 防止 instructions、tool descriptions 和大结果集悄悄膨胀。

## 真实会话证据

样本：`C:\Users\admin3\Documents\trace5.json` 的性能分析日志
`C:\Users\admin3\Documents\log`。

- 正向证据：`load_trace` summary 成功把 agent 路由到 Chrome domain tools；
  `chrome_page_load_summary` 快速暴露 FCP / load 约 10.7 秒；
  `chrome_scroll_jank_summary` 和 `chrome_web_content_interactions` 的空结果帮助排除
  滚动 / 交互卡顿；`chrome_main_thread_hotspots` 暴露 Renderer 主线程长任务。
- 主要缺口：后续定位“哪个资源或加载阶段卡住”依赖大量手写 SQL，尤其是
  `slice` / `args` join、递归展开 descendants、按 navigation/FCP 时间窗口过滤。
- 输出问题：多处结果在客户端 UI 中被 `...` 截断，`list_table_structure` 和长
  `args.display_value` 噪声较高；输出里还包含本地路径、headers、User-Agent 等
  可能敏感的字符串。
- 错误模式：一次递归 SQL 因 `ambiguous column name: depth` 失败，agent 能修复，
  但说明稳定 SQL 模式应被工具化或至少被错误 hint 明确引导。

## P0：最高优先级

### T0.1 实现 `inspect_trace` / `trace_overview`

- **范围：** `src/server.rs`，必要时新增结构化 response 类型。
- **返回：**
  - 当前 loaded trace 名称、路径显示名、trace 类型判断。
  - 关键表 / stdlib view 是否存在，例如 `slice`、`process`、`thread`、Chrome views、Android views。
  - 粗略规模信号：process count、thread count、slice count、counter count。
  - trace profile：`chrome`、`android`、`generic`、`unknown`，带 confidence 和 evidence。
  - quality warnings：缺关键表、缺 thread metadata、trace 为空、query/schema 探索受限。
  - recommended next steps：按 rank 给出下一步 Tool、参数、reason、confidence。
- **推荐格式：**

```json
{
  "recommended_next_steps": [
    {
      "rank": 1,
      "goal": "main_thread_jank",
      "tool": "chrome_main_thread_hotspots",
      "arguments": { "min_dur_ms": 16, "limit": 100 },
      "reason": "Chrome task views are available",
      "confidence": 0.86
    }
  ],
  "fallback_steps": [
    {
      "tool": "list_tables",
      "arguments": { "pattern": "chrome_*" },
      "reason": "Use schema discovery if the dedicated Chrome tool is empty"
    }
  ]
}
```

- **验收：** LLM 调一次即可知道该走 Chrome 工具、schema discovery、stdlib quickref，还是直接 `execute_sql`。
- **非目标：** 不动态修改 `tools/list`；不做自然语言推理式建议。

### T0.2 增强 `execute_sql` 输出塑形

- **范围：** `ExecuteSqlParams`、query result serialization、相关测试。
- **新增可选参数：**
  - `head`：输出层只返回前 N 行。
  - `limit`：`head` 的正式别名；与 `head` 同时出现时报错。
  - `columns_only`：只返回列信息。
  - `summary`：返回列、行数状态、少量样本行。
  - `include_row_count`：请求返回解码行数或已知行数状态。
  - `max_string_len`：限制单个字符串单元格长度，默认值应足够保留 URL / slice 名称
    的关键信息。
  - `redact_strings`：对 headers、cookies、tokens、本地用户路径等常见敏感字符串
    做保守脱敏。
- **返回元数据：**
  - `returned_rows`
  - `truncated`
  - `row_count_known`
  - `string_truncated`
  - `redacted`
  - `note`: 明确说明“仅输出层截断，SQL 执行语义未改变”。
- **验收：**
  - `execute_sql(sql)` 默认行为保持兼容。
  - 塑形不会自动向 SQL 注入 `LIMIT`。
  - 样本或截断结果永远带 `truncated` / `returned_rows` 标记。
  - 长字符串截断不会破坏 JSON；脱敏策略有测试覆盖，并允许显式关闭。

### T0.3 建立上下文预算测试

- **范围：** server unit tests。
- **统计对象：**
  - `STDLIB_INSTRUCTIONS`
  - 每个 `#[tool(description = ...)]`
  - 全量 `tools/list` JSON 大小
  - `list_stdlib_modules` 默认返回大小
- **验收：**
  - CI 能发现常驻上下文超预算。
  - 失败信息指出具体超预算项。
  - 调整预算必须显式修改测试，避免无意识膨胀。

### T0.4 实现 `slice_descendants_summary`

- **动机：** 真实会话中 agent 多次手写 `WITH RECURSIVE descendants...` 展开
  slice 子树，并出现 `ambiguous column name: depth`。这是高频、稳定、易错的
  Perfetto 分析子任务。
- **输入：**
  - `slice_ids`: 根 slice id 列表。
  - `min_dur_ms`: 子 slice 最小时长，默认 1ms。
  - `max_depth`: 最大递归深度，防止异常 trace 或 query 失控。
  - `include_args`: 是否返回匹配子树中的 args 摘要，默认 false。
  - `limit`: 返回行数上限。
- **输出：**
  - 按 `root_id`、`depth`、`slice.name` 聚合的 `count`、`total_ms`、`max_ms`。
  - 可选 args 摘要使用字符串截断 / 脱敏策略。
- **验收：**
  - 对真实日志中的长任务 slice id 可复现手写 SQL 的核心结果。
  - 不要求调用方理解 recursive CTE。
  - 输出包含 `root_id`，支持一次分析多个根 slice。
  - 对无效 slice id 返回空 rows，而不是 SQL 错误。

## P1：导航与知识按需化

### T1.1 实现 `stdlib-quickref` Resource

- **URI：** `resource://perfetto-mcp/stdlib-quickref`
- **内容：** Chrome、Android、generic 常用 stdlib 模块、公开 views、最小查询示例。
- **分工：** Resource 负责教学；`list_stdlib_modules` 负责结构化枚举。
- **验收：** 支持 Resource 的客户端可按需读取长指南；Tools-only 客户端仍可通过 `list_stdlib_modules` 工作。

### T1.2 压缩 `STDLIB_INSTRUCTIONS` 和长 tool descriptions

- **保留：** 最小工作流、何时使用、何时不用、关键参数、错误恢复入口。
- **移出：** 长模块列表、复杂 SQL 示例、大量 URL。
- **验收：**
  - instructions 缩短到当前显著更小的固定预算内。
  - 每个 tool description 控制在 1-2 个短段落。
  - 现有错误 hint 保留，因为它们是按需出现的高价值 token。

### T1.3 增强 `list_stdlib_modules`

- **新增可选参数：**
  - `domain`: `chrome | android | generic`
  - `query`: 在 module、view、description 中搜索
  - `limit`: 限制返回条数
- **验收：**
  - 无参数调用保持兼容。
  - LLM 可用 `query: "binder"` 或 `domain: "android"` 减少无关返回。

### T1.4 实现 Chrome page-load 二级分析工具

- **候选名称：** `chrome_page_load_detail` 或
  `chrome_resource_loading_summary`。
- **动机：** `chrome_page_load_summary` 能发现 FCP / load 慢，但真实会话中 agent
  仍需大量手写 SQL 才定位到 `EnhanceConfigManager::GetResource` / URL request
  链路和关键 JS 文件。
- **输入：**
  - `navigation_id` 或 `page_load_id`。
  - 可选 `url_substring`，用于聚焦特定站点或资源族。
  - `start_ms` / `end_ms` 或 `phase`，支持导航开始、DCL、FCP、load 后窗口。
  - `limit`。
- **输出：**
  - navigation 关键时间点：DCL、FCP、load、LCP（如有）。
  - Browser 主线程导航阻塞摘要。
  - Renderer 主线程长任务摘要。
  - 资源请求摘要：URL / 文件名、first_ms、last_end_ms、max_ms、total_ms、
    关键 slice 名称。
  - 输出应保留行级证据，不直接写死自然语言根因结论。
- **验收：**
  - 能把“FCP 10.7s”后续分析压缩为一次或少数几次工具调用。
  - 对无资源 URL 的 trace 返回空资源段并保留 navigation / long task 段。
  - URL 和 headers 默认使用截断 / 脱敏。

## P2：运行效率与可观测性

### T2.1 缓存 schema 查询

- **缓存对象：**
  - `list_tables(pattern)`
  - `list_table_structure(table_name)`
- **约束：** 缓存绑定当前 trace identity；切换 trace 后不能复用旧 schema。
- **验收：** 重复 schema 探索不重复打 trace_processor；有命中、未命中、切换 trace 失效测试。

### T2.2 增加 tool annotations

- **候选分类：**
  - 查询类工具：`readOnlyHint=true`、`idempotentHint=true`。
  - `load_trace`：会启动 / 缓存 `trace_processor_shell`，不应标成纯 read-only。
- **验收：** 客户端可更好显示安全提示；服务端仍以真实校验和只读 trace_processor 行为为准。

### T2.3 增加 tracing spans

- **记录字段：** tool name、SQL 长度、duration、row count、truncated、error kind。
- **验收：** 能区分 trace_processor 慢、SQL 慢、decode 慢、输出体积过大。

### T2.4 增强 SQL 错误分类与 hint

- **新增候选 `QueryErrorKind`：**
  - `AmbiguousColumn`
  - `SyntaxError`
- **动机：** 真实会话中 `ambiguous column name: depth` 来自递归 CTE 与 `slice`
  表同名列冲突。agent 可以修复，但错误 hint 可以直接提示使用表别名或 CTE
  别名。
- **验收：**
  - `ambiguous column name:` 错误返回明确建议：给列加表/CTE alias。
  - 仍保留原始 trace_processor 错误文本。

## P3：谨慎扩展分析工具

### T3.1 补少量高频 domain tools

- **优先候选：**
  1. `chrome_page_load_detail` / `chrome_resource_loading_summary`
  2. `android_startup_summary`
  3. `anr_suspects`
  4. `cpu_hot_threads`
  5. `memory_growth_summary`
- **准入标准：**
  - 比 `execute_sql + stdlib-quickref` 明显减少试错。
  - 有 fixture / e2e。
  - 输出保持 row-preserving 或结构化摘要，不把分析结论写死。
- **验收：** 每个新增工具都有清晰的使用场景、不适用场景和后续步骤。

## 推荐实施顺序

1. T0.2 增强 `execute_sql` 输出塑形，先解决真实会话中结果被 `...` 截断的问题。
2. T0.1 实现 `inspect_trace` / `trace_overview`。
3. T0.4 实现 `slice_descendants_summary`。
4. T0.3 建立上下文预算测试。
5. T1.4 实现 Chrome page-load 二级分析工具。
6. T1.1 实现 `stdlib-quickref` Resource。
7. T1.2 压缩 instructions 和 descriptions。
8. T1.3 增强 `list_stdlib_modules` 筛选。
9. T2.1 缓存 schema 查询。
10. T2.2 增加 tool annotations。
11. T2.3 增加 tracing spans。
12. T2.4 增强 SQL 错误分类与 hint。
13. T3.1 按真实使用频率补 domain tools。

## 暂不实施

- 动态过滤 `tools/list`：会破坏 prompt-prefix caching，客户端兼容性不稳定。
- 为每个 Perfetto stdlib view 增加一个 MCP tool：工具面会快速膨胀。
- 让 `inspect_trace` 输出自由文本推理：推荐应是结构化、可执行、可测试的。
- 自动改写用户 SQL：容易破坏 CTE、排序、窗口函数和聚合语义。
- 复制 Perfetto UI 渲染功能：timeline、flamegraph、截图、图表导出对 LLM 不是关键能力。

## 完成标准

- LLM 初次分析 trace 的工具选择更稳定。
- 大结果集不会默认占满上下文。
- Tools-only 客户端仍能完成核心分析路径。
- Resource / annotations 只是增强，不成为关键路径依赖。
- 新增 domain tools 不显著增加选错工具概率。
