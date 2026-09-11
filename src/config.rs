//! Runtime configuration, assembled from environment variables and CLI flags.
//!
//! Environment names mirror the reference JavaScript server so an existing
//! `.mcp.json` keeps working after swapping the binary.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;

/// How the access token is presented to GitLab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// Personal / project / group access token via the `PRIVATE-TOKEN` header.
    PrivateToken,
    /// OAuth or CI job token via `Authorization: Bearer`.
    Bearer,
}

/// Which tool categories are exposed at startup.
#[derive(Debug, Clone)]
pub enum ToolsetSelection {
    /// Only the categories marked `is_default` in the tool data.
    Defaults,
    /// Every category, including opt-in ones.
    All,
    /// An explicit list of category ids.
    Explicit(BTreeSet<String>),
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Fully qualified REST base, e.g. `https://gitlab.example.com/api/v4`.
    pub api_url: String,
    /// GraphQL endpoint derived from the same instance root.
    pub graphql_url: String,
    pub token: String,
    pub auth: AuthKind,
    /// Injected when a tool argument omits `project_id`.
    pub default_project_id: Option<String>,
    /// Reject every tool that is not marked read-only.
    pub read_only: bool,
    /// Tools whose name matches are hidden and refused.
    pub denied_tools: Option<Regex>,
    /// When non-empty, only these projects may be addressed.
    pub allowed_project_ids: BTreeSet<String>,
    pub toolsets: ToolsetSelection,
    /// Legacy per-category switches from the reference server.
    pub use_wiki: bool,
    pub use_milestone: bool,
    pub use_pipeline: bool,
    pub timeout: Duration,
    /// Truncate tool output beyond this many bytes to protect the context window.
    pub max_response_bytes: usize,
    pub insecure_tls: bool,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn env_bool(key: &str, default: bool) -> bool {
    match env_opt(key) {
        None => default,
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
    }
}

/// Turn whatever the user configured into a REST base ending in `/api/v4`,
/// and a GraphQL base ending in `/api/graphql`.
fn normalize_urls(raw: &str) -> Result<(String, String)> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("GitLab API URL is empty");
    }
    let root = trimmed
        .strip_suffix("/api/v4")
        .or_else(|| trimmed.strip_suffix("/api/graphql"))
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    if !root.starts_with("http://") && !root.starts_with("https://") {
        bail!("GitLab API URL must start with http:// or https://, got {raw:?}");
    }
    Ok((format!("{root}/api/v4"), format!("{root}/api/graphql")))
}

/// Values recognised on the command line, for drop-in compatibility with the
/// reference server's `--token=... --api-url=...` invocation.
#[derive(Default)]
pub struct CliOverrides {
    pub token: Option<String>,
    pub api_url: Option<String>,
    pub toolsets: Option<String>,
    pub read_only: bool,
}

pub fn parse_cli<I: IntoIterator<Item = String>>(args: I) -> CliOverrides {
    let mut out = CliOverrides::default();
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next());
        match key.as_str() {
            "--token" | "--gitlab-token" => out.token = value(),
            "--api-url" | "--gitlab-api-url" | "--url" => out.api_url = value(),
            "--toolsets" => out.toolsets = value(),
            "--read-only" => out.read_only = true,
            _ => {}
        }
    }
    out
}

impl Config {
    pub fn from_env_and_cli(cli: CliOverrides) -> Result<Self> {
        let token = cli
            .token
            .or_else(|| env_opt("GITLAB_PERSONAL_ACCESS_TOKEN"))
            .or_else(|| env_opt("GITLAB_TOKEN"))
            .or_else(|| env_opt("CI_JOB_TOKEN"))
            .context(
                "no GitLab token: set GITLAB_PERSONAL_ACCESS_TOKEN (preferred) or pass --token",
            )?;

        let raw_url = cli
            .api_url
            .or_else(|| env_opt("GITLAB_API_URL"))
            .or_else(|| env_opt("GITLAB_URL"))
            .unwrap_or_else(|| "https://gitlab.com".to_string());
        let (api_url, graphql_url) = normalize_urls(&raw_url)?;

        let auth = match env_opt("GITLAB_AUTH_MODE").as_deref() {
            Some("bearer") | Some("oauth") => AuthKind::Bearer,
            Some("private") | Some("token") => AuthKind::PrivateToken,
            _ if token.starts_with("gloas-") => AuthKind::Bearer,
            _ => AuthKind::PrivateToken,
        };

        let denied_tools = match env_opt("GITLAB_DENIED_TOOLS_REGEX") {
            Some(p) => Some(
                Regex::new(&p).with_context(|| format!("GITLAB_DENIED_TOOLS_REGEX is not a valid regex: {p}"))?,
            ),
            None => None,
        };

        let allowed_project_ids = env_opt("GITLAB_ALLOWED_PROJECT_IDS")
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();

        let toolsets = match cli.toolsets.or_else(|| env_opt("GITLAB_TOOLSETS")) {
            None => ToolsetSelection::Defaults,
            Some(v) if v.eq_ignore_ascii_case("all") => ToolsetSelection::All,
            Some(v) if v.eq_ignore_ascii_case("default") => ToolsetSelection::Defaults,
            Some(v) => ToolsetSelection::Explicit(
                v.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
            ),
        };

        let timeout = env_opt("GITLAB_REQUEST_TIMEOUT_MS")
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(60));

        let max_response_bytes = env_opt("GITLAB_MAX_RESPONSE_BYTES")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1_000_000);

        Ok(Self {
            api_url,
            graphql_url,
            token,
            auth,
            default_project_id: env_opt("GITLAB_PROJECT_ID"),
            read_only: cli.read_only || env_bool("GITLAB_READ_ONLY_MODE", false),
            denied_tools,
            allowed_project_ids,
            toolsets,
            use_wiki: env_bool("USE_GITLAB_WIKI", false),
            use_milestone: env_bool("USE_MILESTONE", false),
            use_pipeline: env_bool("USE_PIPELINE", false),
            timeout,
            max_response_bytes,
            insecure_tls: env_bool("GITLAB_INSECURE_TLS", false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_bare_host() {
        let (rest, gql) = normalize_urls("https://gitlab.example.com").unwrap();
        assert_eq!(rest, "https://gitlab.example.com/api/v4");
        assert_eq!(gql, "https://gitlab.example.com/api/graphql");
    }

    #[test]
    fn normalizes_url_that_already_has_api_suffix() {
        let (rest, gql) = normalize_urls("https://git.example.com/api/v4/").unwrap();
        assert_eq!(rest, "https://git.example.com/api/v4");
        assert_eq!(gql, "https://git.example.com/api/graphql");
    }

    #[test]
    fn rejects_url_without_scheme() {
        assert!(normalize_urls("gitlab.example.com").is_err());
    }

    #[test]
    fn parses_inline_and_separate_cli_values() {
        let cli = parse_cli(
            ["--token=abc", "--api-url", "https://x.example/api/v4", "--read-only"]
                .map(String::from)
                .to_vec(),
        );
        assert_eq!(cli.token.as_deref(), Some("abc"));
        assert_eq!(cli.api_url.as_deref(), Some("https://x.example/api/v4"));
        assert!(cli.read_only);
    }
}
