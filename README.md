# gitlab-mcp

A GitLab [Model Context Protocol](https://modelcontextprotocol.io) server written in Rust.
It is a drop-in replacement for the Node reference server `@zereight/mcp-gitlab`,
covering the same 262 tools with the same names and argument schemas.

## Why

The Node server runs as three processes (an `npm exec` wrapper, the server, and
its dependency tree) and holds roughly 110 MB per Claude session. This one is a
single 4.3 MB static binary that starts in milliseconds and idles at a few MB.

## Design

The tool catalogue is data, not code. Two files are embedded at build time:

| File | Contents |
| --- | --- |
| `data/tools.json` | 262 tool names, descriptions and JSON input schemas |
| `data/endpoints.json` | how each tool maps onto the GitLab API |

`src/exec.rs` is a generic executor: it fills path parameters, splits the
remaining arguments between the query string and the JSON body, and shapes the
response. Adding a GitLab endpoint means adding a row to `endpoints.json`, not
writing Rust.

An endpoint record has one of four kinds:

- `rest` — a single REST call, the common case.
- `graphql` — a stored GraphQL document with a variable mapping.
- `composite` — several calls or a polling loop, such as `wait_for_pipeline`.
- `local` — answered by the server, such as `discover_tools`.

## Configuration

Environment names match the reference server, so an existing `.mcp.json` keeps
working. Command-line flags `--token` and `--api-url` are also accepted, but
prefer the environment: arguments are visible to any process via `ps`.

| Variable | Meaning |
| --- | --- |
| `GITLAB_PERSONAL_ACCESS_TOKEN` | Access token. Required. |
| `GITLAB_API_URL` | Instance URL. A bare host or an `/api/v4` suffix both work. Defaults to gitlab.com. |
| `GITLAB_PROJECT_ID` | Fills in a missing `project_id` argument. |
| `GITLAB_READ_ONLY_MODE` | Hides and refuses every tool that changes state. |
| `GITLAB_TOOLSETS` | `all`, `default`, or a comma-separated list of categories. |
| `GITLAB_DENIED_TOOLS_REGEX` | Tools whose name matches are hidden and refused. |
| `GITLAB_ALLOWED_PROJECT_IDS` | Restricts which projects may be addressed. |
| `GITLAB_REQUEST_TIMEOUT_MS` | Per-request timeout. Defaults to 60000. |
| `GITLAB_MAX_RESPONSE_BYTES` | Truncation limit for non-JSON responses. Defaults to 1000000. |
| `GITLAB_MCP_LOG` | Log filter for stderr. Defaults to `warn`. |

Nine categories are active by default: merge requests, issues, repositories,
branches, projects, labels, CI, groups and users. The rest are opt-in, either
through `GITLAB_TOOLSETS` or by calling `discover_tools` at runtime.

Example configuration:

```json
{
  "mcpServers": {
    "gitlab": {
      "command": "/path/to/gitlab-mcp",
      "env": {
        "GITLAB_PERSONAL_ACCESS_TOKEN": "${GITLAB_TOKEN}",
        "GITLAB_API_URL": "https://gitlab.example.com"
      }
    }
  }
}
```

## Build

```sh
cargo build --release
cargo test
```

## Differences from the reference server

- Only the stdio transport is implemented. The reference also offers SSE and
  streamable HTTP.
- OAuth device flow and the token proxy are not implemented; the server
  authenticates with a token.
- The `full_response` argument is accepted and ignored: responses are always
  returned in full rather than trimmed to a summary shape.
- Errors reach the caller as MCP tool errors carrying GitLab's own message,
  rather than as protocol-level failures.

## Known limitations

- `update_work_item` is listed but not implemented. The reference performs it as
  a chain of up to nine GraphQL mutations; this server refuses the call with an
  explanation rather than applying part of the change.
- Some GraphQL documents come from the reference implementation and target a
  newer GitLab than 16.11. On an older instance `list_ci_catalog_resources`,
  `get_ci_catalog_resource`, `list_work_items`, `list_work_item_statuses` and
  `list_custom_field_definitions` fail with an unknown field or input type. The
  reference server fails identically on the same instance, so this is a GitLab
  version constraint rather than a difference between the two servers.
- The `orbit` category needs a GitLab feature that most instances do not expose.

## Verification

`scripts/smoke.sh` drives the server over stdio against a live instance and
calls every read-only tool whose required arguments a project id satisfies:

```sh
GITLAB_PERSONAL_ACCESS_TOKEN=... GITLAB_API_URL=https://gitlab.example.com \
  scripts/smoke.sh group/subgroup/project
```

Against GitLab 16.11 it answers 44 calls, of which the failures are the version
constraints listed above plus tools whose alternative arguments the script does
not supply.

The endpoint table was cross-checked tool by tool against the reference
implementation. That audit found 23 wrong records, all from the same cause: a
generated mapping had captured the URL of a resolver helper rather than the
tool's own endpoint. `cargo test` includes a guard against that class of error.

## Licence

MIT, see `LICENSE`. Parts of what ships here are derived from other projects and
carry their licences; `ATTRIBUTION.md` says exactly which parts and under which
terms.
