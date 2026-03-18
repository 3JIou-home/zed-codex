use super::{importance_score, normalize_importance, score_memory, AppState};
use crate::{config::ServerConfig, model::MemoryRecord};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn unique_temp_dir() -> PathBuf {
    let unique = format!(
        "codex-companion-state-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time must be after unix epoch")
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique);
    fs::create_dir_all(&path).expect("temp dir must be created");
    path
}

fn test_config(cache_dir: PathBuf) -> ServerConfig {
    ServerConfig {
        cache_dir_override: Some(cache_dir),
        max_file_bytes: 262_144,
        max_indexed_files: 1_500,
        ignore_globs: Vec::new(),
        enable_git_tools: false,
        refresh_window_secs: 120,
        git_cache_ttl_secs: 15,
        bundle_cache_ttl_secs: 180,
        skill_cache_ttl_secs: 300,
        prewarm_on_start: false,
        execution_mode: "balanced".to_string(),
        prefer_full_access: false,
        max_parallel_workstreams: 4,
        skill_roots: Vec::new(),
        skill_file_globs: vec!["**/*.md".to_string()],
        max_skill_bytes: 131_072,
        max_skills_per_query: 4,
    }
}

#[test]
fn importance_score_normalizes_aliases() {
    assert_eq!(normalize_importance("medium"), "normal");
    assert_eq!(normalize_importance("urgent"), "high");
    assert_eq!(importance_score("minor"), 0.0);
}

#[test]
fn score_memory_prefers_high_importance_when_relevance_ties() {
    let low = MemoryRecord {
        id: "1".to_string(),
        title: "Cache policy".to_string(),
        content: "Invalidate on writes.".to_string(),
        tags: vec!["cache".to_string()],
        importance: "low".to_string(),
        created_at: "2026-03-17T00:00:00Z".to_string(),
        updated_at: "2026-03-17T00:00:00Z".to_string(),
    };
    let high = MemoryRecord {
        importance: "high".to_string(),
        ..low.clone()
    };

    let query_tokens = crate::text::tokenize_query("cache policy");
    assert!(
        score_memory(&high, Some("cache policy"), &query_tokens, &[])
            > score_memory(&low, Some("cache policy"), &query_tokens, &[])
    );
}

#[tokio::test]
async fn remember_deduplicates_equivalent_memories() {
    let root = unique_temp_dir();
    let cache_dir = unique_temp_dir();
    let state = AppState::new(root, test_config(cache_dir)).expect("app state must initialize");

    let first = state
        .remember(
            "Cache Policy".to_string(),
            "Invalidate on writes.".to_string(),
            vec!["cache".to_string()],
            "normal".to_string(),
        )
        .await
        .expect("first memory should be saved");
    let second = state
        .remember(
            "cache   policy".to_string(),
            "Invalidate on writes.".to_string(),
            vec!["architecture".to_string()],
            "high".to_string(),
        )
        .await
        .expect("duplicate memory should be upserted");

    let memories = state.load_memories().await.expect("memories should load");
    assert_eq!(memories.entries.len(), 1);
    assert_eq!(first.id, second.id);
    assert_eq!(memories.entries[0].importance, "high");
    assert!(memories.entries[0].tags.contains(&"cache".to_string()));
    assert!(memories.entries[0]
        .tags
        .contains(&"architecture".to_string()));
}
