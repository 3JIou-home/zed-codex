use std::path::PathBuf;

use crate::indexer::IndexOptions;

const DEFAULT_SKILL_ROOT_CANDIDATES: &[&str] = &[r"D:\downloads\agency-agents"];

#[derive(Debug, Clone)]
pub(crate) struct ServerConfig {
    pub(crate) cache_dir_override: Option<PathBuf>,
    pub(crate) max_file_bytes: usize,
    pub(crate) max_indexed_files: usize,
    pub(crate) ignore_globs: Vec<String>,
    pub(crate) enable_git_tools: bool,
    pub(crate) refresh_window_secs: u64,
    pub(crate) git_cache_ttl_secs: u64,
    pub(crate) bundle_cache_ttl_secs: u64,
    pub(crate) skill_cache_ttl_secs: u64,
    pub(crate) prewarm_on_start: bool,
    pub(crate) execution_mode: String,
    pub(crate) prefer_full_access: bool,
    pub(crate) max_parallel_workstreams: usize,
    pub(crate) skill_roots: Vec<PathBuf>,
    pub(crate) skill_file_globs: Vec<String>,
    pub(crate) max_skill_bytes: usize,
    pub(crate) max_skills_per_query: usize,
}

impl ServerConfig {
    pub(crate) fn from_env() -> Self {
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

    pub(crate) fn index_options(&self) -> IndexOptions {
        IndexOptions {
            max_file_bytes: self.max_file_bytes,
            max_indexed_files: self.max_indexed_files,
            ignore_globs: self.ignore_globs.clone(),
        }
    }
}

pub(crate) fn configured_skill_roots(raw_skill_roots: Option<String>) -> Vec<PathBuf> {
    match raw_skill_roots {
        Some(value) => serde_json::from_str::<Vec<String>>(&value)
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        None => existing_skill_roots(default_skill_root_candidates()),
    }
}

pub(crate) fn default_skill_root_candidates() -> Vec<PathBuf> {
    DEFAULT_SKILL_ROOT_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .collect()
}

pub(crate) fn existing_skill_roots(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|path| path.is_dir()).collect()
}

pub(crate) fn normalize_execution_mode(value: Option<String>) -> String {
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
