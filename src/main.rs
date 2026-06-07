use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use axum::Router;
use rmcp::{
    Json,
    schemars, tool, tool_handler, tool_router, ServerHandler,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    transport::streamable_http_server::{
        tower::{StreamableHttpService, StreamableHttpServerConfig},
        session::local::LocalSessionManager,
    },
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const DEFAULT_READ_LIMIT: usize = 2000;
const MAX_READ_LIMIT: usize = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_READ_BYTES: usize = 50 * 1024;
const DEFAULT_GREP_RESULTS: usize = 100;
const MAX_GREP_RESULTS: usize = 300;
const MAX_GLOB_RESULTS: usize = 1000;

#[derive(Deserialize, Clone)]
struct ProjectConfig {
    root: PathBuf,
}

#[derive(Deserialize, Clone)]
struct Config {
    projects: HashMap<String, ProjectConfig>,
}

fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| "failed to parse config")?;
    Ok(config)
}

fn load_allowed_hosts() -> Vec<String> {
    std::env::var("PASTALESS_ALLOWED_HOSTS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|hosts| !hosts.is_empty())
        .unwrap_or_else(|| {
            vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string(),
            ]
        })
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListRequest {
    project_key: String,
    path: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GlobRequest {
    project_key: String,
    pattern: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadRequest {
    project_key: String,
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GrepRequest {
    project_key: String,
    pattern: String,
    path: Option<String>,
    case_sensitive: Option<bool>,
    max_results: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchTextRequest {
    project_key: String,
    query: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ProjectListResponse {
    projects: Vec<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum EntryKind {
    File,
    Dir,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ListEntry {
    name: String,
    kind: EntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ListResponse {
    entries: Vec<ListEntry>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct GlobResponse {
    paths: Vec<String>,
    truncated: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ReadResponse {
    path: String,
    start_line: usize,
    end_line: usize,
    total_lines: usize,
    content: String,
    truncated: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct GrepResponse {
    matches: Vec<GrepMatch>,
    truncated: bool,
}

#[derive(Clone)]
#[allow(dead_code)]
struct Pastaless {
    tool_router: ToolRouter<Self>,
    projects: HashMap<String, ProjectConfig>,
}

impl Pastaless {
    fn new(projects: HashMap<String, ProjectConfig>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            projects,
        }
    }

    fn resolve_path(&self, project_key: &str, rel: &str) -> Result<PathBuf> {
        let config = self.projects.get(project_key)
            .ok_or_else(|| anyhow::anyhow!("unknown project: {project_key}"))?;

        let root = std::fs::canonicalize(&config.root)
            .with_context(|| format!("cannot access project root: {}", config.root.display()))?;

        if Path::new(rel).is_absolute() {
            anyhow::bail!("absolute paths are not allowed");
        }
        if rel.contains("..") {
            anyhow::bail!("path traversal is not allowed");
        }
        let candidate = root.join(rel);

        if Self::is_denied_path(&candidate) {
            anyhow::bail!("access denied to requested path");
        }

        let canonical = candidate.canonicalize()
            .with_context(|| format!("cannot access path: {rel}"))?;

        if !canonical.starts_with(&root) {
            anyhow::bail!("path escapes project root");
        }

        Ok(canonical)
    }

    fn project_root(&self, project_key: &str) -> Result<PathBuf> {
        let config = self.projects.get(project_key)
            .ok_or_else(|| anyhow::anyhow!("unknown project: {project_key}"))?;

        std::fs::canonicalize(&config.root)
            .with_context(|| format!("cannot access project root: {}", config.root.display()))
    }

    fn is_denied_path(path: &Path) -> bool {
        path.components().any(|component| {
            matches!(
                component.as_os_str(),
                name if name == OsStr::new(".env")
                    || name == OsStr::new(".git")
                    || name == OsStr::new("node_modules")
                    || name == OsStr::new("target")
                    || name == OsStr::new("dist")
                    || name == OsStr::new("build")
                    || name == OsStr::new(".import")
                    || name == OsStr::new(".godot")
            )
        })
    }

    fn normalize_relative(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    fn truncate_line(text: &str) -> (String, bool) {
        let mut truncated = false;
        let mut result = String::new();

        for (idx, ch) in text.chars().enumerate() {
            if idx >= MAX_LINE_LENGTH {
                truncated = true;
                break;
            }
            result.push(ch);
        }

        if truncated {
            result.push_str("...");
        }

        (result, truncated)
    }

    async fn grep_impl(&self, req: GrepRequest) -> Result<Json<GrepResponse>, String> {
        let root = self.project_root(&req.project_key).map_err(|e| e.to_string())?;
        let scope = req.path.unwrap_or_else(|| ".".to_string());
        let scope_path = self.resolve_path(&req.project_key, &scope)
            .map_err(|e| e.to_string())?;
        let scope_relative = scope_path.strip_prefix(&root)
            .map_err(|_| "path escapes project root".to_string())?;
        let scope_arg = if scope_relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            Self::normalize_relative(scope_relative)
        };
        let max_results = req.max_results.unwrap_or(DEFAULT_GREP_RESULTS).min(MAX_GREP_RESULTS).max(1);

        let mut command = Command::new("rg");
        command
            .args(["-n", "--color", "never", "--with-filename"])
            .arg(&req.pattern)
            .arg(&scope_arg)
            .current_dir(&root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if req.case_sensitive == Some(true) {
            command.arg("--case-sensitive");
        } else {
            command.arg("--ignore-case");
        }

        let output = command.output().await
            .map_err(|e| format!("failed to run ripgrep: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.code() == Some(1) {
                return Ok(Json(GrepResponse {
                    matches: Vec::new(),
                    truncated: false,
                }));
            }
            return Err(format!("ripgrep failed: {stderr}"));
        }

        let mut matches = Vec::new();
        let mut truncated = false;

        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.splitn(3, ':');
            let Some(path) = parts.next() else { continue; };
            let Some(line_number) = parts.next() else { continue; };
            let Some(text) = parts.next() else { continue; };

            let relative = Path::new(path);
            if Self::is_denied_path(relative) {
                continue;
            }

            let Ok(line) = line_number.parse::<usize>() else { continue; };
            let (text, line_truncated) = Self::truncate_line(text);
            matches.push(GrepMatch {
                path: Self::normalize_relative(relative),
                line,
                text,
            });

            if matches.len() >= max_results {
                truncated = true;
                break;
            }

            truncated |= line_truncated;
        }

        Ok(Json(GrepResponse { matches, truncated }))
    }
}

#[tool_router]
impl Pastaless {
    #[tool(description = "List all available project keys")]
    fn list_projects(&self) -> Result<Json<ProjectListResponse>, String> {
        let mut projects: Vec<String> = self.projects.keys().cloned().collect();
        projects.sort();
        Ok(Json(ProjectListResponse { projects }))
    }

    #[tool(description = "List files and directories in a project directory. Path is relative to the project root and defaults to the root.")]
    fn list(&self, params: Parameters<ListRequest>) -> Result<Json<ListResponse>, String> {
        let req = params.0;
        let rel = req.path.unwrap_or_default();
        let path = self
            .resolve_path(&req.project_key, &rel)
            .map_err(|e| e.to_string())?;

        if !path.is_dir() {
            return Err("requested path is not a directory".to_string());
        }

        let mut entries = std::fs::read_dir(&path)
            .map_err(|e| format!("cannot read directory: {e}"))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| !Self::is_denied_path(&entry.path()))
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                let kind = if metadata.is_dir() { EntryKind::Dir } else { EntryKind::File };
                let size = metadata.is_file().then_some(metadata.len());

                Some(ListEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    kind,
                    size,
                })
            })
            .collect::<Vec<_>>();

        entries.sort_by(|a, b| match (&a.kind, &b.kind) {
            (EntryKind::Dir, EntryKind::File) => std::cmp::Ordering::Less,
            (EntryKind::File, EntryKind::Dir) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });

        Ok(Json(ListResponse { entries }))
    }

    #[tool(description = "Find files in a project using a glob pattern like **/*.rs or **/package.json.")]
    async fn glob(&self, params: Parameters<GlobRequest>) -> Result<Json<GlobResponse>, String> {
        let req = params.0;
        let root = self.project_root(&req.project_key).map_err(|e| e.to_string())?;

        let output = Command::new("rg")
            .args(["--files", ".", "-g"])
            .arg(&req.pattern)
            .current_dir(&root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to run ripgrep: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ripgrep failed: {stderr}"));
        }

        let mut truncated = false;
        let mut paths = Vec::new();

        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let relative = Path::new(line);
            if Self::is_denied_path(relative) {
                continue;
            }

            paths.push(Self::normalize_relative(relative));
            if paths.len() >= MAX_GLOB_RESULTS {
                truncated = true;
                break;
            }
        }

        paths.sort();
        Ok(Json(GlobResponse { paths, truncated }))
    }

    #[tool(description = "Read a file from a project with line-based pagination. Path is relative to the project root.")]
    async fn read(
        &self,
        params: Parameters<ReadRequest>,
    ) -> Result<Json<ReadResponse>, String> {
        let req = params.0;
        let path = self.resolve_path(&req.project_key, &req.path)
            .map_err(|e| e.to_string())?;

        if !path.is_file() {
            return Err("requested path is not a file".to_string());
        }

        let meta = std::fs::metadata(&path).map_err(|e| format!("cannot stat file: {e}"))?;
        if meta.len() > 1_048_576 {
            return Err("file too large (max 1MB)".to_string());
        }

        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("failed to read file: {e}"))?;

        let lines: Vec<&str> = raw.lines().collect();
        let total_lines = lines.len();
        let start_line = req.offset.unwrap_or(1).max(1);
        let limit = req.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT).max(1);

        let mut end_line = start_line.saturating_sub(1);
        let mut content = String::new();
        let mut bytes = 0usize;
        let mut truncated = false;

        for (idx, line) in lines.iter().enumerate().skip(start_line.saturating_sub(1)).take(limit) {
            let line_number = idx + 1;
            let (display, line_truncated) = Self::truncate_line(line);
            let rendered = format!("{}: {}\n", line_number, display);

            if bytes + rendered.len() > MAX_READ_BYTES {
                truncated = true;
                break;
            }

            bytes += rendered.len();
            content.push_str(&rendered);
            end_line = line_number;
            truncated |= line_truncated;
        }

        if start_line.saturating_sub(1) + limit < total_lines {
            truncated = true;
        }

        Ok(Json(ReadResponse {
            path: req.path,
            start_line,
            end_line,
            total_lines,
            content,
            truncated,
        }))
    }

    #[tool(name = "read_file", description = "Compatibility alias for read. Read a file from a project with line-based pagination.")]
    async fn read_file(
        &self,
        params: Parameters<ReadRequest>,
    ) -> Result<Json<ReadResponse>, String> {
        self.read(params).await
    }

    #[tool(description = "Search for text or regex patterns in a project. Optionally limit the search to a relative subpath.")]
    async fn grep(
        &self,
        params: Parameters<GrepRequest>,
    ) -> Result<Json<GrepResponse>, String> {
        self.grep_impl(params.0).await
    }

    #[tool(name = "search_text", description = "Compatibility alias for grep. Search for text in a project.")]
    async fn search_text(
        &self,
        params: Parameters<SearchTextRequest>,
    ) -> Result<Json<GrepResponse>, String> {
        let req = params.0;
        self.grep_impl(GrepRequest {
            project_key: req.project_key,
            pattern: req.query,
            path: None,
            case_sensitive: None,
            max_results: None,
        }).await
    }
}

#[tool_handler(name = "pastaless", version = "0.1.0", instructions = "Read-only access to codebase projects. Use list_projects to discover valid project keys.")]
impl ServerHandler for Pastaless {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".to_string().into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config_path = std::env::var("PASTALESS_CONFIG")
        .unwrap_or_else(|_| "/app/config.toml".to_string());

    let config = load_config(Path::new(&config_path))?;
    let allowed_hosts = load_allowed_hosts();
    tracing::info!("loaded {} project(s)", config.projects.len());
    for key in config.projects.keys() {
        tracing::info!("  available project: {key}");
    }
    tracing::info!("allowed hosts: {}", allowed_hosts.join(", "));

    let pastaless = Pastaless::new(config.projects);

    let mcp_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(true)
        .with_allowed_hosts(allowed_hosts);

    let service = StreamableHttpService::new(
        move || Ok(pastaless.clone()),
        std::sync::Arc::new(LocalSessionManager::default()),
        mcp_config,
    );

    let app = Router::new()
        .route("/health", axum::routing::get(|| async { "OK" }))
        .nest_service("/mcp", service);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:6967").await?;
    tracing::info!("pastaless MCP server listening on 0.0.0.0:6967");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;

    Ok(())
}
