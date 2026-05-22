#!/usr/bin/env bash
set -euo pipefail

repo="${1:-tooluse-labs/perfetto-mcp-rs}"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

gh auth status >/dev/null

create_issue() {
  local title="$1"
  local body="$2"
  local body_file
  body_file="$tmp_dir/issue-$RANDOM-$RANDOM.md"
  printf '%s\n' "$body" > "$body_file"
  gh issue create --repo "$repo" --title "$title" --body-file "$body_file"
}

replace_epic_url() {
  local template="$1"
  printf '%s' "${template//\{\{EPIC_URL\}\}/$epic_url}"
}

epic_body="$(cat <<'EOF'
## Goal

Track the MCP surface and context-efficiency work described in `docs/mcp-surface-implementation-tasks.zh-CN.md`.

This epic is intentionally about LLM usability, routing accuracy, and context control. It is not a request to clone every Perfetto UI feature as a separate MCP tool.

## Scope

- Add a trace overview / routing entry point.
- Shape large SQL outputs without changing SQL execution semantics.
- Move long teaching material out of always-on context where possible.
- Add lightweight observability and schema-query efficiency.
- Keep `execute_sql` as the general escape hatch.

## Initial Child Work

- [ ] Implement `inspect_trace` / `trace_overview`
- [ ] Add `execute_sql` output shaping
- [ ] Add context budget tests
- [ ] Expose `stdlib-quickref` Resource
- [ ] Compress server instructions and tool descriptions
- [ ] Add filters to `list_stdlib_modules`
- [ ] Cache schema discovery queries
- [ ] Add MCP tool annotations
- [ ] Add tracing spans for tool/query execution

## Non-Goals

- Dynamic `tools/list` filtering.
- One MCP tool per Perfetto stdlib view.
- Replacing `execute_sql` with narrow wrappers.
- Automatically rewriting user SQL.
- Porting Perfetto UI rendering features such as timelines, flamegraph images, screenshots, or chart export.
EOF
)"

epic_url="$(create_issue "MCP surface and context-efficiency improvements" "$epic_body")"

declare -a child_titles=()
declare -a child_urls=()

add_child_issue() {
  local title="$1"
  local template="$2"
  local body
  local url
  body="$(replace_epic_url "$template")"
  url="$(create_issue "$title" "$body")"
  child_titles+=("$title")
  child_urls+=("$url")
}

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Add a structured first-call tool that tells the LLM what kind of trace is loaded, what analysis paths are available, and which tool to call next.

## Proposed Shape

Return structured fields for:

- Loaded trace display name and trace profile: `chrome`, `android`, `generic`, or `unknown`.
- Evidence and confidence for the profile classification.
- Key table / stdlib-view availability.
- Coarse size signals: process count, thread count, slice count, counter count.
- Quality warnings: missing key tables, empty trace, incomplete thread metadata, or limited schema discovery.
- `recommended_next_steps`: ranked tool calls with arguments, reason, and confidence.
- `fallback_steps`: schema-discovery or `execute_sql` paths when a dedicated tool is unavailable or empty.

## Acceptance Criteria

- Calling the tool after `load_trace` gives enough guidance for the next MCP call.
- Recommendations are deterministic and testable, not free-text reasoning.
- The tool does not mutate `tools/list`.
- Tests cover at least Chrome, generic, missing-table, and no-trace-loaded cases.
EOF
)"
add_child_issue "Implement inspect_trace / trace_overview" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Let callers request smaller, explicit result shapes from `execute_sql` without changing SQL execution semantics.

## Proposed Parameters

- `head`: return only the first N decoded rows.
- `limit`: formal alias for `head`; reject calls that pass both.
- `columns_only`: return column metadata without row payload.
- `summary`: return columns, row-count status, and sample rows.
- `include_row_count`: include decoded / known row-count metadata when available.

## Required Metadata

- `returned_rows`
- `truncated`
- `row_count_known`
- `note` explaining that truncation is output-layer only and SQL semantics were not changed.

## Acceptance Criteria

- `execute_sql(sql)` remains backward-compatible by default.
- The implementation never injects `LIMIT` into the user SQL.
- Truncated or sampled output is always marked explicitly.
- Tests cover default compatibility, `head`, `summary`, `columns_only`, conflicting parameters, and over-limit output.
EOF
)"
add_child_issue "Add execute_sql output shaping" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Prevent always-on MCP context from growing accidentally as tools and descriptions evolve.

## Measure

- `STDLIB_INSTRUCTIONS`
- Each tool description
- Serialized `tools/list` payload
- Default `list_stdlib_modules` output

## Acceptance Criteria

- CI fails when a budget is exceeded.
- Failure messages identify the specific item over budget.
- Budget changes require an explicit test update.
- The test uses character counts or another deterministic approximation; no tokenizer dependency is required.
EOF
)"
add_child_issue "Add context budget tests" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Move long PerfettoSQL stdlib teaching material into an on-demand MCP Resource while keeping Tools-only clients functional through `list_stdlib_modules`.

## Resource

`resource://perfetto-mcp/stdlib-quickref`

## Content

- Chrome stdlib entry points and minimal query examples.
- Android stdlib entry points and minimal query examples.
- Generic modules such as `slices.with_context` and CPU frequency counters.
- Guidance on when to use dedicated tools vs `execute_sql`.

## Acceptance Criteria

- The Resource is exposed through MCP resource capabilities.
- README mentions the quickref for clients that support Resources.
- `list_stdlib_modules` remains a Tool fallback for clients without Resource support.
- Tests verify the Resource URI and representative content.
EOF
)"
add_child_issue "Expose stdlib quickref Resource" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Reduce always-on context by keeping server instructions and tool descriptions focused on routing, while moving long teaching material to the quickref Resource or `list_stdlib_modules`.

## Keep In Descriptions

- When to use the tool.
- When not to use the tool.
- Key parameters.
- Empty-result meaning.
- Error-recovery entry points.

## Move Out

- Long stdlib module lists.
- Large SQL examples.
- URL lists.
- Repeated background explanation.

## Acceptance Criteria

- Instructions fit within the context budget introduced by the budget tests.
- Tool descriptions remain accurate and actionable.
- Existing error hints are preserved.
- Tests that pin important descriptions continue to pass or are updated deliberately.
EOF
)"
add_child_issue "Compress server instructions and tool descriptions" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Let callers retrieve a smaller, more relevant subset of the curated stdlib module list.

## Proposed Parameters

- `domain`: `chrome`, `android`, or `generic`.
- `query`: case-insensitive search over module, views, and description.
- `limit`: max returned modules.

## Acceptance Criteria

- No-argument behavior remains compatible.
- Filters can be combined.
- Invalid domains fail clearly.
- Descriptions stay concise and do not duplicate the full module list.
- Tests cover no-filter, domain-only, query-only, combined filters, and limit.
EOF
)"
add_child_issue "Add filters to list_stdlib_modules" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Avoid repeated trace_processor RPCs for common schema discovery calls during multi-step investigations.

## Cache

- `list_tables(pattern)`
- `list_table_structure(table_name)`

## Constraints

- Cache must be scoped to the current trace identity.
- Switching traces must not reuse stale schema.
- In-memory only for the first implementation.

## Acceptance Criteria

- Repeated schema calls hit the cache.
- Loading or switching to a different trace invalidates or bypasses the old cache.
- Tests cover hit, miss, and trace-switch behavior.
EOF
)"
add_child_issue "Cache schema discovery queries" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Expose MCP tool annotations so clients can display safer grouping and intent hints.

## Initial Classification

- Query tools: `readOnlyHint=true`, `idempotentHint=true`.
- `load_trace`: starts or reuses a `trace_processor_shell` process and updates current-trace state, so do not mark it as purely read-only.

## Acceptance Criteria

- Verify the `rmcp` annotation API before mass changes.
- Add annotations to one low-risk tool first, then apply the confirmed pattern.
- Tests or snapshot checks verify annotations are present in tool metadata if the SDK exposes them.
- Documentation states annotations are hints, not server-side safety boundaries.
EOF
)"
add_child_issue "Add MCP tool annotations" "$body"

body="$(cat <<'EOF'
Epic: {{EPIC_URL}}

## Goal

Add enough observability to distinguish slow SQL, slow trace_processor responses, expensive decoding, and large serialized outputs.

## Suggested Fields

- Tool name
- SQL length
- Duration
- Row count / returned rows
- `truncated`
- Error kind

## Acceptance Criteria

- Tool and query execution paths emit structured tracing spans.
- No sensitive full SQL payload is logged by default unless explicitly intended.
- Slow and failed queries include enough metadata for diagnosis.
- Tests or documented manual verification cover span fields.
EOF
)"
add_child_issue "Add tracing spans for tool/query execution" "$body"

child_lines=""
for i in "${!child_titles[@]}"; do
  child_lines+="- [ ] [${child_titles[$i]}](${child_urls[$i]})"$'\n'
done

final_epic_body="$(cat <<EOF
## Goal

Track the MCP surface and context-efficiency work described in \`docs/mcp-surface-implementation-tasks.zh-CN.md\`.

This epic is intentionally about LLM usability, routing accuracy, and context control. It is not a request to clone every Perfetto UI feature as a separate MCP tool.

## Scope

- Add a trace overview / routing entry point.
- Shape large SQL outputs without changing SQL execution semantics.
- Move long teaching material out of always-on context where possible.
- Add lightweight observability and schema-query efficiency.
- Keep \`execute_sql\` as the general escape hatch.

## Child Issues

${child_lines}
## Non-Goals

- Dynamic \`tools/list\` filtering.
- One MCP tool per Perfetto stdlib view.
- Replacing \`execute_sql\` with narrow wrappers.
- Automatically rewriting user SQL.
- Porting Perfetto UI rendering features such as timelines, flamegraph images, screenshots, or chart export.
EOF
)"

epic_number="${epic_url##*/}"
final_epic_file="$tmp_dir/epic-final.md"
printf '%s\n' "$final_epic_body" > "$final_epic_file"
gh issue edit "$epic_number" --repo "$repo" --body-file "$final_epic_file" >/dev/null

printf 'Created epic: %s\n' "$epic_url"
printf 'Created child issues:\n'
for i in "${!child_titles[@]}"; do
  printf '%s\n' "- ${child_titles[$i]}: ${child_urls[$i]}"
done
