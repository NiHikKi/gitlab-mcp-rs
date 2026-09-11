//! Turns a tool call plus its arguments into GitLab API traffic.

use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Method, StatusCode};
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::config::Config;
use crate::gitlab::{ApiError, GitLabClient};
use crate::registry::{Endpoint, Kind, Registry, ToolEntry, VariableSource};

/// Everything a tool call can fail with, phrased for the model to read.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl From<ApiError> for ToolError {
    fn from(e: ApiError) -> Self {
        ToolError(e.to_string())
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, ToolError> {
    Err(ToolError(msg.into()))
}

/// Percent-encoding for a single path segment: a project path such as
/// `group/sub/project` must arrive at GitLab as `group%2Fsub%2Fproject`.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Same, but for values that legitimately span several segments.
const MULTI_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// Arguments consumed by the server, never forwarded to GitLab.
const META_ARGS: &[&str] = &["full_response"];

/// Path parameters whose value may contain slashes that must survive encoding.
const MULTI_SEGMENT_PARAMS: &[&str] = &["direct_asset_path"];

/// Keys under which resolved global ids are stashed for the variable builder.
const WORK_ITEM_GID: &str = "__work_item_gid";
const WORK_ITEM_TYPE_GID: &str = "__work_item_type_gid";

/// Composite tools this executor knows how to run. `update_work_item` is the
/// one the reference implements as a nine-step mutation chain; it is listed in
/// the catalogue but refuses politely rather than doing half the work.
#[cfg_attr(not(test), allow(dead_code))]
pub const IMPLEMENTED_COMPOSITES: &[&str] = &[
    "wait_for_pipeline",
    "wait_for_job",
    "play_pipeline_jobs",
    "get_webhook_event",
    "create_branch",
    "create_or_update_file",
    "my_issues",
    "update_issue_description_patch",
    "get_users",
    "health_check",
];

const TERMINAL_STATUSES: &[&str] = &["success", "failed", "canceled", "skipped", "manual"];

pub struct Executor {
    client: GitLabClient,
    registry: Arc<Registry>,
    default_project_id: Option<String>,
    allowed_project_ids: std::collections::BTreeSet<String>,
    api_url: String,
}

impl Executor {
    pub fn new(cfg: &Config, registry: Arc<Registry>) -> anyhow::Result<Self> {
        Ok(Self {
            client: GitLabClient::new(cfg)?,
            registry,
            default_project_id: cfg.default_project_id.clone(),
            allowed_project_ids: cfg.allowed_project_ids.clone(),
            api_url: cfg.api_url.clone(),
        })
    }

    pub async fn call(
        &self,
        entry: &ToolEntry,
        mut args: Map<String, Value>,
    ) -> Result<Value, ToolError> {
        self.apply_default_project(entry, &mut args);
        self.check_project_allowed(&args)?;

        match entry.endpoint.kind {
            Kind::Rest => self.call_rest(entry, args).await,
            Kind::Graphql => self.call_graphql(entry, args).await,
            Kind::Composite => self.call_composite(entry, args).await,
            Kind::Local => self.call_local(entry, args),
        }
    }

    /// A configured default project fills in for a missing `project_id`.
    fn apply_default_project(&self, entry: &ToolEntry, args: &mut Map<String, Value>) {
        let Some(default) = &self.default_project_id else {
            return;
        };
        let accepts_project = entry
            .meta
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|p| p.contains_key("project_id"));
        if accepts_project && !args.contains_key("project_id") {
            args.insert("project_id".into(), Value::String(default.clone()));
        }
    }

    fn check_project_allowed(&self, args: &Map<String, Value>) -> Result<(), ToolError> {
        if self.allowed_project_ids.is_empty() {
            return Ok(());
        }
        let Some(p) = args.get("project_id").and_then(as_scalar_string) else {
            return Ok(());
        };
        if self.allowed_project_ids.contains(&p) {
            Ok(())
        } else {
            err(format!(
                "project {p} is not in GITLAB_ALLOWED_PROJECT_IDS; allowed: {}",
                self.allowed_project_ids.iter().cloned().collect::<Vec<_>>().join(", ")
            ))
        }
    }

    // ---------------------------------------------------------------- REST

    async fn call_rest(
        &self,
        entry: &ToolEntry,
        mut args: Map<String, Value>,
    ) -> Result<Value, ToolError> {
        let ep = &entry.endpoint;
        self.resolve_iid_from_source_branch(ep, &mut args).await?;
        let (method_str, template, path_params) = select_route(ep, &args)?;
        let method = parse_method(&method_str)?;
        let path = render_path(&template, &path_params, &args)?;

        let (query, body) = split_args(ep, &method, &path_params, &args)?;
        let resp = self.client.rest(method, &path, &query, body.as_ref()).await?;

        let mut out = resp.body;
        if !ep.redact.is_empty() {
            redact(&mut out, &ep.redact);
        }
        Ok(match ep.wrap.as_deref() {
            Some("pagination") => json!({
                "items": out,
                "pagination": {
                    "next_page": resp.next_page,
                    "total": resp.total,
                },
            }),
            Some("keyset") => json!({
                "items": out,
                "next_page_token": resp.next_page_token.clone().or(resp.next_page),
                "pagination_note": "pass the token back as page_token to continue",
            }),
            _ => out,
        })
    }

    /// Several merge request tools accept a branch name instead of an iid.
    async fn resolve_iid_from_source_branch(
        &self,
        ep: &Endpoint,
        args: &mut Map<String, Value>,
    ) -> Result<(), ToolError> {
        let wants_iid = ep.path_params.iter().any(|p| p == "merge_request_iid");
        if !wants_iid || args.contains_key("merge_request_iid") {
            return Ok(());
        }
        let Some(branch) = args.get("source_branch").and_then(Value::as_str) else {
            return Ok(());
        };
        let project = required_str(args, "project_id")?;
        let path = format!("/projects/{}/merge_requests", enc(&project));
        let query = vec![
            ("source_branch".to_string(), branch.to_string()),
            ("per_page".to_string(), "1".to_string()),
        ];
        let found = self.client.rest(Method::GET, &path, &query, None).await?;
        let iid = found
            .body
            .as_array()
            .and_then(|list| list.first())
            .and_then(|mr| mr.get("iid"))
            .cloned()
            .ok_or_else(|| {
                ToolError(format!("no merge request was found for source branch {branch:?}"))
            })?;
        args.insert("merge_request_iid".into(), iid);
        Ok(())
    }

    // ------------------------------------------------------------- GraphQL

    async fn call_graphql(
        &self,
        entry: &ToolEntry,
        args: Map<String, Value>,
    ) -> Result<Value, ToolError> {
        let ep = &entry.endpoint;

        // `execute_graphql` is a passthrough: the caller supplies the document.
        if entry.meta.name == "execute_graphql" {
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return err("execute_graphql requires a `query` string");
            };
            let variables = args.get("variables").cloned().unwrap_or_else(|| json!({}));
            return Ok(self.client.graphql(query, variables).await?.body);
        }

        let Some(query) = ep.query.as_deref() else {
            return err(format!(
                "{} is a GraphQL tool but carries no query document",
                entry.meta.name
            ));
        };

        // Several GraphQL calls need a namespace full path rather than the id the
        // caller supplied, and `move_work_item` needs two of them.
        let mut resolved = Map::new();
        if let Some(step) = &ep.pre_step {
            let mut targets: Vec<String> = vec![step.arg.clone()];
            for src in ep.variables.values() {
                if src.transform.as_deref() == Some("resolved_full_path")
                    && let Some(from) = &src.from
                    && !targets.contains(from)
                {
                    targets.push(from.clone());
                }
            }
            for arg in targets {
                let Some(raw) = args.get(&arg).and_then(as_scalar_string) else {
                    continue;
                };
                let needs_lookup = match step.when.as_deref() {
                    Some("numeric") => raw.chars().all(|c| c.is_ascii_digit()),
                    _ => true,
                };
                let value = if needs_lookup {
                    let path = step
                        .path
                        .replace(&format!("{{{}}}", step.arg), &enc(&raw));
                    let method = parse_method(&step.method)?;
                    let looked_up = self.client.rest(method, &path, &[], None).await?;
                    looked_up
                        .body
                        .get(&step.field)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            ToolError(format!("{path} did not return a {}", step.field))
                        })?
                } else {
                    raw
                };
                resolved.insert(arg, Value::String(value));
            }
        }

        // Work item mutations address their target by global id, which costs
        // one more GraphQL round trip.
        let transforms: Vec<&str> =
            ep.variables.values().filter_map(|v| v.transform.as_deref()).collect();
        if transforms
            .iter()
            .any(|t| matches!(*t, "work_item_gid" | "issue_gid_from_work_item" | "timeline_event_create_input"))
        {
            let gid = self.resolve_work_item_gid(&args, &resolved).await?;
            resolved.insert(WORK_ITEM_GID.into(), Value::String(gid));
        }
        if transforms.contains(&"work_item_type_gid") {
            let wanted = args
                .get("type")
                .or_else(|| args.get("new_type"))
                .and_then(Value::as_str)
                .unwrap_or("issue")
                .to_string();
            let gid = self.resolve_work_item_type_gid(&args, &resolved, &wanted).await?;
            resolved.insert(WORK_ITEM_TYPE_GID.into(), Value::String(gid));
        }

        let variables = build_variables(&ep.variables, &args, &resolved)?;
        let resp = self.client.graphql(query, Value::Object(variables.clone())).await?;
        check_mutation_errors(&resp.body)?;

        // Updating dependency proxy settings discards the mutation payload and
        // re-reads the settings with the path already resolved.
        if entry.meta.name == "update_dependency_proxy_settings" {
            let read = self
                .registry
                .get("get_dependency_proxy_settings")
                .ok_or_else(|| ToolError("get_dependency_proxy_settings is missing".into()))?;
            let follow_query = read
                .endpoint
                .query
                .as_deref()
                .ok_or_else(|| ToolError("the settings query is missing".into()))?;
            let follow_vars = build_variables(&read.endpoint.variables, &args, &resolved)?;
            let follow = self.client.graphql(follow_query, Value::Object(follow_vars)).await?;
            check_mutation_errors(&follow.body)?;
            return Ok(unwrap_path(follow.body, &read.endpoint.result_path));
        }

        Ok(unwrap_path(resp.body, &ep.result_path))
    }

    /// `namespace(fullPath).workItem(iid)` reduced to just the id.
    async fn resolve_work_item_gid(
        &self,
        args: &Map<String, Value>,
        resolved: &Map<String, Value>,
    ) -> Result<String, ToolError> {
        let path = resolved
            .get("project_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("the project path could not be resolved".into()))?;
        let iid = args
            .get("iid")
            .or_else(|| args.get("incident_iid"))
            .and_then(as_scalar_string)
            .ok_or_else(|| ToolError("iid is required".into()))?;

        const QUERY: &str =
            "query($path: ID!, $iid: String!) { namespace(fullPath: $path) { workItem(iid: $iid) { id } } }";
        let resp = self
            .client
            .graphql(QUERY, json!({ "path": path, "iid": iid }))
            .await?;
        resp.body
            .pointer("/data/namespace/workItem/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ToolError(format!("work item #{iid} was not found in {path}")))
    }

    /// Work item types are named by the caller but addressed by global id.
    async fn resolve_work_item_type_gid(
        &self,
        _args: &Map<String, Value>,
        resolved: &Map<String, Value>,
        wanted: &str,
    ) -> Result<String, ToolError> {
        let path = resolved
            .get("project_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("the project path could not be resolved".into()))?;

        const QUERY: &str =
            "query($path: ID!) { namespace(fullPath: $path) { workItemTypes { nodes { id name } } } }";
        let resp = self.client.graphql(QUERY, json!({ "path": path })).await?;
        let nodes = resp
            .body
            .pointer("/data/namespace/workItemTypes/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError(format!("{path} exposes no work item types")))?;

        let normalised = wanted.replace('_', " ");
        nodes
            .iter()
            .find(|n| {
                n.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case(&normalised))
            })
            .and_then(|n| n.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| {
                let available: Vec<&str> =
                    nodes.iter().filter_map(|n| n.get("name").and_then(Value::as_str)).collect();
                ToolError(format!(
                    "unknown work item type {wanted:?}; {path} offers: {}",
                    available.join(", ")
                ))
            })
    }

    // ----------------------------------------------------------- composite

    async fn call_composite(
        &self,
        entry: &ToolEntry,
        args: Map<String, Value>,
    ) -> Result<Value, ToolError> {
        match entry.meta.name.as_str() {
            "wait_for_pipeline" => {
                let project = required_str(&args, "project_id")?;
                let id = required_str(&args, "pipeline_id")?;
                let path = format!(
                    "/projects/{}/pipelines/{}",
                    enc(&project),
                    enc(&id)
                );
                self.poll_until_terminal(&path, &args, "Pipeline not found").await
            }
            "wait_for_job" => {
                let project = required_str(&args, "project_id")?;
                let id = required_str(&args, "job_id")?;
                let path = format!("/projects/{}/jobs/{}", enc(&project), enc(&id));
                self.poll_until_terminal(&path, &args, "Job not found").await
            }
            "play_pipeline_jobs" => self.play_pipeline_jobs(&args).await,
            "get_webhook_event" => self.get_webhook_event(&args).await,
            "create_branch" => self.create_branch(&args).await,
            "create_or_update_file" => self.create_or_update_file(&args).await,
            "my_issues" => self.my_issues(&args).await,
            "update_issue_description_patch" => self.update_issue_description_patch(&args).await,
            "get_users" => self.get_users(&args).await,
            "health_check" => self.health_check().await,
            other => err(format!(
                "{other} is not implemented in this server; it needs a multi-step mutation \
                 sequence that the reference implementation performs client-side"
            )),
        }
    }

    /// Fetch immediately, then poll until the object reaches a terminal status.
    async fn poll_until_terminal(
        &self,
        path: &str,
        args: &Map<String, Value>,
        not_found: &str,
    ) -> Result<Value, ToolError> {
        let timeout = Duration::from_secs(number_arg(args, "timeout_seconds", 300));
        let interval = Duration::from_secs(number_arg(args, "poll_interval_seconds", 5).max(1));
        let deadline = Instant::now() + timeout;

        loop {
            let status = match self.client.rest(Method::GET, path, &[], None).await {
                Ok(resp) => match resp.body.get("status").and_then(Value::as_str) {
                    Some(s) if TERMINAL_STATUSES.contains(&s) => return Ok(resp.body),
                    Some(s) => s.to_string(),
                    // Nothing to wait on: hand the object back as it is.
                    None => return Ok(resp.body),
                },
                Err(e) if e.status == StatusCode::NOT_FOUND => return err(not_found),
                Err(e) => return Err(e.into()),
            };

            let now = Instant::now();
            if now >= deadline {
                return err(format!(
                    "Timed out waiting for terminal status; last status: {status}"
                ));
            }
            tokio::time::sleep(interval.min(deadline - now)).await;
        }
    }

    async fn play_pipeline_jobs(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let project = required_str(args, "project_id")?;
        let Some(ids) = args.get("job_ids").and_then(Value::as_array) else {
            return err("job_ids must be an array of job ids");
        };
        let vars = args.get("job_variables_attributes").cloned();
        let send_body = vars
            .as_ref()
            .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));

        let mut completed = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(job_id) = as_scalar_string(id) else {
                return err("job_ids entries must be numbers or strings");
            };
            let play_path = format!("/projects/{}/jobs/{}/play", enc(&project), enc(&job_id));
            let body = send_body.then(|| json!({ "job_variables_attributes": vars.clone() }));
            let played = self
                .client
                .rest(Method::POST, &play_path, &[], body.as_ref())
                .await?;
            let played_id = played
                .body
                .get("id")
                .and_then(as_scalar_string)
                .unwrap_or(job_id);
            let watch_path = format!("/projects/{}/jobs/{}", enc(&project), enc(&played_id));
            completed.push(self.poll_until_terminal(&watch_path, args, "Job not found").await?);
        }
        Ok(Value::Array(completed))
    }

    /// GitLab exposes no single-event endpoint, so page the hook's event list.
    async fn get_webhook_event(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        const PER_PAGE: u64 = 20;
        const MAX_PAGES: u64 = 25;

        let hook_id = required_str(args, "hook_id")?;
        let base = match args.get("project_id").and_then(as_scalar_string) {
            Some(p) => format!("/projects/{}/hooks/{}/events", enc(&p), enc(&hook_id)),
            None => {
                let g = required_str(args, "group_id")?;
                format!("/groups/{}/hooks/{}/events", enc(&g), enc(&hook_id))
            }
        };
        let Some(wanted) = args.get("event_id").and_then(as_scalar_string) else {
            return err("event_id is required");
        };

        let single_page = args.get("page").and_then(as_scalar_string);
        let pages: Vec<u64> = match &single_page {
            Some(p) => vec![p.parse().unwrap_or(1)],
            None => (1..=MAX_PAGES).collect(),
        };

        for page in pages {
            let query = vec![
                ("page".to_string(), page.to_string()),
                ("per_page".to_string(), PER_PAGE.to_string()),
            ];
            let resp = self.client.rest(Method::GET, &base, &query, None).await?;
            let Some(items) = resp.body.as_array() else {
                return Ok(resp.body);
            };
            for item in items {
                if item.get("id").and_then(as_scalar_string).as_deref() == Some(wanted.as_str()) {
                    return Ok(item.clone());
                }
            }
            if (items.len() as u64) < PER_PAGE {
                break;
            }
        }

        match single_page {
            Some(p) => err(format!("webhook event {wanted} was not found on page {p}")),
            None => err(format!(
                "webhook event {wanted} was not found in the {} most recent events",
                PER_PAGE * MAX_PAGES
            )),
        }
    }

    /// A branch is cut from the project default when no ref is given.
    async fn create_branch(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let project = required_str(args, "project_id")?;
        let branch = required_str(args, "branch")?;
        let ref_name = match args.get("ref").and_then(as_scalar_string) {
            Some(r) if !r.is_empty() => r,
            _ => {
                let project_path = format!("/projects/{}", enc(&project));
                let info = self.client.rest(Method::GET, &project_path, &[], None).await?;
                info.body
                    .get("default_branch")
                    .and_then(Value::as_str)
                    .unwrap_or("main")
                    .to_string()
            }
        };
        let path = format!("/projects/{}/repository/branches", enc(&project));
        let body = json!({ "branch": branch, "ref": ref_name });
        Ok(self.client.rest(Method::POST, &path, &[], Some(&body)).await?.body)
    }

    /// Whether the file already exists decides between create and update.
    async fn create_or_update_file(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let project = required_str(args, "project_id")?;
        let file_path = required_str(args, "file_path")?;
        let branch = required_str(args, "branch")?;
        let path = format!(
            "/projects/{}/repository/files/{}",
            enc(&project),
            enc(&file_path)
        );

        let existing = self
            .client
            .rest(
                Method::GET,
                &path,
                &[("ref".to_string(), branch.clone())],
                None,
            )
            .await;

        let (method, existing_body) = match existing {
            Ok(resp) => (Method::PUT, Some(resp.body)),
            Err(e) if e.status == StatusCode::NOT_FOUND => (Method::POST, None),
            Err(e) => return Err(e.into()),
        };

        let mut body = Map::new();
        for key in [
            "branch",
            "content",
            "commit_message",
            "encoding",
            "previous_path",
            "author_email",
            "author_name",
            "start_branch",
        ] {
            if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
                body.insert(key.into(), v.clone());
            }
        }
        // Updating an existing file carries its commit ids so GitLab can detect
        // a concurrent write.
        for key in ["commit_id", "last_commit_id"] {
            let value = args
                .get(key)
                .filter(|v| !v.is_null())
                .cloned()
                .or_else(|| existing_body.as_ref().and_then(|b| b.get(key).cloned()));
            if let Some(v) = value {
                body.insert(key.into(), v);
            }
        }
        Ok(self
            .client
            .rest(method, &path, &[], Some(&Value::Object(body)))
            .await?
            .body)
    }

    /// Issues assigned to whoever the token belongs to.
    async fn my_issues(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let me = self.client.rest(Method::GET, "/user", &[], None).await?.body;
        let path = match args.get("project_id").and_then(as_scalar_string) {
            Some(p) if !p.is_empty() => format!("/projects/{}/issues", enc(&p)),
            _ => "/issues".to_string(),
        };

        let mut query: Vec<(String, String)> = Vec::new();
        match me.get("username").and_then(Value::as_str) {
            Some(u) => query.push(("assignee_username[]".into(), u.to_string())),
            None => {
                if let Some(id) = me.get("id").and_then(as_scalar_string) {
                    query.push(("assignee_id".into(), id));
                }
            }
        }
        query.push((
            "state".into(),
            args.get("state")
                .and_then(Value::as_str)
                .unwrap_or("opened")
                .to_string(),
        ));
        for key in [
            "labels",
            "milestone",
            "search",
            "created_after",
            "created_before",
            "updated_after",
            "updated_before",
            "per_page",
            "page",
        ] {
            if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
                if key == "labels"
                    && let Some(items) = v.as_array()
                {
                    for item in items.iter().filter_map(as_scalar_string) {
                        query.push(("labels[]".into(), item));
                    }
                    continue;
                }
                push_query(&mut query, key, v);
            }
        }
        Ok(self.client.rest(Method::GET, &path, &query, None).await?.body)
    }

    /// Edit an issue description without resending the whole body.
    async fn update_issue_description_patch(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Value, ToolError> {
        let project = required_str(args, "project_id")?;
        let iid = required_str(args, "issue_iid")?;
        let patch = args
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("patch is required".into()))?;
        let issue_path = format!("/projects/{}/issues/{}", enc(&project), enc(&iid));

        let issue = self.client.rest(Method::GET, &issue_path, &[], None).await?.body;
        let original = issue.get("description").and_then(Value::as_str).unwrap_or("");

        let patch_type = args.get("patch_type").and_then(Value::as_str).unwrap_or("search_replace");
        let allow_multiple = args.get("allow_multiple").and_then(Value::as_bool).unwrap_or(false);
        let updated = match patch_type {
            "search_replace" => apply_search_replace(original, patch, allow_multiple)?,
            "unified_diff" => apply_unified_diff(original, patch)?,
            other => return err(format!("unknown patch_type {other:?}")),
        };

        if args.get("dry_run").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(json!({
                "dry_run": true,
                "changed": updated != original,
                "description": updated,
            }));
        }

        let body = json!({ "description": updated });
        let saved = self
            .client
            .rest(Method::PUT, &issue_path, &[], Some(&body))
            .await?
            .body;

        let mut out = json!({ "issue": saved, "changed": updated != original });
        if args.get("create_note").and_then(Value::as_bool).unwrap_or(false) {
            let notes_path = format!("{issue_path}/notes");
            let note_body = json!({
                "body": "Updated issue description using patch-based tool.",
            });
            let note = match self
                .client
                .rest(Method::POST, &notes_path, &[], Some(&note_body))
                .await
            {
                Ok(r) => json!({ "status": "created", "note": r.body }),
                // A failed note must not fail the edit that already landed.
                Err(e) => json!({ "status": "failed", "error": e.to_string() }),
            };
            out["note"] = note;
        }
        Ok(out)
    }

    /// Look each username up separately; a miss is a null, not a failure.
    async fn get_users(&self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let Some(usernames) = args.get("usernames").and_then(Value::as_array) else {
            return err("usernames must be an array of strings");
        };
        let mut out = Map::new();
        for entry in usernames {
            let Some(username) = entry.as_str() else {
                return err("usernames entries must be strings");
            };
            let query = vec![("username".to_string(), username.to_string())];
            let found = match self.client.rest(Method::GET, "/users", &query, None).await {
                Ok(resp) => resp
                    .body
                    .as_array()
                    .and_then(|list| {
                        list.iter()
                            .find(|u| u.get("username").and_then(Value::as_str) == Some(username))
                            .cloned()
                    })
                    .unwrap_or(Value::Null),
                Err(_) => Value::Null,
            };
            out.insert(username.to_string(), found);
        }
        Ok(Value::Object(out))
    }

    /// Report whether the token works, and which GitLab it is talking to.
    async fn health_check(&self) -> Result<Value, ToolError> {
        let user = self.client.rest(Method::GET, "/user", &[], None).await;
        let authenticated = match &user {
            Ok(_) => true,
            Err(e) if matches!(e.status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                self.client.rest(Method::GET, "/job", &[], None).await.is_ok()
            }
            Err(_) => false,
        };

        let mut out = json!({
            "status": if authenticated { "ok" } else { "error" },
            "authenticated": authenticated,
            "gitlab_url": self.api_url,
        });
        if !authenticated {
            if let Err(e) = user {
                out["error"] = json!(e.to_string());
            }
            return Ok(out);
        }
        // A missing version endpoint must not turn a healthy server unhealthy.
        if let Ok(v) = self.client.rest(Method::GET, "/version", &[], None).await {
            for key in ["version", "revision", "enterprise"] {
                if let Some(value) = v.body.get(key) {
                    out[key] = value.clone();
                }
            }
        }
        Ok(out)
    }

    // --------------------------------------------------------------- local

    fn call_local(&self, entry: &ToolEntry, args: Map<String, Value>) -> Result<Value, ToolError> {
        if entry.meta.name != "discover_tools" {
            return err(format!("{} has no local implementation", entry.meta.name));
        }
        let active = self.registry.active_categories();

        let Some(category) = args.get("category").and_then(Value::as_str) else {
            let categories: Vec<Value> = self
                .registry
                .toolsets()
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "toolCount": t.tools.len(),
                        "active": active.contains(&t.id),
                        "isDefault": t.is_default,
                    })
                })
                .collect();
            return Ok(json!({
                "categories": categories,
                "hint": "call discover_tools again with a category id to expose that group of tools",
            }));
        };

        if !self.registry.activate(category) {
            let valid: Vec<&str> =
                self.registry.toolsets().iter().map(|t| t.id.as_str()).collect();
            return err(format!(
                "unknown category {category:?}; valid categories: {}",
                valid.join(", ")
            ));
        }

        let now_active = self.registry.active_categories();
        Ok(json!({
            "activated": category,
            "activeCategories": now_active,
            "toolCount": self.registry.visible().len(),
        }))
    }
}

// ------------------------------------------------------------------ helpers

fn parse_method(s: &str) -> Result<Method, ToolError> {
    Method::from_bytes(s.as_bytes()).map_err(|_| ToolError(format!("unsupported HTTP method {s}")))
}

fn enc(value: &str) -> String {
    utf8_percent_encode(value, SEGMENT).to_string()
}

fn enc_multi(value: &str) -> String {
    utf8_percent_encode(value, MULTI_SEGMENT).to_string()
}

/// Numbers, strings and booleans all reach us as JSON; render them uniformly.
fn as_scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn required_str(args: &Map<String, Value>, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(as_scalar_string)
        .ok_or_else(|| ToolError(format!("{key} is required")))
}

fn number_arg(args: &Map<String, Value>, key: &str, default: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(default)
}

/// An argument counts as present when it is set and not falsy.
fn is_present(args: &Map<String, Value>, key: &str) -> bool {
    match args.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Pick the route, honouring variants that switch on an argument.
fn select_route(
    ep: &Endpoint,
    args: &Map<String, Value>,
) -> Result<(String, String, Vec<String>), ToolError> {
    if !ep.path_variants.is_empty() {
        for v in &ep.path_variants {
            if is_present(args, &v.when.param) == v.when.present {
                let method = v
                    .method
                    .clone()
                    .or_else(|| ep.method.clone())
                    .unwrap_or_else(|| "GET".into());
                return Ok((method, v.path.clone(), v.path_params.clone()));
            }
        }
        return err("no path variant matched the supplied arguments");
    }
    let (Some(method), Some(path)) = (ep.method.clone(), ep.path.clone()) else {
        return err("this tool has no REST route");
    };
    Ok((method, path, ep.path_params.clone()))
}

fn render_path(
    template: &str,
    params: &[String],
    args: &Map<String, Value>,
) -> Result<String, ToolError> {
    let mut path = template.to_string();
    for name in params {
        let Some(raw) = args.get(name).and_then(as_scalar_string) else {
            return err(format!("{name} is required"));
        };
        let encoded = if MULTI_SEGMENT_PARAMS.contains(&name.as_str()) {
            enc_multi(&raw)
        } else {
            enc(&raw)
        };
        path = path.replace(&format!("{{{name}}}"), &encoded);
    }
    if path.contains('{') {
        return err(format!("path {path} still has unfilled parameters"));
    }
    Ok(path)
}

/// Query-string pairs plus an optional JSON body.
type RequestParts = (Vec<(String, String)>, Option<Value>);

/// Split the remaining arguments between the query string and the JSON body.
fn split_args(
    ep: &Endpoint,
    method: &Method,
    path_params: &[String],
    args: &Map<String, Value>,
) -> Result<RequestParts, ToolError> {
    let body_carrying = matches!(*method, Method::POST | Method::PUT | Method::PATCH);
    let mut query = Vec::new();
    let mut body = Map::new();

    for (key, value) in args {
        if path_params.iter().any(|p| p == key) || META_ARGS.contains(&key.as_str()) {
            continue;
        }
        if value.is_null() {
            continue;
        }
        let forced_query = ep.query_params.iter().any(|p| p == key);
        if body_carrying && !forced_query {
            body.insert(key.clone(), value.clone());
        } else {
            push_query(&mut query, key, value);
        }
    }

    let body = (body_carrying && !body.is_empty()).then(|| Value::Object(body));
    Ok((query, body))
}

/// GitLab takes repeated values comma-joined and nested filters bracketed.
fn push_query(out: &mut Vec<(String, String)>, key: &str, value: &Value) {
    match value {
        Value::Array(items) => {
            let joined: Vec<String> = items.iter().filter_map(as_scalar_string).collect();
            if !joined.is_empty() {
                out.push((key.to_string(), joined.join(",")));
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                if let Some(s) = as_scalar_string(v) {
                    out.push((format!("{key}[{k}]"), s));
                }
            }
        }
        other => {
            if let Some(s) = as_scalar_string(other) {
                out.push((key.to_string(), s));
            }
        }
    }
}

fn build_variables(
    spec: &std::collections::BTreeMap<String, VariableSource>,
    args: &Map<String, Value>,
    resolved: &Map<String, Value>,
) -> Result<Map<String, Value>, ToolError> {
    let mut out = Map::new();
    for (name, src) in spec {
        if let Some(fixed) = &src.value {
            out.insert(name.clone(), fixed.clone());
            continue;
        }
        let raw = src
            .from
            .as_ref()
            .and_then(|f| args.get(f))
            .cloned()
            .filter(|v| !v.is_null());

        let value = match src.transform.as_deref() {
            None => raw.or_else(|| src.default.clone()),
            Some(t) => {
                apply_transform(t, raw, args, resolved, src.from.as_deref(), src.default.as_ref())?
            }
        };
        if let Some(v) = value {
            out.insert(name.clone(), v);
        }
    }
    Ok(out)
}

/// Value shaping that GitLab's GraphQL schema requires but the tool schema does
/// not express. Each name is referenced from `data/endpoints.json`.
fn apply_transform(
    name: &str,
    raw: Option<Value>,
    args: &Map<String, Value>,
    resolved: &Map<String, Value>,
    from: Option<&str>,
    default: Option<&Value>,
) -> Result<Option<Value>, ToolError> {
    match name {
        // Resolved per argument, so a tool moving between two projects gets
        // both paths rather than one shared value.
        "resolved_full_path" => Ok(from
            .and_then(|f| resolved.get(f))
            .cloned()
            .or(raw)
            .or_else(|| default.cloned())),

        // GitLab types some ids as String even when the caller sends a number.
        "string" => Ok(raw
            .as_ref()
            .and_then(as_scalar_string)
            .map(Value::String)
            .or_else(|| default.cloned())),

        // Already a valid IssuableState; forwarded untouched.
        "state_passthrough" => Ok(raw.or_else(|| default.cloned())),

        "work_item_type_enum" => Ok(raw
            .as_ref()
            .and_then(Value::as_str)
            .map(|v| json!(v.to_uppercase()))),

        "work_item_type_enums" => Ok(raw.as_ref().and_then(Value::as_array).map(|items| {
            json!(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|v| v.to_uppercase())
                    .collect::<Vec<_>>()
            )
        })),

        "work_item_gid" => Ok(resolved.get(WORK_ITEM_GID).cloned()),

        "work_item_type_gid" => Ok(resolved.get(WORK_ITEM_TYPE_GID).cloned()),

        // Timeline events address the same object as an Issue, not a WorkItem.
        "issue_gid_from_work_item" => Ok(resolved
            .get(WORK_ITEM_GID)
            .and_then(Value::as_str)
            .map(|g| json!(g.replace("/WorkItem/", "/Issue/")))),

        "timeline_event_create_input" => {
            let incident = resolved
                .get(WORK_ITEM_GID)
                .and_then(Value::as_str)
                .map(|g| g.replace("/WorkItem/", "/Issue/"))
                .ok_or_else(|| ToolError("the incident could not be resolved".into()))?;
            let mut input = Map::new();
            input.insert("incidentId".into(), json!(incident));
            input.insert(
                "note".into(),
                args.get("note")
                    .cloned()
                    .ok_or_else(|| ToolError("note is required".into()))?,
            );
            if let Some(v) = args.get("occurred_at").filter(|v| !v.is_null()) {
                input.insert("occurredAt".into(), v.clone());
            }
            if let Some(v) = args.get("tag_names").filter(|v| !v.is_null()) {
                input.insert("tagNames".into(), v.clone());
            }
            Ok(Some(Value::Object(input)))
        }

        // GitLab caps this page size at 100.
        "min_100" => {
            let n = raw
                .as_ref()
                .and_then(Value::as_u64)
                .or_else(|| default.and_then(Value::as_u64))
                .unwrap_or(20);
            Ok(Some(json!(n.min(100))))
        }

        // A single lowercase enum becomes a one-element uppercase list.
        "upper_single_array" => Ok(raw
            .as_ref()
            .and_then(Value::as_str)
            .map(|s| json!([s.to_uppercase()]))),

        "vulnerability_gid" => {
            let id = raw
                .as_ref()
                .and_then(as_scalar_string)
                .ok_or_else(|| ToolError("vulnerability_id is required".into()))?;
            Ok(Some(Value::String(vulnerability_gid(&id)?)))
        }

        "dependency_proxy_settings_input" => {
            let group_path = resolved
                .get("resolved_full_path")
                .cloned()
                .or_else(|| raw.clone())
                .ok_or_else(|| ToolError("group_id is required".into()))?;
            let mut input = Map::new();
            input.insert("groupPath".into(), group_path);
            for key in ["enabled", "identity", "secret"] {
                if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
                    input.insert(key.into(), v.clone());
                }
            }
            if input.len() == 1 {
                return err("At least one of enabled, identity, or secret must be provided");
            }
            Ok(Some(Value::Object(input)))
        }

        "vulnerability_dismiss_input" | "vulnerability_confirm_input" => {
            let id = raw
                .as_ref()
                .and_then(as_scalar_string)
                .ok_or_else(|| ToolError("vulnerability_id is required".into()))?;
            let mut input = Map::new();
            input.insert("id".into(), Value::String(vulnerability_gid(&id)?));
            if name == "vulnerability_dismiss_input" {
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError("reason is required".into()))?;
                input.insert("dismissalReason".into(), json!(reason.to_uppercase()));
            }
            if let Some(c) = args.get("comment").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                input.insert("comment".into(), json!(c));
            }
            Ok(Some(Value::Object(input)))
        }

        other => err(format!("unknown variable transform {other:?}")),
    }
}

fn vulnerability_gid(id: &str) -> Result<String, ToolError> {
    if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() {
        return Ok(format!("gid://gitlab/Vulnerability/{id}"));
    }
    if id.starts_with("gid://gitlab/Vulnerability/") {
        return Ok(id.to_string());
    }
    Err(ToolError(format!(
        "vulnerability_id must be a number or a gid://gitlab/Vulnerability/<n>, got {id:?}"
    )))
}

fn unwrap_path(mut value: Value, path: &[String]) -> Value {
    for key in path {
        match value.get(key) {
            Some(v) => value = v.clone(),
            None => return value,
        }
    }
    value
}

/// GitLab mutations answer with HTTP 200 and report failure in a payload-level
/// `errors` array, which the transport layer cannot see.
fn check_mutation_errors(body: &Value) -> Result<(), ToolError> {
    let Some(data) = body.get("data").and_then(Value::as_object) else {
        return Ok(());
    };
    for (field, payload) in data {
        let Some(errors) = payload.get("errors").and_then(Value::as_array) else {
            continue;
        };
        let messages: Vec<String> = errors.iter().filter_map(as_scalar_string).collect();
        if !messages.is_empty() {
            return err(format!("{field} failed: {}", messages.join(", ")));
        }
    }
    Ok(())
}

/// Apply `<<<<<<< SEARCH / ======= / >>>>>>> REPLACE` blocks to a text.
fn apply_search_replace(
    original: &str,
    patch: &str,
    allow_multiple: bool,
) -> Result<String, ToolError> {
    let mut out = original.to_string();
    let mut applied = 0usize;

    let mut search: Option<Vec<&str>> = None;
    let mut replace: Option<Vec<&str>> = None;
    for line in patch.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("<<<<<<<") {
            search = Some(Vec::new());
            replace = None;
        } else if trimmed.starts_with("=======") && search.is_some() {
            replace = Some(Vec::new());
        } else if trimmed.starts_with(">>>>>>>") {
            let (Some(s), Some(r)) = (search.take(), replace.take()) else {
                continue;
            };
            let needle = s.join("\n");
            let value = r.join("\n");
            if needle.is_empty() || !out.contains(&needle) {
                return err(format!(
                    "the search block was not found in the description: {:?}",
                    needle.chars().take(60).collect::<String>()
                ));
            }
            if !allow_multiple && out.matches(&needle).count() > 1 {
                return err(
                    "the search block matches more than once; set allow_multiple to replace every occurrence",
                );
            }
            out = if allow_multiple {
                out.replace(&needle, &value)
            } else {
                out.replacen(&needle, &value, 1)
            };
            applied += 1;
        } else if let Some(r) = replace.as_mut() {
            r.push(line);
        } else if let Some(s) = search.as_mut() {
            s.push(line);
        }
    }

    if applied == 0 {
        return err("No valid search/replace blocks found");
    }
    Ok(out)
}

/// Apply a unified diff by rebuilding each hunk's before and after text.
fn apply_unified_diff(original: &str, patch: &str) -> Result<String, ToolError> {
    let mut out = original.to_string();
    let mut applied = 0usize;
    let mut before: Vec<&str> = Vec::new();
    let mut after: Vec<&str> = Vec::new();
    let mut in_hunk = false;

    let flush = |before: &mut Vec<&str>,
                     after: &mut Vec<&str>,
                     out: &mut String,
                     applied: &mut usize|
     -> Result<(), ToolError> {
        if before.is_empty() && after.is_empty() {
            return Ok(());
        }
        let needle = before.join("\n");
        let value = after.join("\n");
        if !needle.is_empty() && !out.contains(&needle) {
            return err(format!(
                "hunk context was not found in the description: {:?}",
                needle.chars().take(60).collect::<String>()
            ));
        }
        *out = out.replacen(&needle, &value, 1);
        *applied += 1;
        before.clear();
        after.clear();
        Ok(())
    };

    for line in patch.lines() {
        if line.starts_with("@@") {
            flush(&mut before, &mut after, &mut out, &mut applied)?;
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with("---") || line.starts_with("+++") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'-') => before.push(&line[1..]),
            Some(b'+') => after.push(&line[1..]),
            Some(b' ') => {
                before.push(&line[1..]);
                after.push(&line[1..]);
            }
            None => {
                before.push("");
                after.push("");
            }
            _ => {}
        }
    }
    flush(&mut before, &mut after, &mut out, &mut applied)?;

    if applied == 0 {
        return err("the unified diff contained no hunks");
    }
    Ok(out)
}

/// Remove secret-bearing fields from a response before the model sees them.
fn redact(value: &mut Value, fields: &[String]) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(|i| redact(i, fields)),
        Value::Object(map) => {
            for f in fields {
                map.remove(f);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn encodes_slashes_in_project_paths() {
        let a = args(&[("project_id", json!("group/sub/project")), ("issue_iid", json!(7))]);
        let path = render_path(
            "/projects/{project_id}/issues/{issue_iid}",
            &["project_id".into(), "issue_iid".into()],
            &a,
        )
        .unwrap();
        assert_eq!(path, "/projects/group%2Fsub%2Fproject/issues/7");
    }

    #[test]
    fn keeps_slashes_in_multi_segment_params() {
        let a = args(&[("direct_asset_path", json!("bin/linux/tool.tar.gz"))]);
        let path =
            render_path("/x/{direct_asset_path}", &["direct_asset_path".into()], &a).unwrap();
        assert_eq!(path, "/x/bin/linux/tool.tar.gz");
    }

    #[test]
    fn missing_path_parameter_is_reported_by_name() {
        let e = render_path("/p/{project_id}", &["project_id".into()], &Map::new()).unwrap_err();
        assert!(e.0.contains("project_id is required"));
    }

    #[test]
    fn get_puts_everything_in_the_query_string() {
        let ep = Endpoint {
            kind: Kind::Rest,
            category: "issues".into(),
            read_only: true,
            destructive: false,
            method: Some("GET".into()),
            path: Some("/projects/{project_id}/issues".into()),
            path_params: vec!["project_id".into()],
            path_variants: vec![],
            query: None,
            variables: Default::default(),
            pre_step: None,
            result_path: vec![],
            query_params: vec![],
            redact: vec![],
            wrap: None,
        };
        let a = args(&[
            ("project_id", json!("x/y")),
            ("state", json!("opened")),
            ("labels", json!(["bug", "p1"])),
            ("full_response", json!(true)),
        ]);
        let (q, b) = split_args(&ep, &Method::GET, &["project_id".into()], &a).unwrap();
        assert!(b.is_none(), "GET must not carry a body");
        assert!(q.contains(&("state".into(), "opened".into())));
        assert!(q.contains(&("labels".into(), "bug,p1".into())));
        assert!(!q.iter().any(|(k, _)| k == "full_response"), "meta args are stripped");
        assert!(!q.iter().any(|(k, _)| k == "project_id"), "path args are not repeated");
    }

    #[test]
    fn post_puts_everything_in_the_body_except_forced_query_params() {
        let ep = Endpoint {
            kind: Kind::Rest,
            category: "ci".into(),
            read_only: false,
            destructive: false,
            method: Some("POST".into()),
            path: Some("/p/{project_id}/e".into()),
            path_params: vec!["project_id".into()],
            path_variants: vec![],
            query: None,
            variables: Default::default(),
            pre_step: None,
            result_path: vec![],
            query_params: vec!["force".into()],
            redact: vec![],
            wrap: None,
        };
        let a = args(&[("project_id", json!(1)), ("name", json!("prod")), ("force", json!(true))]);
        let (q, b) = split_args(&ep, &Method::POST, &["project_id".into()], &a).unwrap();
        assert_eq!(q, vec![("force".to_string(), "true".to_string())]);
        let body = b.expect("body present");
        assert_eq!(body["name"], json!("prod"));
        assert!(body.get("force").is_none(), "forced query params leave the body");
    }

    #[test]
    fn nested_filters_become_bracketed_query_keys() {
        let mut q = Vec::new();
        push_query(&mut q, "filter", &json!({"environment_scope": "prod"}));
        assert_eq!(q, vec![("filter[environment_scope]".to_string(), "prod".to_string())]);
    }

    #[test]
    fn variant_selection_follows_argument_presence() {
        let ep = Endpoint {
            kind: Kind::Rest,
            category: "webhooks".into(),
            read_only: true,
            destructive: false,
            method: None,
            path: None,
            path_params: vec![],
            path_variants: vec![
                crate::registry::PathVariant {
                    when: crate::registry::VariantCondition {
                        param: "project_id".into(),
                        present: true,
                    },
                    method: Some("GET".into()),
                    path: "/projects/{project_id}/hooks".into(),
                    path_params: vec!["project_id".into()],
                },
                crate::registry::PathVariant {
                    when: crate::registry::VariantCondition {
                        param: "project_id".into(),
                        present: false,
                    },
                    method: Some("GET".into()),
                    path: "/groups/{group_id}/hooks".into(),
                    path_params: vec!["group_id".into()],
                },
            ],
            query: None,
            variables: Default::default(),
            pre_step: None,
            result_path: vec![],
            query_params: vec![],
            redact: vec![],
            wrap: None,
        };
        let (_, p, _) = select_route(&ep, &args(&[("group_id", json!("g"))])).unwrap();
        assert_eq!(p, "/groups/{group_id}/hooks");
        let (_, p, _) = select_route(&ep, &args(&[("project_id", json!("p"))])).unwrap();
        assert_eq!(p, "/projects/{project_id}/hooks");
    }

    /// Guards against a data change introducing a composite with no handler.
    #[test]
    fn composite_tools_are_either_implemented_or_declared_unimplemented() {
        let data: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_str(include_str!("../data/endpoints.json")).unwrap();
        let composites: Vec<&String> = data
            .iter()
            .filter(|(_, v)| v.get("kind").and_then(Value::as_str) == Some("composite"))
            .map(|(k, _)| k)
            .collect();
        assert!(!composites.is_empty());
        let unimplemented: Vec<&&String> = composites
            .iter()
            .filter(|n| !IMPLEMENTED_COMPOSITES.contains(&n.as_str()))
            .collect();
        assert_eq!(
            unimplemented,
            vec![&&"update_work_item".to_string()],
            "a composite tool gained or lost an implementation"
        );
    }

    #[test]
    fn search_replace_rewrites_the_matching_block() {
        let out = apply_search_replace(
            "line one\nold text\nline three",
            "<<<<<<< SEARCH\nold text\n=======\nnew text\n>>>>>>> REPLACE",
            false,
        )
        .unwrap();
        assert_eq!(out, "line one\nnew text\nline three");
    }

    #[test]
    fn search_replace_refuses_an_ambiguous_match() {
        let e = apply_search_replace(
            "dup\ndup",
            "<<<<<<< SEARCH\ndup\n=======\nx\n>>>>>>> REPLACE",
            false,
        )
        .unwrap_err();
        assert!(e.0.contains("more than once"));
    }

    #[test]
    fn search_replace_can_replace_every_occurrence() {
        let out = apply_search_replace(
            "dup\ndup",
            "<<<<<<< SEARCH\ndup\n=======\nx\n>>>>>>> REPLACE",
            true,
        )
        .unwrap();
        assert_eq!(out, "x\nx");
    }

    #[test]
    fn search_replace_reports_a_missing_block() {
        let e = apply_search_replace(
            "text",
            "<<<<<<< SEARCH\nabsent\n=======\nx\n>>>>>>> REPLACE",
            false,
        )
        .unwrap_err();
        assert!(e.0.contains("was not found"));
    }

    #[test]
    fn empty_patch_is_rejected() {
        assert!(apply_search_replace("text", "nothing here", false).is_err());
    }

    #[test]
    fn unified_diff_applies_a_hunk_with_context() {
        let out = apply_unified_diff(
            "alpha\nbeta\ngamma",
            "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma",
        )
        .unwrap();
        assert_eq!(out, "alpha\nBETA\ngamma");
    }

    #[test]
    fn unified_diff_reports_missing_context() {
        let e = apply_unified_diff("alpha", "@@ -1 +1 @@\n-absent\n+x").unwrap_err();
        assert!(e.0.contains("was not found"));
    }

    #[test]
    fn redaction_strips_secrets_from_objects_and_arrays() {
        let mut v = json!([{"id": 1, "token": "glptt-secret"}, {"id": 2, "token": "x"}]);
        redact(&mut v, &["token".to_string()]);
        assert_eq!(v, json!([{"id": 1}, {"id": 2}]));
    }

    #[test]
    fn graphql_variables_apply_defaults_and_transforms() {
        let mut spec = std::collections::BTreeMap::new();
        spec.insert(
            "first".to_string(),
            VariableSource { from: Some("limit".into()), default: Some(json!(20)), transform: None, value: None },
        );
        spec.insert(
            "fullPath".to_string(),
            VariableSource { from: Some("group_id".into()), default: None, transform: Some("resolved_full_path".into()), value: None },
        );
        let mut resolved = Map::new();
        resolved.insert("group_id".into(), json!("acme/platform"));
        let vars = build_variables(&spec, &args(&[("group_id", json!(42))]), &resolved).unwrap();
        assert_eq!(vars["first"], json!(20), "default applies when the arg is absent");
        assert_eq!(vars["fullPath"], json!("acme/platform"), "transform wins over the raw arg");
    }
}
