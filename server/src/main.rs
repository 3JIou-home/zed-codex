mod cache;
mod git_tools;
mod indexer;
mod model;
mod skills;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
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
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use crate::{
    cache::{load_json, resolve_workspace_cache, save_json, WorkspaceCache},
    git_tools::collect_git_summary,
    indexer::{build_workspace_overview, refresh_workspace_index, search_workspace, IndexOptions},
    model::{
        CacheStatus, ContextBundle, GitSummary, MemoryRecord, MemorySearchResults, MemoryStore,
        OrchestrationStage, SearchResults, SkillCatalog, SkillMatch, SkillSearchResults,
        SubagentSpec, TaskDecomposition, TaskOrchestration, TaskWorkstream, WarmupStatus,
        WorkspaceIndex, WorkspaceOverview,
    },
    skills::{build_skill_catalog, search_skills, SkillIndexOptions},
};

const DEFAULT_SKILL_ROOT_CANDIDATES: &[&str] = &[];

#[derive(Debug, Clone)]
struct ServerConfig {
    cache_dir_override: Option<PathBuf>,
    max_file_bytes: usize,
    max_indexed_files: usize,
    ignore_globs: Vec<String>,
    enable_git_tools: bool,
    refresh_window_secs: u64,
    git_cache_ttl_secs: u64,
    bundle_cache_ttl_secs: u64,
    skill_cache_ttl_secs: u64,
    prewarm_on_start: bool,
    execution_mode: String,
    prefer_full_access: bool,
    max_parallel_workstreams: usize,
    skill_roots: Vec<PathBuf>,
    skill_file_globs: Vec<String>,
    max_skill_bytes: usize,
    max_skills_per_query: usize,
}

impl ServerConfig {
    fn from_env() -> Self {
        Self {
            cache_dir_override: std::env::var("CODEX_COMPANION_CACHE_DIR")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            max_file_bytes: std::env::var("CODEX_COMPANION_MAX_FILE_BYTES")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(262_144),
            max_indexed_files: std::env::var("CODEX_COMPANION_MAX_INDEXED_FILES")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1_500),
            ignore_globs: std::env::var("CODEX_COMPANION_IGNORE_GLOBS_JSON")
                .ok()
                .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
                .unwrap_or_default(),
            enable_git_tools: std::env::var("CODEX_COMPANION_ENABLE_GIT_TOOLS")
                .ok()
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "True"))
                .unwrap_or(true),
            refresh_window_secs: std::env::var("CODEX_COMPANION_REFRESH_WINDOW_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(120),
            git_cache_ttl_secs: std::env::var("CODEX_COMPANION_GIT_CACHE_TTL_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(15),
            bundle_cache_ttl_secs: std::env::var("CODEX_COMPANION_BUNDLE_CACHE_TTL_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(180),
            skill_cache_ttl_secs: std::env::var("CODEX_COMPANION_SKILL_CACHE_TTL_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(300),
            prewarm_on_start: std::env::var("CODEX_COMPANION_PREWARM_ON_START")
                .ok()
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "True"))
                .unwrap_or(true),
            execution_mode: normalize_execution_mode(
                std::env::var("CODEX_COMPANION_EXECUTION_MODE").ok(),
            ),
            prefer_full_access: std::env::var("CODEX_COMPANION_PREFER_FULL_ACCESS")
                .ok()
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "True"))
                .unwrap_or(true),
            max_parallel_workstreams: std::env::var("CODEX_COMPANION_MAX_PARALLEL_WORKSTREAMS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .map(|value| value.clamp(1, 8))
                .unwrap_or(4),
            skill_roots: configured_skill_roots(
                std::env::var("CODEX_COMPANION_SKILL_ROOTS_JSON").ok(),
            ),
            skill_file_globs: std::env::var("CODEX_COMPANION_SKILL_FILE_GLOBS_JSON")
                .ok()
                .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
                .unwrap_or_else(|| vec!["**/*.md".to_string()]),
            max_skill_bytes: std::env::var("CODEX_COMPANION_MAX_SKILL_BYTES")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(131_072),
            max_skills_per_query: std::env::var("CODEX_COMPANION_MAX_SKILLS_PER_QUERY")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .map(|value| value.clamp(1, 12))
                .unwrap_or(4),
        }
    }

    fn index_options(&self) -> IndexOptions {
        IndexOptions {
            max_file_bytes: self.max_file_bytes,
            max_indexed_files: self.max_indexed_files,
            ignore_globs: self.ignore_globs.clone(),
        }
    }
}

fn configured_skill_roots(raw_skill_roots: Option<String>) -> Vec<PathBuf> {
    match raw_skill_roots {
        Some(value) => serde_json::from_str::<Vec<String>>(&value)
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        None => existing_skill_roots(default_skill_root_candidates()),
    }
}

fn default_skill_root_candidates() -> Vec<PathBuf> {
    DEFAULT_SKILL_ROOT_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .collect()
}

fn existing_skill_roots(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|path| path.is_dir()).collect()
}

#[derive(Debug, Clone)]
struct TimedCacheEntry<T> {
    value: T,
    loaded_at: Instant,
}

#[derive(Debug, Default)]
struct RuntimeState {
    index: Option<Arc<WorkspaceIndex>>,
    index_loaded_at: Option<Instant>,
    memories: Option<MemoryStore>,
    git_summary_cache: HashMap<String, TimedCacheEntry<GitSummary>>,
    skill_catalog: Option<SkillCatalog>,
    skill_catalog_loaded_at: Option<Instant>,
    bundle_cache: HashMap<String, TimedCacheEntry<ContextBundle>>,
    decomposition_cache: HashMap<String, TimedCacheEntry<TaskDecomposition>>,
}

const MAX_GIT_SUMMARY_CACHE_ENTRIES: usize = 8;
const MAX_BUNDLE_CACHE_ENTRIES: usize = 32;
const MAX_DECOMPOSITION_CACHE_ENTRIES: usize = 32;

#[derive(Clone)]
struct AppState {
    root: PathBuf,
    cache: WorkspaceCache,
    config: ServerConfig,
    runtime: Arc<Mutex<RuntimeState>>,
    memory_write: Arc<Mutex<()>>,
}

impl AppState {
    fn new(root: PathBuf, config: ServerConfig) -> Result<Self> {
        let cache = resolve_workspace_cache(&root, config.cache_dir_override.as_ref())?;
        Ok(Self {
            root,
            cache,
            config,
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
            memory_write: Arc::new(Mutex::new(())),
        })
    }

    async fn ensure_index(&self, force_refresh: bool) -> Result<Arc<WorkspaceIndex>> {
        {
            let runtime = self.runtime.lock().await;
            if !force_refresh {
                if let (Some(index), Some(index_loaded_at)) =
                    (&runtime.index, runtime.index_loaded_at)
                {
                    if index_loaded_at.elapsed()
                        <= Duration::from_secs(self.config.refresh_window_secs)
                    {
                        return Ok(Arc::clone(index));
                    }
                }
            }
        }

        let root = self.root.clone();
        let cache = self.cache.clone();
        let options = self.config.index_options();
        let refreshed =
            tokio::task::spawn_blocking(move || refresh_workspace_index(&root, &cache, &options))
                .await
                .map_err(|error| anyhow!("index refresh task failed: {error}"))??;

        let refreshed = Arc::new(refreshed);
        let mut runtime = self.runtime.lock().await;
        invalidate_task_caches(&mut runtime);
        runtime.index = Some(Arc::clone(&refreshed));
        runtime.index_loaded_at = Some(Instant::now());
        Ok(refreshed)
    }

    async fn cache_status(&self, force_refresh: bool) -> Result<CacheStatus> {
        let index = self.ensure_index(force_refresh).await?;
        Ok(CacheStatus {
            workspace_id: self.cache.workspace_id.clone(),
            workspace_root: self.root.display().to_string(),
            cache_dir: self.cache.workspace_dir.display().to_string(),
            indexed_at: index.indexed_at.clone(),
            indexed_files: index.files.len(),
            indexed_bytes: index.total_indexed_bytes,
            scan_metrics: index.scan_metrics.clone(),
        })
    }

    async fn load_memories(&self) -> Result<MemoryStore> {
        {
            let runtime = self.runtime.lock().await;
            if let Some(memories) = &runtime.memories {
                return Ok(memories.clone());
            }
        }

        let memory_file = self.cache.memory_file.clone();
        let workspace_id = self.cache.workspace_id.clone();
        let workspace_root = self.root.display().to_string();
        let memories = tokio::task::spawn_blocking(move || {
            Ok::<_, anyhow::Error>(
                load_json::<MemoryStore>(&memory_file)?.unwrap_or(MemoryStore {
                    workspace_id,
                    workspace_root,
                    updated_at: Utc::now().to_rfc3339(),
                    entries: Vec::new(),
                }),
            )
        })
        .await
        .map_err(|error| anyhow!("memory load task failed: {error}"))??;

        let mut runtime = self.runtime.lock().await;
        runtime.memories = Some(memories.clone());
        Ok(memories)
    }

    async fn save_memories(&self, store: MemoryStore) -> Result<()> {
        let memory_file = self.cache.memory_file.clone();
        let store_to_save = store.clone();
        tokio::task::spawn_blocking(move || save_json(&memory_file, &store_to_save))
            .await
            .map_err(|error| anyhow!("memory save task failed: {error}"))??;

        let mut runtime = self.runtime.lock().await;
        runtime.memories = Some(store);
        invalidate_task_caches(&mut runtime);
        Ok(())
    }

    async fn skill_catalog(&self, force_refresh: bool) -> Result<Option<SkillCatalog>> {
        if self.config.skill_roots.is_empty() {
            return Ok(None);
        }

        {
            let runtime = self.runtime.lock().await;
            if !force_refresh {
                if let (Some(catalog), Some(loaded_at)) =
                    (&runtime.skill_catalog, runtime.skill_catalog_loaded_at)
                {
                    if loaded_at.elapsed() <= Duration::from_secs(self.config.skill_cache_ttl_secs)
                    {
                        return Ok(Some(catalog.clone()));
                    }
                }
            }
        }

        let options = SkillIndexOptions {
            roots: self.config.skill_roots.clone(),
            include_globs: self.config.skill_file_globs.clone(),
            max_skill_bytes: self.config.max_skill_bytes,
        };
        let catalog = tokio::task::spawn_blocking(move || build_skill_catalog(&options))
            .await
            .map_err(|error| anyhow!("skill catalog task failed: {error}"))??;

        let mut runtime = self.runtime.lock().await;
        invalidate_task_caches(&mut runtime);
        runtime.skill_catalog = Some(catalog.clone());
        runtime.skill_catalog_loaded_at = Some(Instant::now());
        Ok(Some(catalog))
    }

    async fn search_skills(
        &self,
        query: Option<String>,
        limit: usize,
        force_refresh: bool,
    ) -> Result<SkillSearchResults> {
        let catalog = self.skill_catalog(force_refresh).await?;
        let Some(catalog) = catalog else {
            return Ok(SkillSearchResults {
                query,
                indexed_at: None,
                hits: Vec::new(),
            });
        };

        Ok(search_skills(&catalog, query.as_deref(), limit))
    }

    async fn warmup(&self, force_refresh: bool) -> Result<WarmupStatus> {
        let started_at = Instant::now();
        let cache_status = self.cache_status(force_refresh).await?;
        let warmed_git = matches!(
            self.git_summary(6, true).await,
            Some(summary) if summary.available
        );
        let warmed_memory = self.load_memories().await.is_ok();
        let warmed_skills = self.skill_catalog(force_refresh).await?.is_some();

        Ok(WarmupStatus {
            workspace_root: self.root.display().to_string(),
            elapsed_ms: started_at.elapsed().as_millis() as u64,
            cache_status,
            warmed_git,
            warmed_memory,
            warmed_skills,
        })
    }

    async fn remember(
        &self,
        title: String,
        content: String,
        tags: Vec<String>,
        importance: String,
    ) -> Result<MemoryRecord> {
        let _memory_write_guard = self.memory_write.lock().await;
        let mut store = self.load_memories().await?;
        let now = Utc::now().to_rfc3339();
        let id_source = format!(
            "{}:{}:{}:{}",
            self.cache.workspace_id,
            title,
            content,
            store.entries.len()
        );
        let memory = MemoryRecord {
            id: blake3::hash(id_source.as_bytes()).to_hex()[..16].to_string(),
            title,
            content,
            tags,
            importance,
            created_at: now.clone(),
            updated_at: now.clone(),
        };

        store.entries.push(memory.clone());
        store.updated_at = now;
        self.save_memories(store).await?;
        Ok(memory)
    }

    async fn recall(
        &self,
        query: Option<String>,
        tags: Vec<String>,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let store = self.load_memories().await?;
        let normalized_query = query
            .as_ref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty());
        let normalized_tags = tags
            .into_iter()
            .map(|tag| tag.trim().to_lowercase())
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();

        let query_tokens = normalized_query
            .as_ref()
            .map(|value| tokenize(value))
            .unwrap_or_default();

        let mut matches = store
            .entries
            .into_iter()
            .filter_map(|entry| {
                let score = score_memory(
                    &entry,
                    normalized_query.as_deref(),
                    &query_tokens,
                    &normalized_tags,
                );
                if score <= 0.0 {
                    return None;
                }

                Some((score, entry))
            })
            .collect::<Vec<_>>();

        matches.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.1.updated_at.cmp(&left.1.updated_at))
        });

        Ok(matches
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect())
    }

    async fn git_summary(
        &self,
        limit_commits: usize,
        include_diffstat: bool,
    ) -> Option<GitSummary> {
        if !self.config.enable_git_tools {
            return None;
        }

        {
            let mut runtime = self.runtime.lock().await;
            prune_timed_cache(
                &mut runtime.git_summary_cache,
                self.config.git_cache_ttl_secs,
                MAX_GIT_SUMMARY_CACHE_ENTRIES,
            );

            let cache_key = git_summary_cache_key(limit_commits, include_diffstat);
            if let Some(summary) = runtime.git_summary_cache.get(&cache_key) {
                return Some(summary.value.clone());
            }
        }

        let root = self.root.clone();
        let summary = tokio::task::spawn_blocking(move || {
            collect_git_summary(&root, limit_commits, include_diffstat)
        })
        .await
        .ok()?;

        let cache_key = git_summary_cache_key(limit_commits, include_diffstat);
        let mut runtime = self.runtime.lock().await;
        prune_timed_cache(
            &mut runtime.git_summary_cache,
            self.config.git_cache_ttl_secs,
            MAX_GIT_SUMMARY_CACHE_ENTRIES,
        );
        runtime.git_summary_cache.insert(
            cache_key,
            TimedCacheEntry {
                value: summary.clone(),
                loaded_at: Instant::now(),
            },
        );
        Some(summary)
    }

    async fn build_context_bundle(
        &self,
        task: String,
        limit: usize,
        memory_limit: usize,
        force_refresh: bool,
    ) -> Result<ContextBundle> {
        let cache_key = task_cache_key(&task, limit, memory_limit);
        let cached_bundle = {
            let mut runtime = self.runtime.lock().await;
            prune_timed_cache(
                &mut runtime.bundle_cache,
                self.config.bundle_cache_ttl_secs,
                MAX_BUNDLE_CACHE_ENTRIES,
            );

            if !force_refresh && can_reuse_task_cache(&runtime, &self.config) {
                runtime
                    .bundle_cache
                    .get(&cache_key)
                    .map(|entry| entry.value.clone())
            } else {
                None
            }
        };
        if let Some(mut bundle) = cached_bundle {
            bundle.recent_changes = self.git_summary(6, true).await;
            return Ok(bundle);
        }

        let index = self.ensure_index(force_refresh).await?;
        let overview = build_workspace_overview(&index);
        let search_hits = search_workspace(&index, &task, limit);
        let memories = self
            .recall(Some(task.clone()), Vec::new(), memory_limit)
            .await?;
        let recommended_skills = self
            .search_skills(
                Some(task.clone()),
                self.config.max_skills_per_query,
                force_refresh,
            )
            .await?
            .hits;
        let recent_changes = self.git_summary(6, true).await;
        let suggested_next_actions = build_suggested_next_actions(
            &task,
            &search_hits,
            &recommended_skills,
            &self.config.execution_mode,
            self.config.prefer_full_access,
        );

        let bundle = ContextBundle {
            task,
            overview,
            search_hits,
            memories,
            recommended_skills,
            recent_changes,
            suggested_next_actions,
        };

        let mut runtime = self.runtime.lock().await;
        prune_timed_cache(
            &mut runtime.bundle_cache,
            self.config.bundle_cache_ttl_secs,
            MAX_BUNDLE_CACHE_ENTRIES,
        );
        runtime.bundle_cache.insert(
            cache_key,
            TimedCacheEntry {
                value: bundle.clone(),
                loaded_at: Instant::now(),
            },
        );
        Ok(bundle)
    }

    async fn decompose_task(
        &self,
        task: String,
        limit: usize,
        memory_limit: usize,
        force_refresh: bool,
    ) -> Result<TaskDecomposition> {
        let cache_key = task_cache_key(&task, limit, memory_limit);
        {
            let mut runtime = self.runtime.lock().await;
            prune_timed_cache(
                &mut runtime.decomposition_cache,
                self.config.bundle_cache_ttl_secs,
                MAX_DECOMPOSITION_CACHE_ENTRIES,
            );

            if !force_refresh && can_reuse_task_cache(&runtime, &self.config) {
                if let Some(entry) = runtime.decomposition_cache.get(&cache_key) {
                    return Ok(entry.value.clone());
                }
            }
        }

        let bundle = self
            .build_context_bundle(task.clone(), limit, memory_limit, force_refresh)
            .await?;
        let decomposition = build_task_decomposition(&bundle, &self.config);
        self.cache_task_decomposition(cache_key, decomposition.clone())
            .await;

        Ok(decomposition)
    }

    async fn orchestrate_task(
        &self,
        task: String,
        limit: usize,
        memory_limit: usize,
        force_refresh: bool,
    ) -> Result<TaskOrchestration> {
        let cache_key = task_cache_key(&task, limit, memory_limit);
        let bundle = self
            .build_context_bundle(task, limit, memory_limit, force_refresh)
            .await?;
        let decomposition = build_task_decomposition(&bundle, &self.config);
        self.cache_task_decomposition(cache_key, decomposition.clone())
            .await;

        let skill_catalog = self.skill_catalog(force_refresh).await?;
        let skill_limit = self.config.max_skills_per_query.clamp(1, 3);
        let mut workstream_skill_matches = HashMap::new();

        for workstream in &decomposition.workstreams {
            let targeted_skills = skill_catalog
                .as_ref()
                .map(|catalog| {
                    let query = build_workstream_skill_query(&bundle.task, workstream);
                    search_skills(catalog, query.as_deref(), skill_limit).hits
                })
                .unwrap_or_default();
            let fallback_skills =
                fallback_workstream_skills(&bundle.recommended_skills, workstream);
            let merged_skills = merge_skill_matches(targeted_skills, fallback_skills, skill_limit);
            workstream_skill_matches.insert(workstream.id.clone(), merged_skills);
        }

        Ok(build_task_orchestration(
            bundle,
            decomposition,
            &workstream_skill_matches,
            &self.config,
        ))
    }

    async fn cache_task_decomposition(&self, cache_key: String, decomposition: TaskDecomposition) {
        let mut runtime = self.runtime.lock().await;
        prune_timed_cache(
            &mut runtime.decomposition_cache,
            self.config.bundle_cache_ttl_secs,
            MAX_DECOMPOSITION_CACHE_ENTRIES,
        );
        runtime.decomposition_cache.insert(
            cache_key,
            TimedCacheEntry {
                value: decomposition.clone(),
                loaded_at: Instant::now(),
            },
        );
    }
}

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
        let hits = search_workspace(&index, &args.query, args.limit.unwrap_or(8));
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

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .filter(|piece| !piece.is_empty())
        .map(|piece| piece.to_lowercase())
        .collect()
}

fn score_memory(
    entry: &MemoryRecord,
    normalized_query: Option<&str>,
    query_tokens: &[String],
    normalized_tags: &[String],
) -> f64 {
    let title = entry.title.to_lowercase();
    let content = entry.content.to_lowercase();
    let tags = entry
        .tags
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>();
    let tags_joined = tags.join(" ");

    let mut score = 0.0;
    if let Some(query) = normalized_query {
        if title.contains(query) {
            score += 12.0;
        }
        if content.contains(query) {
            score += 8.0;
        }
        if tags_joined.contains(query) {
            score += 10.0;
        }
    }

    for token in query_tokens {
        if title.contains(token) {
            score += 6.0;
        }
        if content.contains(token) {
            score += 2.5;
        }
        if tags.iter().any(|tag| tag.contains(token)) {
            score += 4.0;
        }
    }

    for tag in normalized_tags {
        if tags.iter().any(|entry_tag| entry_tag == tag) {
            score += 5.0;
        }
    }

    if normalized_query.is_none() && normalized_tags.is_empty() {
        score += 1.0;
    }

    score
}

fn normalize_execution_mode(value: Option<String>) -> String {
    match value
        .unwrap_or_else(|| "balanced".to_string())
        .trim()
        .to_lowercase()
        .as_str()
    {
        "autonomous" | "fast" => "autonomous".to_string(),
        "careful" | "safe" => "careful".to_string(),
        _ => "balanced".to_string(),
    }
}

fn task_cache_key(task: &str, limit: usize, memory_limit: usize) -> String {
    blake3::hash(format!("{task}::{limit}::{memory_limit}").as_bytes())
        .to_hex()
        .to_string()
}

fn git_summary_cache_key(limit_commits: usize, include_diffstat: bool) -> String {
    format!("{limit_commits}:{include_diffstat}")
}

fn invalidate_task_caches(runtime: &mut RuntimeState) {
    runtime.bundle_cache.clear();
    runtime.decomposition_cache.clear();
}

fn can_reuse_task_cache(runtime: &RuntimeState, config: &ServerConfig) -> bool {
    let index_fresh = runtime
        .index_loaded_at
        .map(|loaded_at| loaded_at.elapsed() <= Duration::from_secs(config.refresh_window_secs))
        .unwrap_or(false);
    if !index_fresh {
        return false;
    }

    if config.skill_roots.is_empty() {
        return true;
    }

    runtime
        .skill_catalog_loaded_at
        .map(|loaded_at| loaded_at.elapsed() <= Duration::from_secs(config.skill_cache_ttl_secs))
        .unwrap_or(false)
}

fn prune_timed_cache<T>(
    cache: &mut HashMap<String, TimedCacheEntry<T>>,
    ttl_secs: u64,
    max_entries: usize,
) {
    let ttl = Duration::from_secs(ttl_secs);
    cache.retain(|_, entry| entry.loaded_at.elapsed() <= ttl);

    if cache.len() <= max_entries {
        return;
    }

    let mut oldest_entries = cache
        .iter()
        .map(|(key, entry)| (key.clone(), entry.loaded_at))
        .collect::<Vec<_>>();
    oldest_entries.sort_by_key(|(_, loaded_at)| *loaded_at);

    for (key, _) in oldest_entries.into_iter().take(cache.len() - max_entries) {
        cache.remove(&key);
    }
}

fn build_suggested_next_actions(
    task: &str,
    search_hits: &[crate::model::SearchHit],
    recommended_skills: &[SkillMatch],
    execution_mode: &str,
    prefer_full_access: bool,
) -> Vec<String> {
    let mut actions = vec![
        "Use warm_workspace at the start of a fresh session to prime cache, git, and memories."
            .to_string(),
    ];

    if !search_hits.is_empty() {
        actions.push("Open the highest-scoring files from search hits before editing.".to_string());
    }

    if search_hits.len() > 3 || task.split_whitespace().count() > 8 {
        actions.push(
            "Call orchestrate_task first so skills, context, workstreams, and delegate briefs stay aligned in one result."
                .to_string(),
        );
    }

    if !recommended_skills.is_empty() {
        actions.push(
            "Use orchestrate_task or search_skills before planning implementation details so each workstream is grounded in a concrete skill match."
                .to_string(),
        );
    }

    if prefer_full_access {
        actions.push(
            "If the host offers full-access or auto-approved tools for a trusted workspace, enable it before heavy edits, tests, or multi-file refactors."
                .to_string(),
        );
    }

    actions.push(format!(
        "Execution mode is `{execution_mode}`: adapt pacing and autonomy to match that preference."
    ));
    actions.push(
        "Write durable architectural or workflow decisions with remember_memory.".to_string(),
    );
    actions
}

fn build_workstream_skill_query(task: &str, workstream: &TaskWorkstream) -> Option<String> {
    let mut parts = vec![task.trim().to_string(), workstream.title.clone()];

    if !workstream.objective.trim().is_empty() {
        parts.push(workstream.objective.clone());
    }
    if let Some(file) = workstream.recommended_files.first() {
        parts.push(file.clone());
    }
    if let Some(symbol) = workstream.matching_symbols.first() {
        parts.push(symbol.clone());
    }

    let query = parts
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if query.is_empty() {
        None
    } else {
        Some(query)
    }
}

fn fallback_workstream_skills(
    recommended_skills: &[SkillMatch],
    workstream: &TaskWorkstream,
) -> Vec<SkillMatch> {
    if !workstream.recommended_skills.is_empty() {
        let wanted = workstream
            .recommended_skills
            .iter()
            .map(|name| name.to_lowercase())
            .collect::<HashSet<_>>();
        let matches = recommended_skills
            .iter()
            .filter(|skill| wanted.contains(&skill.name.to_lowercase()))
            .cloned()
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return matches;
        }
    }

    recommended_skills.iter().take(2).cloned().collect()
}

fn merge_skill_matches(
    primary: Vec<SkillMatch>,
    fallback: Vec<SkillMatch>,
    limit: usize,
) -> Vec<SkillMatch> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();

    for skill in primary.into_iter().chain(fallback) {
        let key = if skill.id.trim().is_empty() {
            skill.path.clone()
        } else {
            skill.id.clone()
        };
        if seen.insert(key) {
            merged.push(skill);
        }
        if merged.len() >= limit {
            break;
        }
    }

    merged
}

fn candidate_scope_depth(path: &str) -> usize {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    segments.len().clamp(1, 3)
}

fn derive_workstream_scope(path: &str, depth: usize) -> String {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() {
        return ".".to_string();
    }
    if segments.len() == 1 {
        return segments[0].to_string();
    }

    let directory_segments = &segments[..segments.len() - 1];
    if directory_segments.is_empty() {
        return segments[0].to_string();
    }

    let clamped_depth = depth.max(1);
    if clamped_depth > directory_segments.len() {
        return path.to_string();
    }

    directory_segments[..clamped_depth].join("/")
}

fn group_hits_for_workstreams(
    search_hits: &[crate::model::SearchHit],
) -> Vec<(String, Vec<crate::model::SearchHit>)> {
    let max_depth = search_hits
        .iter()
        .map(|hit| candidate_scope_depth(&hit.path))
        .max()
        .unwrap_or(1);

    let mut best_groups = HashMap::<String, Vec<crate::model::SearchHit>>::new();
    for depth in 1..=max_depth {
        let mut groups = HashMap::<String, Vec<crate::model::SearchHit>>::new();
        for hit in search_hits {
            let scope = derive_workstream_scope(&hit.path, depth);
            groups.entry(scope).or_default().push(hit.clone());
        }

        if groups.len() >= best_groups.len() {
            best_groups = groups;
        }
        if best_groups.len() > 1 {
            break;
        }
    }

    best_groups.into_iter().collect()
}

fn build_task_decomposition(bundle: &ContextBundle, config: &ServerConfig) -> TaskDecomposition {
    let mut scopes = group_hits_for_workstreams(&bundle.search_hits);
    scopes.sort_by(|left, right| {
        let left_score = left.1.iter().map(|hit| hit.score).sum::<f64>();
        let right_score = right.1.iter().map(|hit| hit.score).sum::<f64>();
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut workstreams = Vec::new();
    for (index, (scope, hits)) in scopes
        .into_iter()
        .take(config.max_parallel_workstreams)
        .enumerate()
    {
        let representative = hits.first().cloned();
        let mut recommended_files = hits.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
        recommended_files.sort();
        recommended_files.dedup();
        recommended_files.truncate(5);

        let mut matching_symbols = hits
            .iter()
            .flat_map(|hit| hit.matching_symbols.clone())
            .collect::<Vec<_>>();
        matching_symbols.sort();
        matching_symbols.dedup();
        matching_symbols.truncate(8);

        let can_run_in_parallel = recommended_files
            .iter()
            .all(|path| !is_likely_shared_file(path))
            && index > 0;

        let scope_query = scope.to_lowercase();
        let recommended_skills = bundle
            .recommended_skills
            .iter()
            .filter(|skill| {
                skill.category.to_lowercase().contains(&scope_query)
                    || skill.path.to_lowercase().contains(&scope_query)
                    || skill.description.to_lowercase().contains(&scope_query)
            })
            .take(2)
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>();

        let objective = representative
            .as_ref()
            .map(|hit| {
                format!(
                    "Advance `{}` by focusing on `{}` and the files most relevant to that scope.",
                    bundle.task, hit.path
                )
            })
            .unwrap_or_else(|| format!("Advance `{}` inside scope `{scope}`.", bundle.task));

        let rationale = representative
            .as_ref()
            .map(|hit| {
                format!(
                    "This scope matched the task strongly through `{}` with score {:.1}.",
                    hit.path, hit.score
                )
            })
            .unwrap_or_else(|| {
                "This scope groups the strongest matching files for the task.".to_string()
            });

        let handoff = format!(
            "Own the `{scope}` slice for task `{}`. Stay within {:?} when possible, note cross-scope contracts before editing shared files, and leave durable decisions in remember_memory.",
            bundle.task, recommended_files
        );

        workstreams.push(TaskWorkstream {
            id: format!("ws-{}", index + 1),
            title: if scope == "." {
                "Root-level coordination".to_string()
            } else {
                format!("{} workstream", scope)
            },
            objective,
            rationale,
            recommended_files,
            matching_symbols,
            recommended_skills,
            can_run_in_parallel,
            handoff,
        });
    }

    if workstreams.is_empty() {
        workstreams.push(TaskWorkstream {
            id: "ws-1".to_string(),
            title: "Primary implementation path".to_string(),
            objective: format!("Advance `{}` with a single focused pass.", bundle.task),
            rationale:
                "No strong file clusters were found, so a single exploratory stream is safer."
                    .to_string(),
            recommended_files: bundle.overview.key_files.iter().take(5).cloned().collect(),
            matching_symbols: Vec::new(),
            recommended_skills: bundle
                .recommended_skills
                .iter()
                .take(2)
                .map(|skill| skill.name.clone())
                .collect(),
            can_run_in_parallel: false,
            handoff: format!(
                "Drive the task `{}` end-to-end, then record durable findings in remember_memory.",
                bundle.task
            ),
        });
    }

    let can_parallelize = workstreams
        .iter()
        .filter(|stream| stream.can_run_in_parallel)
        .count()
        > 0;
    let mut shared_context = bundle
        .overview
        .key_files
        .iter()
        .take(6)
        .cloned()
        .collect::<Vec<_>>();
    for memory in &bundle.memories {
        if shared_context.len() >= 10 {
            break;
        }
        shared_context.push(format!("memory: {}", memory.title));
    }
    shared_context.dedup();

    let mut coordination_notes = vec![
        "Lock interfaces or file ownership before multiple workstreams edit neighboring modules."
            .to_string(),
        "Use remember_memory for durable decisions so the next session does not repeat discovery."
            .to_string(),
    ];
    if can_parallelize {
        coordination_notes.push(
            "If the host supports subagents, delegate only streams whose recommended files do not overlap."
                .to_string(),
        );
    }
    if config.prefer_full_access {
        coordination_notes.push(
            "Full-access or auto-approved mode is advisory only here: the actual permission boundary is controlled by the host agent, not by the MCP companion."
                .to_string(),
        );
    }

    let mut first_actions = vec![
        "Run warm_workspace to ensure index, git, and memory caches are warm.".to_string(),
        "Inspect the first workstream's recommended files and confirm the smallest safe edit surface.".to_string(),
    ];
    if bundle.search_hits.len() > 3 || bundle.task.split_whitespace().count() > 8 {
        first_actions.push(
            "Keep the decomposition visible while working so independent slices can be handled in parallel when the host allows it."
                .to_string(),
        );
    }
    if config.prefer_full_access {
        first_actions.push(
            "If the workspace is trusted and the host exposes it, switch to full-access or auto-approve before large edits or test runs."
                .to_string(),
        );
    }
    if !bundle.recommended_skills.is_empty() {
        first_actions.push(
            "Map the first workstream to one of the recommended external skills before implementation so the plan is grounded in a concrete playbook."
                .to_string(),
        );
    }

    let delegate_ready_count = workstreams
        .iter()
        .filter(|workstream| workstream.can_run_in_parallel)
        .count();

    TaskDecomposition {
        task: bundle.task.clone(),
        execution_mode: config.execution_mode.clone(),
        prefer_full_access: config.prefer_full_access,
        can_parallelize,
        summary: format!(
            "Task decomposed into {} workstream(s), with {} delegate-ready slice(s), using cached search hits, memories, and repo state. Execution mode is `{}`.",
            workstreams.len(),
            delegate_ready_count,
            config.execution_mode
        ),
        recommended_starting_tools: vec![
            "orchestrate_task".to_string(),
            "warm_workspace".to_string(),
            "build_context_bundle".to_string(),
            "decompose_task".to_string(),
            "search_workspace".to_string(),
            "search_skills".to_string(),
            "remember_memory".to_string(),
        ],
        shared_context,
        recommended_skills: bundle.recommended_skills.clone(),
        workstreams,
        coordination_notes,
        first_actions,
    }
}

fn agent_role_for_workstream(index: usize, workstream: &TaskWorkstream) -> &'static str {
    if index == 0 {
        "coordinator"
    } else if workstream.can_run_in_parallel {
        "parallel-specialist"
    } else {
        "specialist"
    }
}

fn build_completion_criteria(workstream: &TaskWorkstream) -> Vec<String> {
    let mut criteria = vec![
        format!(
            "Advance the workstream objective while staying primarily inside {:?}.",
            workstream.recommended_files
        ),
        "Call out cross-scope contracts before touching shared files or interfaces.".to_string(),
        "Leave durable workflow or architecture decisions in remember_memory if another session will need them."
            .to_string(),
    ];

    if !workstream.matching_symbols.is_empty() {
        criteria.push(format!(
            "Verify the behavior around symbols {:?} after the change.",
            workstream.matching_symbols
        ));
    }

    criteria
}

fn build_subagent_prompt(
    task: &str,
    workstream: &TaskWorkstream,
    recommended_skills: &[SkillMatch],
    shared_context: &[String],
    execution_mode: &str,
    prefer_full_access: bool,
    agent_role: &str,
) -> String {
    let skill_paths = recommended_skills
        .iter()
        .map(|skill| format!("{} ({})", skill.name, skill.path))
        .collect::<Vec<_>>();
    let access_hint = if prefer_full_access {
        "If the host exposes a trusted full-access mode, you may use it for this slice after the initial read-only scan."
    } else {
        "Stay inside the host's default approval boundary unless this slice clearly needs more access."
    };

    format!(
        "Role: {agent_role}. Task: {task}. Workstream: {} [{}]. Objective: {}. Prioritize files: {:?}. Matching symbols: {:?}. Shared context: {:?}. Load these skills first: {:?}. Handoff: {}. Execution mode: {execution_mode}. {}",
        workstream.title,
        workstream.id,
        workstream.objective,
        workstream.recommended_files,
        workstream.matching_symbols,
        shared_context,
        skill_paths,
        workstream.handoff,
        access_hint
    )
}

fn build_task_orchestration(
    bundle: ContextBundle,
    decomposition: TaskDecomposition,
    workstream_skill_matches: &HashMap<String, Vec<SkillMatch>>,
    config: &ServerConfig,
) -> TaskOrchestration {
    let mut stages = Vec::new();
    if let Some(primary) = decomposition.workstreams.first() {
        stages.push(OrchestrationStage {
            id: "stage-1".to_string(),
            title: "Coordinator setup".to_string(),
            objective: "Open shared context, confirm file ownership, and establish any cross-scope contracts before fan-out."
                .to_string(),
            workstream_ids: vec![primary.id.clone()],
            run_in_parallel: false,
        });
    }

    let parallel_ids = decomposition
        .workstreams
        .iter()
        .skip(1)
        .filter(|workstream| workstream.can_run_in_parallel)
        .map(|workstream| workstream.id.clone())
        .collect::<Vec<_>>();
    if !parallel_ids.is_empty() {
        stages.push(OrchestrationStage {
            id: format!("stage-{}", stages.len() + 1),
            title: "Parallel implementation".to_string(),
            objective: "Delegate independent workstreams whose files do not overlap once the coordinator has locked scope boundaries."
                .to_string(),
            workstream_ids: parallel_ids,
            run_in_parallel: true,
        });
    }

    let sequential_ids = decomposition
        .workstreams
        .iter()
        .skip(1)
        .filter(|workstream| !workstream.can_run_in_parallel)
        .map(|workstream| workstream.id.clone())
        .collect::<Vec<_>>();
    if !sequential_ids.is_empty() {
        stages.push(OrchestrationStage {
            id: format!("stage-{}", stages.len() + 1),
            title: "Sequential follow-up".to_string(),
            objective: "Handle overlapping or coordination-heavy slices after the delegate-ready work is merged."
                .to_string(),
            workstream_ids: sequential_ids,
            run_in_parallel: false,
        });
    }

    let subagent_specs = decomposition
        .workstreams
        .iter()
        .enumerate()
        .map(|(index, workstream)| {
            let agent_role = agent_role_for_workstream(index, workstream).to_string();
            let recommended_skills = workstream_skill_matches
                .get(&workstream.id)
                .cloned()
                .unwrap_or_default();
            let completion_criteria = build_completion_criteria(workstream);
            let prompt = build_subagent_prompt(
                &bundle.task,
                workstream,
                &recommended_skills,
                &decomposition.shared_context,
                &config.execution_mode,
                config.prefer_full_access,
                &agent_role,
            );

            SubagentSpec {
                workstream_id: workstream.id.clone(),
                title: workstream.title.clone(),
                agent_role,
                run_in_parallel: workstream.can_run_in_parallel,
                objective: workstream.objective.clone(),
                recommended_files: workstream.recommended_files.clone(),
                matching_symbols: workstream.matching_symbols.clone(),
                shared_context: decomposition.shared_context.clone(),
                recommended_skills,
                completion_criteria,
                handoff: workstream.handoff.clone(),
                prompt,
            }
        })
        .collect::<Vec<_>>();

    let parallel_ready_count = subagent_specs
        .iter()
        .filter(|spec| spec.run_in_parallel)
        .count();
    let recommended_host_steps = vec![
        "Run `warm_workspace` first if the cache is cold or the workspace has changed materially."
            .to_string(),
        "Start with the `coordinator` subagent spec so shared context and file ownership are locked before any fan-out."
            .to_string(),
        "Open the recommended skill paths attached to each subagent spec before implementation so each slice follows a concrete playbook."
            .to_string(),
        "Only fan out the `parallel-specialist` specs together, and only if the host actually supports subagents and tool approvals are already settled."
            .to_string(),
        "Merge results back through the coordinator before editing shared files, running repo-wide checks, or finalizing the handoff."
            .to_string(),
        "Write durable decisions with `remember_memory` once the orchestration completes."
            .to_string(),
    ];

    let mut host_constraints = decomposition.coordination_notes.clone();
    host_constraints.push(
        "The module now defines subagent-ready briefs, but the host still decides whether those briefs become actual parallel agents."
            .to_string(),
    );

    TaskOrchestration {
        task: bundle.task.clone(),
        execution_mode: config.execution_mode.clone(),
        prefer_full_access: config.prefer_full_access,
        summary: format!(
            "Prepared {} orchestration stage(s) and {} subagent spec(s), with {} parallel-ready delegate(s).",
            stages.len(),
            subagent_specs.len(),
            parallel_ready_count
        ),
        context_bundle: bundle,
        decomposition,
        stages,
        subagent_specs,
        recommended_host_steps,
        host_constraints,
    }
}

fn is_likely_shared_file(path: &str) -> bool {
    matches!(
        path,
        "Cargo.toml"
            | "package.json"
            | "pnpm-workspace.yaml"
            | "README.md"
            | "README"
            | "tsconfig.json"
            | "pyproject.toml"
    )
}

fn format_cache_status(status: &CacheStatus) -> String {
    format!(
        "# Codex Companion Cache\n\n\
Workspace: `{}`\n\
Workspace ID: `{}`\n\
Cache dir: `{}`\n\
Indexed at: `{}`\n\n\
- Indexed files: {}\n\
- Indexed bytes: {}\n\
- Reused files: {}\n\
- Reindexed files: {}\n\
- Skipped files: {}\n",
        status.workspace_root,
        status.workspace_id,
        status.cache_dir,
        status.indexed_at,
        status.indexed_files,
        status.indexed_bytes,
        status.scan_metrics.reused_files,
        status.scan_metrics.reindexed_files,
        status.scan_metrics.skipped_files,
    )
}

fn format_warmup_status(status: &WarmupStatus) -> String {
    format!(
        "# Codex Companion Warmup\n\n\
Workspace: `{}`\n\
Elapsed: {} ms\n\
Git warmed: {}\n\
Memory warmed: {}\n\
Skills warmed: {}\n\n{}",
        status.workspace_root,
        status.elapsed_ms,
        status.warmed_git,
        status.warmed_memory,
        status.warmed_skills,
        format_cache_status(&status.cache_status)
    )
}

fn format_context_bundle(bundle: &ContextBundle) -> String {
    let mut output = String::new();
    output.push_str("# Codex Context Bundle\n\n");
    output.push_str(&format!("Task: {}\n\n", bundle.task));
    output.push_str("## Workspace Overview\n");
    output.push_str(&format!(
        "- Root: `{}`\n- Indexed at: `{}`\n- Files: {}\n- Bytes: {}\n",
        bundle.overview.workspace_root,
        bundle.overview.indexed_at,
        bundle.overview.total_indexed_files,
        bundle.overview.total_indexed_bytes
    ));

    if !bundle.overview.major_languages.is_empty() {
        output.push_str("\n## Languages\n");
        for language in &bundle.overview.major_languages {
            output.push_str(&format!("- {}: {}\n", language.language, language.files));
        }
    }

    if !bundle.search_hits.is_empty() {
        output.push_str("\n## Relevant Files\n");
        for hit in &bundle.search_hits {
            output.push_str(&format!(
                "- `{}` (score {:.1})\n{}\n{}\n",
                hit.path, hit.score, hit.summary, hit.snippet
            ));
        }
    }

    if !bundle.memories.is_empty() {
        output.push_str("\n## Recalled Memory\n");
        for memory in &bundle.memories {
            output.push_str(&format!(
                "- **{}** [{}]\n{}\n",
                memory.title,
                memory.tags.join(", "),
                memory.content
            ));
        }
    }

    if !bundle.recommended_skills.is_empty() {
        output.push_str("\n## Recommended Skills\n");
        for skill in &bundle.recommended_skills {
            output.push_str(&format!(
                "- {} ({})\n{}\n{}\n",
                skill.name, skill.category, skill.description, skill.path
            ));
        }
    }

    if let Some(git) = &bundle.recent_changes {
        if git.available {
            output.push_str("\n## Recent Changes\n");
            if let Some(branch) = &git.branch {
                output.push_str(&format!("- Branch: {}\n", branch));
            }
            for line in &git.status_lines {
                output.push_str(&format!("- {}\n", line));
            }
            for commit in &git.recent_commits {
                output.push_str(&format!("- {}\n", commit));
            }
        }
    }

    if !bundle.suggested_next_actions.is_empty() {
        output.push_str("\n## Suggested Next Actions\n");
        for action in &bundle.suggested_next_actions {
            output.push_str(&format!("- {}\n", action));
        }
    }

    output
}

fn format_task_orchestration(orchestration: &TaskOrchestration) -> String {
    let mut output = String::new();
    output.push_str("# Codex Task Orchestration\n\n");
    output.push_str(&format!("Task: {}\n", orchestration.task));
    output.push_str(&format!("Summary: {}\n", orchestration.summary));
    output.push_str(&format!(
        "Execution mode: `{}`\nPrefer full access: `{}`\n\n",
        orchestration.execution_mode, orchestration.prefer_full_access
    ));

    if !orchestration.stages.is_empty() {
        output.push_str("## Stages\n");
        for stage in &orchestration.stages {
            output.push_str(&format!(
                "- {} [{}]\n  Objective: {}\n  Workstreams: {}\n  Parallel: {}\n",
                stage.title,
                stage.id,
                stage.objective,
                if stage.workstream_ids.is_empty() {
                    "-".to_string()
                } else {
                    stage.workstream_ids.join(", ")
                },
                stage.run_in_parallel
            ));
        }
        output.push('\n');
    }

    if !orchestration.subagent_specs.is_empty() {
        output.push_str("## Subagent Specs\n");
        for spec in &orchestration.subagent_specs {
            let skill_summary = if spec.recommended_skills.is_empty() {
                "-".to_string()
            } else {
                spec.recommended_skills
                    .iter()
                    .map(|skill| format!("{} ({})", skill.name, skill.path))
                    .collect::<Vec<_>>()
                    .join(", ")
            };

            output.push_str(&format!(
                "- {} [{} | role: {}]\n  Objective: {}\n  Files: {}\n  Symbols: {}\n  Skills: {}\n  Parallel: {}\n  Completion: {}\n  Prompt: {}\n",
                spec.title,
                spec.workstream_id,
                spec.agent_role,
                spec.objective,
                if spec.recommended_files.is_empty() {
                    "-".to_string()
                } else {
                    spec.recommended_files.join(", ")
                },
                if spec.matching_symbols.is_empty() {
                    "-".to_string()
                } else {
                    spec.matching_symbols.join(", ")
                },
                skill_summary,
                spec.run_in_parallel,
                spec.completion_criteria.join(" | "),
                spec.prompt
            ));
        }
        output.push('\n');
    }

    if !orchestration.recommended_host_steps.is_empty() {
        output.push_str("## Host Steps\n");
        for step in &orchestration.recommended_host_steps {
            output.push_str(&format!("- {}\n", step));
        }
        output.push('\n');
    }

    if !orchestration.host_constraints.is_empty() {
        output.push_str("## Host Constraints\n");
        for note in &orchestration.host_constraints {
            output.push_str(&format!("- {}\n", note));
        }
        output.push('\n');
    }

    output.push_str("## Context Bundle Snapshot\n");
    output.push_str(&format_context_bundle(&orchestration.context_bundle));
    output.push('\n');
    output.push_str("## Decomposition Snapshot\n");
    output.push_str(&format_task_decomposition(&orchestration.decomposition));

    output
}

fn format_task_decomposition(decomposition: &TaskDecomposition) -> String {
    let mut output = String::new();
    output.push_str("# Codex Task Decomposition\n\n");
    output.push_str(&format!("Task: {}\n", decomposition.task));
    output.push_str(&format!("Summary: {}\n", decomposition.summary));
    output.push_str(&format!(
        "Execution mode: `{}`\nPrefer full access: `{}`\nCan parallelize: `{}`\n\n",
        decomposition.execution_mode,
        decomposition.prefer_full_access,
        decomposition.can_parallelize
    ));

    if !decomposition.recommended_starting_tools.is_empty() {
        output.push_str("## Starting Tools\n");
        for tool in &decomposition.recommended_starting_tools {
            output.push_str(&format!("- {}\n", tool));
        }
        output.push('\n');
    }

    if !decomposition.shared_context.is_empty() {
        output.push_str("## Shared Context\n");
        for item in &decomposition.shared_context {
            output.push_str(&format!("- {}\n", item));
        }
        output.push('\n');
    }

    output.push_str("## Workstreams\n");
    for workstream in &decomposition.workstreams {
        output.push_str(&format!(
            "- {} [{}]\n  Objective: {}\n  Rationale: {}\n  Files: {}\n  Symbols: {}\n  Skills: {}\n  Parallel: {}\n  Handoff: {}\n",
            workstream.title,
            workstream.id,
            workstream.objective,
            workstream.rationale,
            if workstream.recommended_files.is_empty() {
                "-".to_string()
            } else {
                workstream.recommended_files.join(", ")
            },
            if workstream.matching_symbols.is_empty() {
                "-".to_string()
            } else {
                workstream.matching_symbols.join(", ")
            },
            if workstream.recommended_skills.is_empty() {
                "-".to_string()
            } else {
                workstream.recommended_skills.join(", ")
            },
            workstream.can_run_in_parallel,
            workstream.handoff
        ));
    }

    if !decomposition.recommended_skills.is_empty() {
        output.push_str("\n## Recommended Skills\n");
        for skill in &decomposition.recommended_skills {
            output.push_str(&format!("- {} ({})\n", skill.name, skill.path));
        }
    }

    if !decomposition.coordination_notes.is_empty() {
        output.push_str("\n## Coordination Notes\n");
        for note in &decomposition.coordination_notes {
            output.push_str(&format!("- {}\n", note));
        }
    }

    if !decomposition.first_actions.is_empty() {
        output.push_str("\n## First Actions\n");
        for action in &decomposition.first_actions {
            output.push_str(&format!("- {}\n", action));
        }
    }

    output
}

fn format_memory_results(results: &MemorySearchResults) -> String {
    let mut output = String::new();
    output.push_str("# Codex Memory Recall\n\n");
    if let Some(query) = &results.query {
        output.push_str(&format!("Query: `{}`\n\n", query));
    }

    if results.matches.is_empty() {
        output.push_str("No stored memories matched.\n");
        return output;
    }

    for memory in &results.matches {
        output.push_str(&format!(
            "## {}\n- Tags: {}\n- Importance: {}\n- Updated: {}\n\n{}\n\n",
            memory.title,
            memory.tags.join(", "),
            memory.importance,
            memory.updated_at,
            memory.content
        ));
    }

    output
}

fn format_skill_results(results: &SkillSearchResults) -> String {
    let mut output = String::new();
    output.push_str("# Codex Skills\n\n");
    if let Some(query) = &results.query {
        output.push_str(&format!("Query: `{}`\n\n", query));
    }

    if results.hits.is_empty() {
        output.push_str("No external skills matched.\n");
        return output;
    }

    for skill in &results.hits {
        output.push_str(&format!(
            "## {} ({})\n- Path: {}\n- Score: {:.1}\n- Match reasons: {}\n\n{}\n\n",
            skill.name,
            skill.category,
            skill.path,
            skill.score,
            if skill.match_reasons.is_empty() {
                "-".to_string()
            } else {
                skill.match_reasons.join(", ")
            },
            skill.description
        ));
    }

    output
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init();
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::PathBuf,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::{
        build_task_decomposition, build_task_orchestration, can_reuse_task_cache,
        configured_skill_roots, default_skill_root_candidates, existing_skill_roots,
        git_summary_cache_key, normalize_execution_mode, prune_timed_cache, RuntimeState,
        ServerConfig, TimedCacheEntry,
    };
    use crate::model::{
        ContextBundle, MemoryRecord, ScanMetrics, SearchHit, SkillMatch, WorkspaceOverview,
    };

    #[test]
    fn git_summary_cache_key_tracks_query_shape() {
        assert_ne!(
            git_summary_cache_key(6, true),
            git_summary_cache_key(3, true)
        );
        assert_ne!(
            git_summary_cache_key(6, true),
            git_summary_cache_key(6, false)
        );
    }

    #[test]
    fn default_skill_root_candidates_are_empty_for_public_defaults() {
        assert!(default_skill_root_candidates().is_empty());
    }

    #[test]
    fn configured_skill_roots_prefers_explicit_json() {
        let explicit_root = r"D:\custom\skills".to_string();
        let raw = serde_json::to_string(&vec![explicit_root.clone()]).unwrap();

        assert_eq!(
            configured_skill_roots(Some(raw)),
            vec![PathBuf::from(explicit_root)]
        );
    }

    #[test]
    fn existing_skill_roots_filters_missing_paths() {
        let existing_root = std::env::temp_dir().join(format!(
            "codex-companion-skill-root-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let missing_root = existing_root.join("missing");
        fs::create_dir_all(&existing_root).unwrap();

        let roots = existing_skill_roots(vec![existing_root.clone(), missing_root]);
        assert_eq!(roots, vec![existing_root.clone()]);

        fs::remove_dir_all(existing_root).unwrap();
    }

    #[test]
    fn prune_timed_cache_drops_expired_and_oldest_entries() {
        let mut cache = HashMap::new();
        cache.insert(
            "expired".to_string(),
            TimedCacheEntry {
                value: "expired".to_string(),
                loaded_at: Instant::now() - Duration::from_secs(30),
            },
        );
        cache.insert(
            "oldest".to_string(),
            TimedCacheEntry {
                value: "oldest".to_string(),
                loaded_at: Instant::now() - Duration::from_secs(3),
            },
        );
        cache.insert(
            "newer".to_string(),
            TimedCacheEntry {
                value: "newer".to_string(),
                loaded_at: Instant::now() - Duration::from_secs(2),
            },
        );
        cache.insert(
            "newest".to_string(),
            TimedCacheEntry {
                value: "newest".to_string(),
                loaded_at: Instant::now() - Duration::from_secs(1),
            },
        );

        prune_timed_cache(&mut cache, 10, 2);

        assert!(!cache.contains_key("expired"));
        assert!(!cache.contains_key("oldest"));
        assert!(cache.contains_key("newer"));
        assert!(cache.contains_key("newest"));
    }

    #[test]
    fn execution_mode_normalizes_aliases() {
        assert_eq!(
            normalize_execution_mode(Some("fast".to_string())),
            "autonomous"
        );
        assert_eq!(
            normalize_execution_mode(Some("safe".to_string())),
            "careful"
        );
        assert_eq!(
            normalize_execution_mode(Some("whatever".to_string())),
            "balanced"
        );
    }

    #[test]
    fn task_cache_requires_fresh_index() {
        let runtime = RuntimeState {
            index_loaded_at: Some(Instant::now() - Duration::from_secs(10)),
            ..RuntimeState::default()
        };
        let config = ServerConfig {
            cache_dir_override: None,
            max_file_bytes: 262_144,
            max_indexed_files: 1_500,
            ignore_globs: Vec::new(),
            enable_git_tools: true,
            refresh_window_secs: 5,
            git_cache_ttl_secs: 15,
            bundle_cache_ttl_secs: 180,
            skill_cache_ttl_secs: 300,
            prewarm_on_start: true,
            execution_mode: "balanced".to_string(),
            prefer_full_access: true,
            max_parallel_workstreams: 4,
            skill_roots: Vec::new(),
            skill_file_globs: vec!["**/*.md".to_string()],
            max_skill_bytes: 131_072,
            max_skills_per_query: 4,
        };

        assert!(!can_reuse_task_cache(&runtime, &config));
    }

    #[test]
    fn task_cache_requires_fresh_skill_catalog_when_skills_are_enabled() {
        let runtime = RuntimeState {
            index_loaded_at: Some(Instant::now()),
            skill_catalog_loaded_at: Some(Instant::now() - Duration::from_secs(10)),
            ..RuntimeState::default()
        };
        let config = ServerConfig {
            cache_dir_override: None,
            max_file_bytes: 262_144,
            max_indexed_files: 1_500,
            ignore_globs: Vec::new(),
            enable_git_tools: true,
            refresh_window_secs: 120,
            git_cache_ttl_secs: 15,
            bundle_cache_ttl_secs: 180,
            skill_cache_ttl_secs: 5,
            prewarm_on_start: true,
            execution_mode: "balanced".to_string(),
            prefer_full_access: true,
            max_parallel_workstreams: 4,
            skill_roots: vec![PathBuf::from(r"D:\downloads\agency-agents")],
            skill_file_globs: vec!["**/*.md".to_string()],
            max_skill_bytes: 131_072,
            max_skills_per_query: 4,
        };

        assert!(!can_reuse_task_cache(&runtime, &config));
    }

    #[test]
    fn decomposition_creates_parallel_workstreams_for_distinct_scopes() {
        let bundle = ContextBundle {
            task: "add cache invalidation and docs".to_string(),
            overview: WorkspaceOverview {
                workspace_root: "/tmp/demo".to_string(),
                indexed_at: "2026-03-17T00:00:00Z".to_string(),
                total_indexed_files: 3,
                total_indexed_bytes: 100,
                major_languages: Vec::new(),
                top_directories: Vec::new(),
                key_files: vec!["Cargo.toml".to_string(), "README.md".to_string()],
                highlights: Vec::new(),
                scan_metrics: ScanMetrics::default(),
            },
            search_hits: vec![
                SearchHit {
                    path: "src/cache.rs".to_string(),
                    language: "rust".to_string(),
                    score: 20.0,
                    line: Some(10),
                    snippet: "fn refresh_cache() {}".to_string(),
                    summary: "cache refresh logic".to_string(),
                    matching_symbols: vec!["refresh_cache".to_string()],
                },
                SearchHit {
                    path: "docs/usage.md".to_string(),
                    language: "markdown".to_string(),
                    score: 15.0,
                    line: Some(5),
                    snippet: "# Usage".to_string(),
                    summary: "user-facing docs".to_string(),
                    matching_symbols: vec!["Usage".to_string()],
                },
            ],
            memories: vec![MemoryRecord {
                id: "1".to_string(),
                title: "Cache policy".to_string(),
                content: "Remember to invalidate on writes.".to_string(),
                tags: vec!["cache".to_string()],
                importance: "high".to_string(),
                created_at: "2026-03-17T00:00:00Z".to_string(),
                updated_at: "2026-03-17T00:00:00Z".to_string(),
            }],
            recommended_skills: vec![SkillMatch {
                id: "skill-1".to_string(),
                name: "Rapid Prototyper".to_string(),
                description: "Builds quickly".to_string(),
                path: "engineering/engineering-rapid-prototyper.md".to_string(),
                source_root: "/tmp/agency-agents".to_string(),
                category: "engineering".to_string(),
                emoji: None,
                vibe: None,
                preview: "Build fast".to_string(),
                score: 12.0,
                match_reasons: vec!["name".to_string()],
            }],
            recent_changes: None,
            suggested_next_actions: Vec::new(),
        };

        let config = ServerConfig {
            cache_dir_override: None,
            max_file_bytes: 262_144,
            max_indexed_files: 1_500,
            ignore_globs: Vec::new(),
            enable_git_tools: true,
            refresh_window_secs: 120,
            git_cache_ttl_secs: 15,
            bundle_cache_ttl_secs: 180,
            skill_cache_ttl_secs: 300,
            prewarm_on_start: true,
            execution_mode: "autonomous".to_string(),
            prefer_full_access: true,
            max_parallel_workstreams: 4,
            skill_roots: Vec::new(),
            skill_file_globs: vec!["**/*.md".to_string()],
            max_skill_bytes: 131_072,
            max_skills_per_query: 4,
        };

        let decomposition = build_task_decomposition(&bundle, &config);
        assert!(decomposition.can_parallelize);
        assert!(decomposition.workstreams.len() >= 2);
    }

    #[test]
    fn decomposition_splits_single_top_level_scope_using_deeper_paths() {
        let bundle = ContextBundle {
            task: "speed up the plugin runtime".to_string(),
            overview: WorkspaceOverview {
                workspace_root: "/tmp/demo".to_string(),
                indexed_at: "2026-03-17T00:00:00Z".to_string(),
                total_indexed_files: 4,
                total_indexed_bytes: 100,
                major_languages: Vec::new(),
                top_directories: Vec::new(),
                key_files: vec!["Cargo.toml".to_string(), "src/lib.rs".to_string()],
                highlights: Vec::new(),
                scan_metrics: ScanMetrics::default(),
            },
            search_hits: vec![
                SearchHit {
                    path: "src/lib.rs".to_string(),
                    language: "rust".to_string(),
                    score: 20.0,
                    line: Some(10),
                    snippet: "fn activate() {}".to_string(),
                    summary: "extension entrypoint".to_string(),
                    matching_symbols: vec!["activate".to_string()],
                },
                SearchHit {
                    path: "src/cli.rs".to_string(),
                    language: "rust".to_string(),
                    score: 18.0,
                    line: Some(8),
                    snippet: "fn run() {}".to_string(),
                    summary: "cli helpers".to_string(),
                    matching_symbols: vec!["run".to_string()],
                },
                SearchHit {
                    path: "src/cache/index.rs".to_string(),
                    language: "rust".to_string(),
                    score: 16.0,
                    line: Some(12),
                    snippet: "fn warm() {}".to_string(),
                    summary: "cache internals".to_string(),
                    matching_symbols: vec!["warm".to_string()],
                },
            ],
            memories: Vec::new(),
            recommended_skills: Vec::new(),
            recent_changes: None,
            suggested_next_actions: Vec::new(),
        };

        let config = ServerConfig {
            cache_dir_override: None,
            max_file_bytes: 262_144,
            max_indexed_files: 1_500,
            ignore_globs: Vec::new(),
            enable_git_tools: true,
            refresh_window_secs: 120,
            git_cache_ttl_secs: 15,
            bundle_cache_ttl_secs: 180,
            skill_cache_ttl_secs: 300,
            prewarm_on_start: true,
            execution_mode: "autonomous".to_string(),
            prefer_full_access: true,
            max_parallel_workstreams: 4,
            skill_roots: Vec::new(),
            skill_file_globs: vec!["**/*.md".to_string()],
            max_skill_bytes: 131_072,
            max_skills_per_query: 4,
        };

        let decomposition = build_task_decomposition(&bundle, &config);
        assert!(decomposition.workstreams.len() >= 2);
        assert!(decomposition.workstreams.iter().any(|stream| stream
            .recommended_files
            .iter()
            .any(|path| path == "src/lib.rs")));
        assert!(decomposition.workstreams.iter().any(|stream| stream
            .recommended_files
            .iter()
            .any(|path| path == "src/cli.rs")));
    }

    #[test]
    fn orchestration_builds_stage_and_subagent_contracts() {
        let bundle = ContextBundle {
            task: "add cache invalidation and docs".to_string(),
            overview: WorkspaceOverview {
                workspace_root: "/tmp/demo".to_string(),
                indexed_at: "2026-03-17T00:00:00Z".to_string(),
                total_indexed_files: 3,
                total_indexed_bytes: 100,
                major_languages: Vec::new(),
                top_directories: Vec::new(),
                key_files: vec!["Cargo.toml".to_string(), "README.md".to_string()],
                highlights: Vec::new(),
                scan_metrics: ScanMetrics::default(),
            },
            search_hits: vec![
                SearchHit {
                    path: "src/cache.rs".to_string(),
                    language: "rust".to_string(),
                    score: 20.0,
                    line: Some(10),
                    snippet: "fn refresh_cache() {}".to_string(),
                    summary: "cache refresh logic".to_string(),
                    matching_symbols: vec!["refresh_cache".to_string()],
                },
                SearchHit {
                    path: "docs/usage.md".to_string(),
                    language: "markdown".to_string(),
                    score: 15.0,
                    line: Some(5),
                    snippet: "# Usage".to_string(),
                    summary: "user-facing docs".to_string(),
                    matching_symbols: vec!["Usage".to_string()],
                },
            ],
            memories: vec![MemoryRecord {
                id: "1".to_string(),
                title: "Cache policy".to_string(),
                content: "Remember to invalidate on writes.".to_string(),
                tags: vec!["cache".to_string()],
                importance: "high".to_string(),
                created_at: "2026-03-17T00:00:00Z".to_string(),
                updated_at: "2026-03-17T00:00:00Z".to_string(),
            }],
            recommended_skills: vec![SkillMatch {
                id: "skill-1".to_string(),
                name: "Rapid Prototyper".to_string(),
                description: "Builds quickly".to_string(),
                path: "engineering/engineering-rapid-prototyper.md".to_string(),
                source_root: "/tmp/agency-agents".to_string(),
                category: "engineering".to_string(),
                emoji: None,
                vibe: None,
                preview: "Build fast".to_string(),
                score: 12.0,
                match_reasons: vec!["name".to_string()],
            }],
            recent_changes: None,
            suggested_next_actions: Vec::new(),
        };

        let config = ServerConfig {
            cache_dir_override: None,
            max_file_bytes: 262_144,
            max_indexed_files: 1_500,
            ignore_globs: Vec::new(),
            enable_git_tools: true,
            refresh_window_secs: 120,
            git_cache_ttl_secs: 15,
            bundle_cache_ttl_secs: 180,
            skill_cache_ttl_secs: 300,
            prewarm_on_start: true,
            execution_mode: "autonomous".to_string(),
            prefer_full_access: true,
            max_parallel_workstreams: 4,
            skill_roots: Vec::new(),
            skill_file_globs: vec!["**/*.md".to_string()],
            max_skill_bytes: 131_072,
            max_skills_per_query: 4,
        };

        let decomposition = build_task_decomposition(&bundle, &config);
        let workstream_skill_matches = decomposition
            .workstreams
            .iter()
            .enumerate()
            .map(|(index, workstream)| {
                (
                    workstream.id.clone(),
                    vec![SkillMatch {
                        id: format!("skill-{}", index + 1),
                        name: format!("Skill {}", index + 1),
                        description: "Scoped workstream skill".to_string(),
                        path: format!("engineering/skill-{}.md", index + 1),
                        source_root: "/tmp/agency-agents".to_string(),
                        category: "engineering".to_string(),
                        emoji: None,
                        vibe: None,
                        preview: "Scoped".to_string(),
                        score: 10.0,
                        match_reasons: vec!["query".to_string()],
                    }],
                )
            })
            .collect::<HashMap<_, _>>();

        let orchestration =
            build_task_orchestration(bundle, decomposition, &workstream_skill_matches, &config);

        assert_eq!(
            orchestration.subagent_specs.len(),
            orchestration.decomposition.workstreams.len()
        );
        assert_eq!(orchestration.subagent_specs[0].agent_role, "coordinator");
        assert!(orchestration
            .subagent_specs
            .iter()
            .all(|spec| !spec.prompt.is_empty()));
        assert!(orchestration
            .subagent_specs
            .iter()
            .all(|spec| !spec.recommended_skills.is_empty()));
        assert!(orchestration
            .stages
            .iter()
            .any(|stage| stage.title == "Coordinator setup"));
        assert!(orchestration
            .stages
            .iter()
            .any(|stage| stage.run_in_parallel));
    }
}
