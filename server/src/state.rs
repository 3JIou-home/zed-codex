use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::warn;

use crate::{
    cache::{
        load_memory_store, load_persistent_generations, load_skill_catalog, load_workspace_index,
        resolve_workspace_cache, save_memory_store, save_skill_catalog, PersistentCacheGenerations,
        WorkspaceCache,
    },
    config::ServerConfig,
    git_tools::collect_git_summary,
    indexer::{
        build_workspace_overview, index_options_signature, refresh_workspace_index,
        search_workspace_cached, RefreshedWorkspaceIndex,
    },
    model::{
        CacheStatus, ContextBundle, GitSummary, MemoryRecord, MemoryStore, SearchHit, SkillCatalog,
        SkillSearchResults, TaskDecomposition, TaskOrchestration, WarmupStatus, WorkspaceIndex,
    },
    planning::{
        build_suggested_next_actions, build_task_decomposition, build_task_orchestration,
        build_workstream_skill_query, fallback_workstream_skills, merge_skill_matches,
    },
    skills::{
        build_skill_catalog, search_skills, skill_index_options_signature, SkillIndexOptions,
    },
    text::tokenize_query,
};

const MAX_GIT_SUMMARY_CACHE_ENTRIES: usize = 8;
const MAX_BUNDLE_CACHE_ENTRIES: usize = 32;
const MAX_DECOMPOSITION_CACHE_ENTRIES: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct TimedCacheEntry<T> {
    pub(crate) value: T,
    pub(crate) loaded_at: Instant,
    pub(crate) versions: TaskCacheVersions,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct TaskCacheVersions {
    pub(crate) index: u64,
    pub(crate) memory: u64,
    pub(crate) skills: u64,
}

#[derive(Debug, Default)]
pub(crate) struct RuntimeState {
    pub(crate) index: Option<Arc<WorkspaceIndex>>,
    pub(crate) index_loaded_at: Option<Instant>,
    pub(crate) index_generation: u64,
    pub(crate) memories: Option<MemoryStore>,
    pub(crate) memory_generation: u64,
    pub(crate) git_summary_cache: HashMap<String, TimedCacheEntry<GitSummary>>,
    pub(crate) skill_catalog: Option<SkillCatalog>,
    pub(crate) skill_catalog_loaded_at: Option<Instant>,
    pub(crate) skills_generation: u64,
    pub(crate) bundle_cache: HashMap<String, TimedCacheEntry<ContextBundle>>,
    pub(crate) decomposition_cache: HashMap<String, TimedCacheEntry<TaskDecomposition>>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) root: PathBuf,
    pub(crate) cache: WorkspaceCache,
    pub(crate) config: ServerConfig,
    runtime: Arc<Mutex<RuntimeState>>,
    memory_write: Arc<Mutex<()>>,
    index_refresh: Arc<Mutex<()>>,
    skill_refresh: Arc<Mutex<()>>,
    git_refresh: Arc<Mutex<()>>,
}

impl AppState {
    pub(crate) fn new(root: PathBuf, config: ServerConfig) -> Result<Self> {
        let cache = resolve_workspace_cache(&root, config.cache_dir_override.as_ref())?;
        let persistent_generations = load_persistent_generations(&cache).unwrap_or_else(|error| {
            warn!(
                "failed to load persistent cache generations from {}: {error}",
                cache.db_path.display()
            );
            Default::default()
        });
        let runtime = RuntimeState {
            index_generation: persistent_generations.index,
            memory_generation: persistent_generations.memory,
            skills_generation: persistent_generations.skills,
            ..RuntimeState::default()
        };
        Ok(Self {
            root,
            cache,
            config,
            runtime: Arc::new(Mutex::new(runtime)),
            memory_write: Arc::new(Mutex::new(())),
            index_refresh: Arc::new(Mutex::new(())),
            skill_refresh: Arc::new(Mutex::new(())),
            git_refresh: Arc::new(Mutex::new(())),
        })
    }

    async fn persistent_generations_snapshot(&self) -> Option<PersistentCacheGenerations> {
        let cache = self.cache.clone();
        match tokio::task::spawn_blocking(move || load_persistent_generations(&cache)).await {
            Ok(Ok(generations)) => Some(generations),
            Ok(Err(error)) => {
                warn!(
                    "failed to refresh persistent cache generations from {}: {error}",
                    self.cache.db_path.display()
                );
                None
            }
            Err(error) => {
                warn!("persistent generation refresh task failed: {error}");
                None
            }
        }
    }

    fn skill_index_options(&self) -> SkillIndexOptions {
        SkillIndexOptions {
            roots: self.config.skill_roots.clone(),
            include_globs: self.config.skill_file_globs.clone(),
            max_skill_bytes: self.config.max_skill_bytes,
        }
    }

    pub(crate) async fn ensure_index(&self, force_refresh: bool) -> Result<Arc<WorkspaceIndex>> {
        if !force_refresh {
            let persistent_generations = self.persistent_generations_snapshot().await;
            let runtime = self.runtime.lock().await;
            if let (Some(index), Some(index_loaded_at)) = (&runtime.index, runtime.index_loaded_at)
            {
                if can_reuse_loaded_index(
                    index_loaded_at,
                    runtime.index_generation,
                    &self.config,
                    persistent_generations,
                ) {
                    return Ok(Arc::clone(index));
                }
            }
        }

        let _refresh_guard = self.index_refresh.lock().await;
        {
            let persistent_generations = if force_refresh {
                None
            } else {
                self.persistent_generations_snapshot().await
            };
            let runtime = self.runtime.lock().await;
            if !force_refresh {
                if let (Some(index), Some(index_loaded_at)) =
                    (&runtime.index, runtime.index_loaded_at)
                {
                    if can_reuse_loaded_index(
                        index_loaded_at,
                        runtime.index_generation,
                        &self.config,
                        persistent_generations,
                    ) {
                        return Ok(Arc::clone(index));
                    }
                }
            }
        }

        if !force_refresh {
            let cache = self.cache.clone();
            let options_signature = index_options_signature(&self.config.index_options());
            let cached_index = tokio::task::spawn_blocking(move || {
                load_workspace_index(&cache, &options_signature)
            })
            .await
            .map_err(|error| anyhow!("cached index load task failed: {error}"))??;
            if let Some(cached_index) = cached_index {
                if let Some(index_loaded_at) = indexed_at_to_instant(&cached_index.value.indexed_at)
                {
                    if index_loaded_at.elapsed()
                        <= Duration::from_secs(self.config.refresh_window_secs)
                    {
                        let index = Arc::new(cached_index.value);
                        let mut runtime = self.runtime.lock().await;
                        runtime.index = Some(Arc::clone(&index));
                        runtime.index_loaded_at = Some(index_loaded_at);
                        runtime.index_generation = cached_index.generation;
                        return Ok(index);
                    }
                }
            }
        }

        let root = self.root.clone();
        let cache = self.cache.clone();
        let options = self.config.index_options();
        let refreshed: RefreshedWorkspaceIndex =
            tokio::task::spawn_blocking(move || refresh_workspace_index(&root, &cache, &options))
                .await
                .map_err(|error| anyhow!("index refresh task failed: {error}"))??;

        let refreshed_generation = refreshed.generation;
        let refreshed = Arc::new(refreshed.index);
        let mut runtime = self.runtime.lock().await;
        runtime.index = Some(Arc::clone(&refreshed));
        runtime.index_loaded_at = Some(Instant::now());
        runtime.index_generation = refreshed_generation;
        Ok(refreshed)
    }

    pub(crate) async fn cache_status(&self, force_refresh: bool) -> Result<CacheStatus> {
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

    pub(crate) fn search_workspace_hits(
        &self,
        index: &WorkspaceIndex,
        query: &str,
        limit: usize,
    ) -> Vec<SearchHit> {
        search_workspace_cached(
            index,
            &self.cache,
            &self.config.index_options(),
            query,
            limit,
        )
    }

    pub(crate) async fn load_memories(&self) -> Result<MemoryStore> {
        {
            let persistent_generations = self.persistent_generations_snapshot().await;
            let runtime = self.runtime.lock().await;
            if let Some(memories) = &runtime.memories {
                let generation_matches = persistent_generations
                    .map(|generations| generations.memory == runtime.memory_generation)
                    .unwrap_or(true);
                if generation_matches {
                    return Ok(memories.clone());
                }
            }
        }

        let cache = self.cache.clone();
        let cache_path = self.cache.db_path.display().to_string();
        let workspace_id = self.cache.workspace_id.clone();
        let workspace_root = self.root.display().to_string();
        let cached_memories = tokio::task::spawn_blocking(move || load_memory_store(&cache))
            .await
            .map_err(|error| anyhow!("memory load task failed: {error}"))?
            .with_context(|| format!("failed to load memory cache from {cache_path}"))?;
        let (memories, generation) = cached_memories
            .map(|cached| (cached.value, cached.generation))
            .unwrap_or_else(|| {
                (
                    MemoryStore {
                        workspace_id,
                        workspace_root,
                        updated_at: Utc::now().to_rfc3339(),
                        entries: Vec::new(),
                    },
                    0,
                )
            });

        let mut runtime = self.runtime.lock().await;
        runtime.memories = Some(memories.clone());
        runtime.memory_generation = generation;
        Ok(memories)
    }

    async fn save_memories(&self, store: MemoryStore) -> Result<()> {
        let cache = self.cache.clone();
        let store_to_save = store.clone();
        let generation =
            tokio::task::spawn_blocking(move || save_memory_store(&cache, &store_to_save))
                .await
                .map_err(|error| anyhow!("memory save task failed: {error}"))??;

        let mut runtime = self.runtime.lock().await;
        runtime.memories = Some(store);
        runtime.memory_generation = generation;
        Ok(())
    }

    async fn skill_catalog(&self, force_refresh: bool) -> Result<Option<SkillCatalog>> {
        if self.config.skill_roots.is_empty() {
            return Ok(None);
        }

        if !force_refresh {
            let persistent_generations = self.persistent_generations_snapshot().await;
            let runtime = self.runtime.lock().await;
            if let (Some(catalog), Some(loaded_at)) =
                (&runtime.skill_catalog, runtime.skill_catalog_loaded_at)
            {
                if loaded_at.elapsed() <= Duration::from_secs(self.config.skill_cache_ttl_secs) {
                    let generation_matches = persistent_generations
                        .map(|generations| generations.skills == runtime.skills_generation)
                        .unwrap_or(true);
                    if generation_matches {
                        return Ok(Some(catalog.clone()));
                    }
                }
            }
        }

        let _refresh_guard = self.skill_refresh.lock().await;
        {
            let persistent_generations = if force_refresh {
                None
            } else {
                self.persistent_generations_snapshot().await
            };
            let runtime = self.runtime.lock().await;
            if !force_refresh {
                if let (Some(catalog), Some(loaded_at)) =
                    (&runtime.skill_catalog, runtime.skill_catalog_loaded_at)
                {
                    if loaded_at.elapsed() <= Duration::from_secs(self.config.skill_cache_ttl_secs)
                    {
                        let generation_matches = persistent_generations
                            .map(|generations| generations.skills == runtime.skills_generation)
                            .unwrap_or(true);
                        if generation_matches {
                            return Ok(Some(catalog.clone()));
                        }
                    }
                }
            }
        }

        let options = self.skill_index_options();
        let options_signature = skill_index_options_signature(&options);
        if !force_refresh {
            let cache = self.cache.clone();
            let cached_catalog = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>(
                    load_skill_catalog(&cache, &options_signature)
                        .map(Some)
                        .unwrap_or_else(|error| {
                            warn!(
                                "failed to load skill cache from {}: {error}",
                                cache.db_path.display()
                            );
                            None
                        }),
                )
            })
            .await
            .map_err(|error| anyhow!("cached skill catalog load task failed: {error}"))??;
            if let Some(cached_catalog) = cached_catalog.flatten() {
                if let Some(loaded_at) = indexed_at_to_instant(&cached_catalog.value.indexed_at) {
                    if loaded_at.elapsed() <= Duration::from_secs(self.config.skill_cache_ttl_secs)
                    {
                        let catalog = cached_catalog.value;
                        let mut runtime = self.runtime.lock().await;
                        runtime.skill_catalog = Some(catalog.clone());
                        runtime.skill_catalog_loaded_at = Some(loaded_at);
                        runtime.skills_generation = cached_catalog.generation;
                        return Ok(Some(catalog));
                    }
                }
            }
        }

        let options = self.skill_index_options();
        let options_signature = skill_index_options_signature(&options);
        let catalog = tokio::task::spawn_blocking(move || build_skill_catalog(&options))
            .await
            .map_err(|error| anyhow!("skill catalog task failed: {error}"))??;

        let cache = self.cache.clone();
        let catalog_to_save = catalog.clone();
        let generation = tokio::task::spawn_blocking(move || {
            save_skill_catalog(&cache, &options_signature, &catalog_to_save)
        })
        .await
        .map_err(|error| anyhow!("skill catalog save task failed: {error}"))??;

        let mut runtime = self.runtime.lock().await;
        runtime.skill_catalog = Some(catalog.clone());
        runtime.skill_catalog_loaded_at = Some(Instant::now());
        runtime.skills_generation = generation;
        Ok(Some(catalog))
    }

    pub(crate) async fn search_skills(
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

    pub(crate) async fn warmup(&self, force_refresh: bool) -> Result<WarmupStatus> {
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

    pub(crate) async fn remember(
        &self,
        title: String,
        content: String,
        tags: Vec<String>,
        importance: String,
    ) -> Result<MemoryRecord> {
        let _memory_write_guard = self.memory_write.lock().await;
        let mut store = self.load_memories().await?;
        let now = Utc::now().to_rfc3339();
        let normalized_title = normalize_memory_identity_text(&title);
        let normalized_content = normalize_memory_identity_text(&content);
        let importance = normalize_importance(&importance);
        let tags = merge_memory_tags(&[], &tags);

        if let Some(existing) = store.entries.iter_mut().find(|entry| {
            normalize_memory_identity_text(&entry.title) == normalized_title
                && normalize_memory_identity_text(&entry.content) == normalized_content
        }) {
            existing.title = title;
            existing.content = content;
            existing.tags = merge_memory_tags(&existing.tags, &tags);
            existing.importance =
                if importance_rank(&importance) >= importance_rank(&existing.importance) {
                    importance
                } else {
                    normalize_importance(&existing.importance)
                };
            existing.updated_at = now.clone();

            let memory = existing.clone();
            store.updated_at = now;
            self.save_memories(store).await?;
            return Ok(memory);
        }

        let id_source = format!(
            "{}:{}:{}",
            self.cache.workspace_id, normalized_title, normalized_content
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

    pub(crate) async fn recall(
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
            .map(|value| tokenize_query(value))
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

    pub(crate) async fn git_summary(
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

        let _refresh_guard = self.git_refresh.lock().await;
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
                versions: TaskCacheVersions::default(),
            },
        );
        Some(summary)
    }

    pub(crate) async fn build_context_bundle(
        &self,
        task: String,
        limit: usize,
        memory_limit: usize,
        force_refresh: bool,
    ) -> Result<ContextBundle> {
        let cache_key = task_cache_key(&task, limit, memory_limit);
        let persistent_generations = if force_refresh {
            None
        } else {
            self.persistent_generations_snapshot().await
        };
        let cached_bundle = {
            let mut runtime = self.runtime.lock().await;
            prune_timed_cache(
                &mut runtime.bundle_cache,
                self.config.bundle_cache_ttl_secs,
                MAX_BUNDLE_CACHE_ENTRIES,
            );

            if !force_refresh {
                if let Some(entry) = runtime.bundle_cache.get(&cache_key) {
                    if can_reuse_task_cache(entry, &runtime, &self.config, persistent_generations) {
                        Some(entry.value.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
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
        let search_hits = self.search_workspace_hits(&index, &task, limit);
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
        let versions = current_task_cache_versions(&runtime);
        runtime.bundle_cache.insert(
            cache_key,
            TimedCacheEntry {
                value: bundle.clone(),
                loaded_at: Instant::now(),
                versions,
            },
        );
        Ok(bundle)
    }

    pub(crate) async fn decompose_task(
        &self,
        task: String,
        limit: usize,
        memory_limit: usize,
        force_refresh: bool,
    ) -> Result<TaskDecomposition> {
        let cache_key = task_cache_key(&task, limit, memory_limit);
        let persistent_generations = if force_refresh {
            None
        } else {
            self.persistent_generations_snapshot().await
        };
        {
            let mut runtime = self.runtime.lock().await;
            prune_timed_cache(
                &mut runtime.decomposition_cache,
                self.config.bundle_cache_ttl_secs,
                MAX_DECOMPOSITION_CACHE_ENTRIES,
            );

            if !force_refresh {
                if let Some(entry) = runtime.decomposition_cache.get(&cache_key) {
                    if can_reuse_task_cache(entry, &runtime, &self.config, persistent_generations) {
                        return Ok(entry.value.clone());
                    }
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

    pub(crate) async fn orchestrate_task(
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
        let versions = current_task_cache_versions(&runtime);
        runtime.decomposition_cache.insert(
            cache_key,
            TimedCacheEntry {
                value: decomposition,
                loaded_at: Instant::now(),
                versions,
            },
        );
    }
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

    let mut score = importance_score(&entry.importance);
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

fn normalize_memory_identity_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase()
}

fn normalize_importance(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "critical" | "highest" | "urgent" | "high" => "high".to_string(),
        "low" | "minor" => "low".to_string(),
        "normal" | "medium" | "default" | "standard" => "normal".to_string(),
        _ => "normal".to_string(),
    }
}

fn importance_rank(value: &str) -> u8 {
    match normalize_importance(value).as_str() {
        "high" => 2,
        "normal" => 1,
        _ => 0,
    }
}

fn importance_score(value: &str) -> f64 {
    match normalize_importance(value).as_str() {
        "high" => 4.0,
        "normal" => 1.5,
        _ => 0.0,
    }
}

fn merge_memory_tags(existing: &[String], incoming: &[String]) -> Vec<String> {
    let mut tags = existing
        .iter()
        .chain(incoming.iter())
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    tags
}

fn task_cache_key(task: &str, limit: usize, memory_limit: usize) -> String {
    blake3::hash(format!("{task}::{limit}::{memory_limit}").as_bytes())
        .to_hex()
        .to_string()
}

pub(crate) fn git_summary_cache_key(limit_commits: usize, include_diffstat: bool) -> String {
    format!("{limit_commits}:{include_diffstat}")
}

pub(crate) fn can_reuse_loaded_index(
    index_loaded_at: Instant,
    index_generation: u64,
    config: &ServerConfig,
    persistent_generations: Option<PersistentCacheGenerations>,
) -> bool {
    if index_loaded_at.elapsed() > Duration::from_secs(config.refresh_window_secs) {
        return false;
    }

    persistent_generations
        .map(|generations| generations.index == index_generation)
        .unwrap_or(true)
}

fn current_task_cache_versions(runtime: &RuntimeState) -> TaskCacheVersions {
    TaskCacheVersions {
        index: runtime.index_generation,
        memory: runtime.memory_generation,
        skills: runtime.skills_generation,
    }
}

fn effective_task_cache_versions(
    runtime: &RuntimeState,
    config: &ServerConfig,
    persistent_generations: Option<PersistentCacheGenerations>,
) -> TaskCacheVersions {
    let runtime_versions = current_task_cache_versions(runtime);
    let mut versions = persistent_generations
        .map(|generations| TaskCacheVersions {
            index: generations.index,
            memory: generations.memory,
            skills: generations.skills,
        })
        .unwrap_or(runtime_versions);
    if config.skill_roots.is_empty() {
        versions.skills = 0;
    }
    versions
}

pub(crate) fn can_reuse_task_cache<T>(
    entry: &TimedCacheEntry<T>,
    runtime: &RuntimeState,
    config: &ServerConfig,
    persistent_generations: Option<PersistentCacheGenerations>,
) -> bool {
    let index_fresh = runtime
        .index_loaded_at
        .map(|loaded_at| loaded_at.elapsed() <= Duration::from_secs(config.refresh_window_secs))
        .unwrap_or(false);
    if !index_fresh {
        return false;
    }

    let current_versions = effective_task_cache_versions(runtime, config, persistent_generations);
    if entry.versions.index != current_versions.index
        || entry.versions.memory != current_versions.memory
    {
        return false;
    }

    if config.skill_roots.is_empty() {
        return entry.versions.skills == 0 || entry.versions.skills == current_versions.skills;
    }

    let skills_fresh = runtime
        .skill_catalog_loaded_at
        .map(|loaded_at| loaded_at.elapsed() <= Duration::from_secs(config.skill_cache_ttl_secs))
        .unwrap_or(false);
    skills_fresh && entry.versions.skills == current_versions.skills
}

fn indexed_at_to_instant(indexed_at: &str) -> Option<Instant> {
    let parsed = DateTime::parse_from_rfc3339(indexed_at)
        .ok()?
        .with_timezone(&Utc);
    let age = Utc::now().signed_duration_since(parsed);
    if age <= chrono::Duration::zero() {
        return Some(Instant::now());
    }

    Instant::now().checked_sub(age.to_std().ok()?)
}

pub(crate) fn prune_timed_cache<T>(
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

#[cfg(test)]
mod tests;
