//! The tool catalogue: 262 GitLab operations described as data.
//!
//! `data/tools.json` holds the MCP-facing names, descriptions and JSON schemas.
//! `data/endpoints.json` holds how each one maps onto the GitLab API. Both are
//! embedded at build time, so the binary needs no files at runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config::{Config, ToolsetSelection};

const TOOLS_JSON: &str = include_str!("../data/tools.json");
const ENDPOINTS_JSON: &str = include_str!("../data/endpoints.json");

#[derive(Debug, Deserialize)]
struct ToolsFile {
    tools: Vec<ToolMeta>,
    toolsets: Vec<Toolset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Toolset {
    pub id: String,
    pub is_default: bool,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A single REST call.
    #[default]
    Rest,
    /// A GraphQL document.
    Graphql,
    /// Several calls, or a polling loop.
    Composite,
    /// Answered by the server itself, without touching GitLab.
    Local,
}

/// One alternative path, chosen by whether an argument is present.
#[derive(Debug, Clone, Deserialize)]
pub struct PathVariant {
    pub when: VariantCondition,
    pub method: Option<String>,
    pub path: String,
    #[serde(default)]
    pub path_params: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VariantCondition {
    pub param: String,
    pub present: bool,
}

/// A REST lookup performed before the main call, to turn a numeric id into the
/// full path that GraphQL requires.
#[derive(Debug, Clone, Deserialize)]
pub struct PreStep {
    pub when: Option<String>,
    pub arg: String,
    pub method: String,
    pub path: String,
    pub field: String,
    /// Name the resolved value is stored under. Kept for readability of the
    /// data file; the executor keys resolutions by their source argument.
    #[allow(dead_code)]
    pub into: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VariableSource {
    pub from: Option<String>,
    pub default: Option<Value>,
    pub transform: Option<String>,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Endpoint {
    #[serde(default)]
    pub kind: Kind,
    pub category: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub destructive: bool,
    pub method: Option<String>,
    pub path: Option<String>,
    #[serde(default)]
    pub path_params: Vec<String>,
    #[serde(default)]
    pub path_variants: Vec<PathVariant>,
    /// GraphQL document, for `Kind::Graphql`.
    pub query: Option<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, VariableSource>,
    pub pre_step: Option<PreStep>,
    #[serde(default)]
    pub result_path: Vec<String>,
    /// Arguments that belong in the query string even on a write request.
    #[serde(default)]
    pub query_params: Vec<String>,
    /// Response fields stripped before the result reaches the model.
    #[serde(default)]
    pub redact: Vec<String>,
    /// How to reshape a list response so pagination is visible to the caller.
    pub wrap: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub meta: ToolMeta,
    pub endpoint: Endpoint,
}

pub struct Registry {
    entries: BTreeMap<String, Arc<ToolEntry>>,
    toolsets: Vec<Toolset>,
    /// Categories exposed right now. `discover_tools` can widen this at runtime.
    active: RwLock<BTreeSet<String>>,
    read_only: bool,
    denied: Option<regex::Regex>,
}

impl Registry {
    pub fn load(cfg: &Config) -> Result<Self> {
        let tools: ToolsFile =
            serde_json::from_str(TOOLS_JSON).context("data/tools.json is malformed")?;
        let endpoints: BTreeMap<String, Endpoint> =
            serde_json::from_str(ENDPOINTS_JSON).context("data/endpoints.json is malformed")?;

        let mut entries = BTreeMap::new();
        for meta in tools.tools {
            let endpoint = endpoints
                .get(&meta.name)
                .cloned()
                .with_context(|| format!("no endpoint mapping for tool {}", meta.name))?;
            entries.insert(meta.name.clone(), Arc::new(ToolEntry { meta, endpoint }));
        }

        let active = initial_categories(cfg, &tools.toolsets);

        Ok(Self {
            entries,
            toolsets: tools.toolsets,
            active: RwLock::new(active),
            read_only: cfg.read_only,
            denied: cfg.denied_tools.clone(),
        })
    }

    pub fn get(&self, name: &str) -> Option<Arc<ToolEntry>> {
        self.entries.get(name).cloned()
    }

    /// Tools currently offered to the client.
    pub fn visible(&self) -> Vec<Arc<ToolEntry>> {
        let active = self.active.read().expect("registry lock poisoned");
        self.entries
            .values()
            .filter(|e| self.is_allowed(e, &active))
            .cloned()
            .collect()
    }

    /// Whether a tool may be called, independent of whether it is listed.
    pub fn is_callable(&self, entry: &ToolEntry) -> bool {
        let active = self.active.read().expect("registry lock poisoned");
        self.is_allowed(entry, &active)
    }

    fn is_allowed(&self, entry: &ToolEntry, active: &BTreeSet<String>) -> bool {
        if self.read_only && !entry.endpoint.read_only {
            return false;
        }
        if let Some(re) = &self.denied
            && re.is_match(&entry.meta.name)
        {
            return false;
        }
        active.contains(&entry.endpoint.category)
    }

    pub fn toolsets(&self) -> &[Toolset] {
        &self.toolsets
    }

    pub fn active_categories(&self) -> BTreeSet<String> {
        self.active.read().expect("registry lock poisoned").clone()
    }

    /// Turn on one category. Returns false when the id is unknown.
    pub fn activate(&self, category: &str) -> bool {
        if !self.toolsets.iter().any(|t| t.id == category) {
            return false;
        }
        self.active
            .write()
            .expect("registry lock poisoned")
            .insert(category.to_string());
        true
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }
}

fn initial_categories(cfg: &Config, toolsets: &[Toolset]) -> BTreeSet<String> {
    let mut active: BTreeSet<String> = match &cfg.toolsets {
        ToolsetSelection::All => toolsets.iter().map(|t| t.id.clone()).collect(),
        ToolsetSelection::Defaults => {
            toolsets.iter().filter(|t| t.is_default).map(|t| t.id.clone()).collect()
        }
        ToolsetSelection::Explicit(ids) => {
            toolsets.iter().filter(|t| ids.contains(&t.id)).map(|t| t.id.clone()).collect()
        }
    };
    // `execute_graphql` and `discover_tools` belong to no category in the
    // reference server and are always offered.
    active.insert("core".into());

    // Legacy switches from the reference server stay honoured.
    if cfg.use_wiki {
        active.insert("wiki".into());
    }
    if cfg.use_milestone {
        active.insert("milestones".into());
    }
    if cfg.use_pipeline {
        active.insert("pipelines".into());
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            api_url: "https://gitlab.example.com/api/v4".into(),
            graphql_url: "https://gitlab.example.com/api/graphql".into(),
            token: "t".into(),
            auth: crate::config::AuthKind::PrivateToken,
            default_project_id: None,
            read_only: false,
            denied_tools: None,
            allowed_project_ids: Default::default(),
            toolsets: ToolsetSelection::Defaults,
            use_wiki: false,
            use_milestone: false,
            use_pipeline: false,
            timeout: std::time::Duration::from_secs(30),
            max_response_bytes: 1_000_000,
            insecure_tls: false,
        }
    }

    #[test]
    fn every_tool_has_an_endpoint() {
        let reg = Registry::load(&test_config()).expect("registry loads");
        assert_eq!(reg.entries.len(), 262, "expected the full GitLab tool set");
    }

    #[test]
    fn defaults_hide_opt_in_categories() {
        let reg = Registry::load(&test_config()).unwrap();
        let names: BTreeSet<_> = reg.visible().iter().map(|e| e.meta.name.clone()).collect();
        assert!(names.contains("get_issue"), "issues are a default category");
        assert!(!names.contains("list_releases"), "releases are opt-in");
        assert!(names.contains("discover_tools"), "the discovery tool is always offered");
    }

    #[test]
    fn all_selection_exposes_everything() {
        let mut cfg = test_config();
        cfg.toolsets = ToolsetSelection::All;
        let reg = Registry::load(&cfg).unwrap();
        assert_eq!(reg.visible().len(), 262);
    }

    #[test]
    fn read_only_mode_hides_mutations() {
        let mut cfg = test_config();
        cfg.read_only = true;
        cfg.toolsets = ToolsetSelection::All;
        let reg = Registry::load(&cfg).unwrap();
        let names: BTreeSet<_> = reg.visible().iter().map(|e| e.meta.name.clone()).collect();
        assert!(names.contains("get_issue"));
        assert!(!names.contains("create_issue"));
        assert!(!names.contains("delete_branch"));
    }

    #[test]
    fn denied_regex_removes_matching_tools() {
        let mut cfg = test_config();
        cfg.denied_tools = Some(regex::Regex::new("^delete_").unwrap());
        cfg.toolsets = ToolsetSelection::All;
        let reg = Registry::load(&cfg).unwrap();
        let names: BTreeSet<_> = reg.visible().iter().map(|e| e.meta.name.clone()).collect();
        assert!(!names.iter().any(|n| n.starts_with("delete_")));
    }

    #[test]
    fn activate_widens_the_visible_set() {
        let reg = Registry::load(&test_config()).unwrap();
        let before = reg.visible().len();
        assert!(reg.activate("releases"));
        assert!(!reg.activate("no_such_category"));
        assert!(reg.visible().len() > before);
    }

    #[test]
    fn rest_tools_all_carry_a_method_and_path() {
        let mut cfg = test_config();
        cfg.toolsets = ToolsetSelection::All;
        let reg = Registry::load(&cfg).unwrap();
        for e in reg.visible() {
            if e.endpoint.kind == Kind::Rest {
                let has_path = e.endpoint.path.is_some() || !e.endpoint.path_variants.is_empty();
                assert!(has_path, "{} has no path", e.meta.name);
                let has_method =
                    e.endpoint.method.is_some() || !e.endpoint.path_variants.is_empty();
                assert!(has_method, "{} has no method", e.meta.name);
            }
        }
    }

    /// A generated table once mapped whole tool families onto a resolver's URL
    /// rather than their own endpoint. These paths address a single project or
    /// group, so only tools that genuinely operate on one may use them.
    #[test]
    fn no_tool_addresses_a_bare_project_or_group_by_mistake() {
        const LEGITIMATE: &[&str] = &[
            "get_project",
            "update_project",
            "get_namespace",
            "list_projects",
            "search_repositories",
            "create_repository",
            "update_default_branch",
        ];
        let mut cfg = test_config();
        cfg.toolsets = ToolsetSelection::All;
        let reg = Registry::load(&cfg).unwrap();
        for e in reg.visible() {
            if e.endpoint.kind != Kind::Rest {
                continue;
            }
            let Some(path) = &e.endpoint.path else { continue };
            let bare = matches!(
                path.as_str(),
                "/projects/{project_id}" | "/groups/{group_id}" | "/groups/{project_id}"
            );
            assert!(
                !bare || LEGITIMATE.contains(&e.meta.name.as_str()),
                "{} points at {path}, which is a resolver URL rather than its own endpoint",
                e.meta.name
            );
        }
    }

    #[test]
    fn path_placeholders_exist_in_the_schema() {
        let mut cfg = test_config();
        cfg.toolsets = ToolsetSelection::All;
        let reg = Registry::load(&cfg).unwrap();
        for e in reg.visible() {
            let props = e
                .meta
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut paths: Vec<&String> = e.endpoint.path.iter().collect();
            for v in &e.endpoint.path_variants {
                paths.push(&v.path);
            }
            for p in paths {
                for seg in p.split('{').skip(1) {
                    let name = seg.split('}').next().unwrap_or_default();
                    assert!(
                        props.contains_key(name),
                        "{}: path parameter {{{}}} is not in the schema",
                        e.meta.name,
                        name
                    );
                }
            }
        }
    }
}
