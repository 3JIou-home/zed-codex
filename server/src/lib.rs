mod cache;
mod codex_bridge;
mod config;
mod formatting;
mod git_tools;
mod indexer;
mod model;
mod planning;
mod skills;
mod state;
mod text;

pub mod acp;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tracing_subscriber::EnvFilter;

pub use config::ServerConfig;
pub use formatting::{
    format_cache_status, format_context_bundle, format_memory_results, format_skill_results,
    format_task_decomposition, format_task_orchestration, format_warmup_status,
};
pub use indexer::build_workspace_overview;
pub use model::{
    CacheStatus, ContextBundle, GitSummary, MemoryRecord, MemorySearchResults, SearchResults,
    SkillSearchResults, TaskDecomposition, TaskOrchestration, WarmupStatus, WorkspaceOverview,
};
pub use state::AppState;

pub fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf> {
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

pub fn normalize_tags(tags: Vec<String>, root: &Path) -> Vec<String> {
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

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .try_init();
}

#[cfg(test)]
mod tests;
