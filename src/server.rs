//! The MCP surface: tool listing, dispatch and result formatting.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use crate::exec::Executor;
use crate::registry::{Registry, ToolEntry};

pub struct GitLabMcp {
    registry: Arc<Registry>,
    exec: Arc<Executor>,
}

impl GitLabMcp {
    pub fn new(registry: Arc<Registry>, exec: Arc<Executor>) -> Self {
        Self { registry, exec }
    }

    fn to_mcp_tool(entry: &ToolEntry) -> Tool {
        Tool::new(
            entry.meta.name.clone(),
            entry.meta.description.clone(),
            Arc::new(entry.meta.input_schema.clone()),
        )
        .annotate(
            ToolAnnotations::default()
                .read_only(entry.endpoint.read_only)
                .destructive(entry.endpoint.destructive)
                .idempotent(matches!(
                    entry.endpoint.method.as_deref(),
                    Some("GET") | Some("PUT") | Some("DELETE")
                ))
                .open_world(true),
        )
    }

    fn instructions(&self) -> String {
        let visible = self.registry.visible().len();
        let inactive: Vec<&str> = {
            let active = self.registry.active_categories();
            self.registry
                .toolsets()
                .iter()
                .filter(|t| !active.contains(&t.id))
                .map(|t| t.id.as_str())
                .collect()
        };
        let mut s = format!(
            "GitLab API access. {visible} tools are active. Project arguments accept either a \
             numeric id or a URL-encoded path such as group/subgroup/project."
        );
        if self.registry.read_only() {
            s.push_str(" The server runs in read-only mode, so no tool can change GitLab state.");
        }
        if !inactive.is_empty() {
            s.push_str(&format!(
                " Further categories are available on request via discover_tools: {}.",
                inactive.join(", ")
            ));
        }
        s
    }
}

impl ServerHandler for GitLabMcp {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("gitlab-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.instructions())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self.registry.visible().iter().map(|e| Self::to_mcp_tool(e)).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.registry.get(name).map(|e| Self::to_mcp_tool(&e))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let Some(entry) = self.registry.get(&name) else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool {name:?}"
            ))])
            .into());
        };

        if !self.registry.is_callable(&entry) {
            let reason = if self.registry.read_only() && !entry.endpoint.read_only {
                format!("{name} changes GitLab state and the server is in read-only mode")
            } else {
                format!(
                    "{name} belongs to the inactive category {:?}; call discover_tools with that \
                     category to enable it",
                    entry.endpoint.category
                )
            };
            return Ok(CallToolResult::error(vec![ContentBlock::text(reason)]).into());
        }

        let args: Map<String, Value> = request.arguments.unwrap_or_default();
        let listing_may_change = entry.meta.name == "discover_tools";

        match self.exec.call(&entry, args).await {
            Ok(value) => {
                let text = match &value {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other)
                        .unwrap_or_else(|e| format!("could not serialise the result: {e}")),
                };
                if listing_may_change {
                    tracing::info!("tool list changed after discover_tools");
                }
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
            }
            // Tool failures are results, not protocol errors: the model must be
            // able to read what went wrong and try something else.
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into()),
        }
    }
}
