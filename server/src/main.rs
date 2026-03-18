mod cache;
mod config;
mod formatting;
mod git_tools;
mod indexer;
mod model;
mod planning;
mod skills;
mod state;
mod text;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use rmcp::{
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter, Router},
        wrapper::Parameters,
    },
    model::{GetPromptResult, PromptMessage, PromptMessageRole, ServerCapabilities, ServerInfo},
    prompt, prompt_router, tool, tool_router,
    transport::stdio,
    ErrorData as McpError, Json, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use crate::{
    config::ServerConfig,
    formatting::{
        format_cache_status, format_context_bundle, format_memory_results, format_skill_results,
        format_task_decomposition, format_task_orchestration, format_warmup_status,
    },
    indexer::build_workspace_overview,
    model::{
        CacheStatus, ContextBundle, GitSummary, MemoryRecord, MemorySearchResults, SearchResults,
        SkillSearchResults, TaskDecomposition, TaskOrchestration, WarmupStatus, WorkspaceOverview,
    },
    state::AppState,
};

#[derive(Clone)]
struct CodexCompanionServer {
    state: AppState,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl CodexCompanionServer {
    fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
struct ForceRefreshArgs {
    #[serde(default)]
    pub force_refresh: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct SearchWorkspaceArgs {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub force_refresh: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct SearchSkillsArgs {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub force_refresh: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct RecallMemoryArgs {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct RememberMemoryArgs {
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub importance: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct RecentChangesArgs {
    #[serde(default)]
    pub limit_commits: Option<usize>,
    #[serde(default)]
    pub include_diffstat: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ContextBundleArgs {
    pub task: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub memory_limit: Option<usize>,
    #[serde(default)]
    pub force_refresh: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct DecomposeTaskArgs {
    pub task: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub memory_limit: Option<usize>,
    #[serde(default)]
    pub force_refresh: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct OrchestrateTaskArgs {
    pub task: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub memory_limit: Option<usize>,
    #[serde(default)]
    pub force_refresh: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct BootstrapPromptArgs {
    pub task: String,
    #[serde(default)]
    pub focus: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct HandoffPromptArgs {
    pub summary: String,
    #[serde(default)]
    pub next_session_goal: Option<String>,
}

#[tool_router(router = tool_router)]
impl CodexCompanionServer {
    #[tool(
        name = "warm_workspace",
        description = "Prewarm the workspace cache, git summary, and memory store so the next Codex turn starts faster"
    )]
    async fn warm_workspace_tool(
        &self,
        Parameters(args): Parameters<ForceRefreshArgs>,
    ) -> Result<Json<WarmupStatus>, String> {
        self.state
            .warmup(args.force_refresh.unwrap_or(false))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "refresh_workspace_index",
        description = "Rescan the current workspace and refresh Codex Companion's cached project index"
    )]
    async fn refresh_workspace_index_tool(&self) -> Result<Json<CacheStatus>, String> {
        self.state
            .cache_status(true)
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "cache_status",
        description = "Show the current cache status, indexed file counts, and scan reuse metrics"
    )]
    async fn cache_status_tool(
        &self,
        Parameters(args): Parameters<ForceRefreshArgs>,
    ) -> Result<Json<CacheStatus>, String> {
        self.state
            .cache_status(args.force_refresh.unwrap_or(false))
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "workspace_overview",
        description = "Summarize the current workspace using the cached index: languages, key files, directories, and important symbols"
    )]
    async fn workspace_overview_tool(
        &self,
        Parameters(args): Parameters<ForceRefreshArgs>,
    ) -> Result<Json<WorkspaceOverview>, String> {
        let index = self
            .state
            .ensure_index(args.force_refresh.unwrap_or(false))
            .await
            .map_err(|error| error.to_string())?;
        Ok(Json(build_workspace_overview(&index)))
    }

    #[tool(
        name = "search_workspace",
        description = "Search the cached workspace index for files, symbols, and snippets relevant to a task or question"
    )]
    async fn search_workspace_tool(
        &self,
        Parameters(args): Parameters<SearchWorkspaceArgs>,
    ) -> Result<Json<SearchResults>, String> {
        let index = self
            .state
            .ensure_index(args.force_refresh.unwrap_or(false))
            .await
            .map_err(|error| error.to_string())?;
        let hits = self
            .state
            .search_workspace_hits(&index, &args.query, args.limit.unwrap_or(8));
        Ok(Json(SearchResults {
            query: args.query,
            hits,
        }))
    }

    #[tool(
        name = "search_skills",
        description = "Search external skill libraries and agent profiles configured for Codex Companion"
    )]
    async fn search_skills_tool(
        &self,
        Parameters(args): Parameters<SearchSkillsArgs>,
    ) -> Result<Json<SkillSearchResults>, String> {
        self.state
            .search_skills(
                args.query,
                args.limit.unwrap_or(self.state.config.max_skills_per_query),
                args.force_refresh.unwrap_or(false),
            )
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "remember_memory",
        description = "Store a durable project memory so future Codex sessions can recall decisions, constraints, and deliverables"
    )]
    async fn remember_memory_tool(
        &self,
        Parameters(args): Parameters<RememberMemoryArgs>,
    ) -> Result<Json<MemoryRecord>, String> {
        let tags = normalize_tags(args.tags.unwrap_or_default(), &self.state.root);
        let importance = args.importance.unwrap_or_else(|| "normal".to_string());
        self.state
            .remember(args.title, args.content, tags, importance)
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "recall_memory",
        description = "Recall persistent memories relevant to the current workspace, task, or tags"
    )]
    async fn recall_memory_tool(
        &self,
        Parameters(args): Parameters<RecallMemoryArgs>,
    ) -> Result<Json<MemorySearchResults>, String> {
        let query = args.query.clone();
        let matches = self
            .state
            .recall(
                args.query,
                args.tags.unwrap_or_default(),
                args.limit.unwrap_or(8),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(Json(MemorySearchResults { query, matches }))
    }

    #[tool(
        name = "recent_changes",
        description = "Summarize recent git activity for the current workspace, including status and recent commits"
    )]
    async fn recent_changes_tool(
        &self,
        Parameters(args): Parameters<RecentChangesArgs>,
    ) -> Result<Json<GitSummary>, String> {
        let summary = self
            .state
            .git_summary(
                args.limit_commits.unwrap_or(8),
                args.include_diffstat.unwrap_or(true),
            )
            .await
            .ok_or_else(|| "git-aware tools are disabled for this server".to_string())?;

        Ok(Json(summary))
    }

    #[tool(
        name = "build_context_bundle",
        description = "Build a task-focused bundle that combines workspace overview, cached search hits, memories, and recent git changes"
    )]
    async fn build_context_bundle_tool(
        &self,
        Parameters(args): Parameters<ContextBundleArgs>,
    ) -> Result<Json<ContextBundle>, String> {
        self.state
            .build_context_bundle(
                args.task,
                args.limit.unwrap_or(6),
                args.memory_limit.unwrap_or(4),
                args.force_refresh.unwrap_or(false),
            )
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "decompose_task",
        description = "Turn a larger task into scoped workstreams, coordination notes, and parallelization hints for Codex"
    )]
    async fn decompose_task_tool(
        &self,
        Parameters(args): Parameters<DecomposeTaskArgs>,
    ) -> Result<Json<TaskDecomposition>, String> {
        self.state
            .decompose_task(
                args.task,
                args.limit.unwrap_or(6),
                args.memory_limit.unwrap_or(4),
                args.force_refresh.unwrap_or(false),
            )
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "orchestrate_task",
        description = "Run the module's full planning pipeline: skill lookup, context bundle, task decomposition, and subagent-ready workstream briefs"
    )]
    async fn orchestrate_task_tool(
        &self,
        Parameters(args): Parameters<OrchestrateTaskArgs>,
    ) -> Result<Json<TaskOrchestration>, String> {
        self.state
            .orchestrate_task(
                args.task,
                args.limit.unwrap_or(6),
                args.memory_limit.unwrap_or(4),
                args.force_refresh.unwrap_or(false),
            )
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }
}

#[prompt_router]
impl CodexCompanionServer {
    #[prompt(
        name = "codex_bootstrap",
        description = "A starter prompt that teaches Codex how to use Codex Companion's cache, memory, and git-aware tools in a new session"
    )]
    async fn codex_bootstrap_prompt(
        &self,
        Parameters(args): Parameters<BootstrapPromptArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let focus = args
            .focus
            .unwrap_or_else(|| "correctness, context gathering, and safe execution".to_string());
        let execution_mode = self.state.config.execution_mode.clone();
        let access_hint = if self.state.config.prefer_full_access {
            "If the host offers a full-access or auto-approved mode for trusted workspaces, enable it after the initial read-only scan so implementation turns are not bottlenecked by confirmations."
        } else {
            "Stay in the host's normal approval mode unless the task clearly needs stronger access."
        };
        let messages = vec![
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                "You are operating inside Zed with the Codex Companion MCP server enabled. Use the companion tools early to gather context and keep durable memory.".to_string(),
            ),
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                format!(
                    "Workflow: start with warm_workspace, then call orchestrate_task for any non-trivial task so skill lookup, context bundle creation, decomposition, and subagent briefs stay in one contract. Use build_context_bundle or workspace_overview only for lighter discovery, use search_workspace before editing, use recent_changes when repo state matters, and write durable decisions to remember_memory. If the host supports subagents, delegate only the workstreams that orchestration marks as parallel-ready. In Codex ACP threads, call MCP tools directly instead of relying on `/codex-*` slash commands, because the host may intercept `/...` before the extension sees it. Execution mode: {execution_mode}. {access_hint}"
                ),
            ),
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Task: {}\nFocus: {}\nPlease gather the minimum useful context first, then proceed with implementation or analysis.",
                    args.task, focus
                ),
            ),
        ];

        Ok(GetPromptResult::new(messages)
            .with_description("Codex Companion bootstrap prompt for a new Zed session"))
    }

    #[prompt(
        name = "codex_handoff",
        description = "A handoff prompt for wrapping up a Codex session and leaving durable memory for the next session"
    )]
    async fn codex_handoff_prompt(
        &self,
        Parameters(args): Parameters<HandoffPromptArgs>,
    ) -> GetPromptResult {
        let goal = args.next_session_goal.unwrap_or_else(|| {
            "continue from the latest checkpoint without re-discovering context".to_string()
        });
        let messages = vec![
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                "Prepare a concise handoff. Capture what changed, what is risky, what remains open, which workstreams are still in flight, and what should be remembered durably.".to_string(),
            ),
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Current session summary: {}\nNext session goal: {}",
                    args.summary, goal
                ),
            ),
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                "Before finishing, call remember_memory with the durable parts of this handoff so the next session can recall it quickly.".to_string(),
            ),
        ];

        GetPromptResult::new(messages).with_description("Codex Companion handoff prompt")
    }
}

impl ServerHandler for CodexCompanionServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_instructions(
            "Codex Companion adds persistent memory, warmable workspace caching, external skill indexing, task orchestration, git awareness, task decomposition, and task-focused context bundles for Codex running inside Zed. Start non-trivial work with orchestrate_task so the module can bind skills, shared context, and subagent-ready briefs into one result. In Codex ACP threads, invoke the MCP tools directly rather than `/codex-*` slash commands, because the host may parse `/...` itself before the extension can handle it. It can define parallel-ready workstreams and subagent specs, but the actual permission model and subagent execution are still controlled by the host agent."
                .to_string(),
        )
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "codex-companion-server",
    version,
    about = "Companion MCP server for Codex in Zed"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Serve(RootArgs),
    Status(QueryRootArgs),
    Index(QueryRootArgs),
    Warm(QueryRootArgs),
    Bundle(BundleCliArgs),
    Plan(PlanCliArgs),
    Orchestrate(PlanCliArgs),
    Skills(SkillsCliArgs),
    Memory(MemoryCliArgs),
}

#[derive(Debug, Clone, Args, Default)]
struct RootArgs {
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Debug, Clone, Args, Default)]
struct QueryRootArgs {
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
struct BundleCliArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    query: String,
    #[arg(long, default_value_t = 6)]
    limit: usize,
    #[arg(long, default_value_t = 4)]
    memory_limit: usize,
}

#[derive(Debug, Clone, Args)]
struct PlanCliArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    query: String,
    #[arg(long, default_value_t = 6)]
    limit: usize,
    #[arg(long, default_value_t = 4)]
    memory_limit: usize,
}

#[derive(Debug, Clone, Args)]
struct SkillsCliArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    query: Option<String>,
    #[arg(long, default_value_t = 6)]
    limit: usize,
}

#[derive(Debug, Clone, Args)]
struct MemoryCliArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    query: Option<String>,
    #[arg(long, default_value_t = 8)]
    limit: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Commands::Serve(RootArgs::default())) {
        Commands::Serve(args) => {
            serve_stdio(resolve_root(args.root)?, ServerConfig::from_env()).await
        }
        Commands::Status(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let status = state.cache_status(false).await?;
            print!("{}", format_cache_status(&status));
            Ok(())
        }
        Commands::Index(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let status = state.cache_status(true).await?;
            print!("{}", format_cache_status(&status));
            Ok(())
        }
        Commands::Warm(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let warmup = state.warmup(true).await?;
            print!("{}", format_warmup_status(&warmup));
            Ok(())
        }
        Commands::Bundle(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let bundle = state
                .build_context_bundle(args.query, args.limit, args.memory_limit, false)
                .await?;
            print!("{}", format_context_bundle(&bundle));
            Ok(())
        }
        Commands::Plan(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let decomposition = state
                .decompose_task(args.query, args.limit, args.memory_limit, false)
                .await?;
            print!("{}", format_task_decomposition(&decomposition));
            Ok(())
        }
        Commands::Orchestrate(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let orchestration = state
                .orchestrate_task(args.query, args.limit, args.memory_limit, false)
                .await?;
            print!("{}", format_task_orchestration(&orchestration));
            Ok(())
        }
        Commands::Skills(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let results = state.search_skills(args.query, args.limit, false).await?;
            print!("{}", format_skill_results(&results));
            Ok(())
        }
        Commands::Memory(args) => {
            let state = AppState::new(resolve_root(args.root)?, ServerConfig::from_env())?;
            let results = MemorySearchResults {
                query: args.query.clone(),
                matches: state.recall(args.query, Vec::new(), args.limit).await?,
            };
            print!("{}", format_memory_results(&results));
            Ok(())
        }
    }
}

async fn serve_stdio(root: PathBuf, config: ServerConfig) -> Result<()> {
    let state = AppState::new(root, config)?;
    if state.config.prewarm_on_start {
        let warm_state = state.clone();
        tokio::spawn(async move {
            let _ = warm_state.warmup(false).await;
        });
    }

    let server = CodexCompanionServer::new(state);
    let router: Router<CodexCompanionServer> = Router {
        tool_router: server.tool_router.clone(),
        prompt_router: server.prompt_router.clone(),
        service: Arc::new(server),
    };
    let service = router.serve(stdio()).await.inspect_err(|error| {
        tracing::error!("failed to serve Codex Companion over stdio: {error:?}")
    })?;

    service.waiting().await?;
    Ok(())
}

fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf> {
    let root = match root {
        Some(path) => path,
        None => {
            std::env::current_dir().context("failed to determine the current working directory")?
        }
    };

    if !root.exists() {
        return Err(anyhow!("workspace root does not exist: {}", root.display()));
    }
    if !root.is_dir() {
        return Err(anyhow!(
            "workspace root is not a directory: {}",
            root.display()
        ));
    }
    Ok(root)
}

fn normalize_tags(tags: Vec<String>, root: &Path) -> Vec<String> {
    let mut normalized = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();

    if let Some(name) = root.file_name().and_then(|value| value.to_str()) {
        normalized.push(name.to_lowercase());
    }
    normalized.push("codex-companion".to_string());
    normalized.sort();
    normalized.dedup();
    normalized
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init();
}

#[cfg(test)]
mod tests;
