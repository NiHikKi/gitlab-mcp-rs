#!/usr/bin/env bash
# Read-only smoke test against a live GitLab instance.
#
#   GITLAB_PERSONAL_ACCESS_TOKEN=... GITLAB_API_URL=https://gitlab.example.com \
#     scripts/smoke.sh [project_path_or_id]
#
# Exercises every read-only tool whose required arguments are satisfied by a
# project id alone, and reports which ones answered and which failed.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${GITLAB_MCP_BIN:-$ROOT/target/release/gitlab-mcp}"
PROJECT="${1:-}"

if [[ -z "${GITLAB_PERSONAL_ACCESS_TOKEN:-}" ]]; then
  echo "GITLAB_PERSONAL_ACCESS_TOKEN is not set" >&2
  exit 2
fi
if [[ ! -x "$BIN" ]]; then
  echo "binary not found at $BIN; run: cargo build --release" >&2
  exit 2
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

node -e '
  const fs = require("fs");
  const root = process.argv[1], project = process.argv[2];
  const tools = JSON.parse(fs.readFileSync(root + "/data/tools.json", "utf8"));
  const eps = JSON.parse(fs.readFileSync(root + "/data/endpoints.json", "utf8"));
  const plan = [];
  for (const tool of tools.tools) {
    const e = eps[tool.name];
    if (!e || !e.read_only) continue;
    const required = tool.inputSchema?.required || [];
    if (required.some(r => r !== "project_id")) continue;
    const props = tool.inputSchema?.properties || {};
    const args = {};
    if (props.project_id && project) args.project_id = project;
    else if (props.project_id) continue;
    if (props.per_page) args.per_page = 2;
    plan.push({ name: tool.name, args });
  }
  fs.writeFileSync(process.argv[3] + "/plan.json", JSON.stringify(plan));
  console.error(`planned ${plan.length} read-only calls`);
' "$ROOT" "$PROJECT" "$WORK"

{
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  node -e '
    const plan = require(process.argv[1] + "/plan.json");
    plan.forEach((t, i) => console.log(JSON.stringify({
      jsonrpc: "2.0", id: 100 + i, method: "tools/call",
      params: { name: t.name, arguments: t.args },
    })));
  ' "$WORK"
  sleep "${SMOKE_WAIT:-60}"
} | GITLAB_TOOLSETS=all GITLAB_READ_ONLY_MODE=true "$BIN" 2>/dev/null > "$WORK/out.jsonl"

node -e '
  const fs = require("fs");
  const work = process.argv[1];
  const plan = require(work + "/plan.json");
  const ok = [], fail = [];
  for (const line of fs.readFileSync(work + "/out.jsonl", "utf8").split("\n")) {
    if (!line.trim().startsWith("{")) continue;
    const m = JSON.parse(line);
    if (typeof m.id !== "number" || m.id < 100) continue;
    const name = plan[m.id - 100].name;
    const text = m.result?.content?.[0]?.text || "";
    if (m.result?.isError || m.error) fail.push([name, text.slice(0, 120)]);
    else ok.push(name);
  }
  console.log(`answered ${ok.length + fail.length} of ${plan.length}: ${ok.length} ok, ${fail.length} failed`);
  if (fail.length) {
    console.log("\nfailures:");
    for (const [n, e] of fail) console.log(`  ${n.padEnd(36)}${e}`);
  }
  process.exit(fail.length ? 1 : 0);
' "$WORK"
