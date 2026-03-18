use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::resolve_root;
use crate::cache::PersistentCacheGenerations;
use crate::model::{
    ContextBundle, MemoryRecord, ScanMetrics, SearchHit, SkillMatch, WorkspaceOverview,
};
use crate::{
    config::{
        configured_skill_roots, default_skill_root_candidates, existing_skill_roots,
        normalize_execution_mode, ServerConfig,
    },
    planning::{build_task_decomposition, build_task_orchestration},
    state::{
        can_reuse_loaded_index, can_reuse_task_cache, git_summary_cache_key, prune_timed_cache,
        AppState, RuntimeState, TaskCacheVersions, TimedCacheEntry,
    },
};

fn unique_temp_dir() -> PathBuf {
    let unique = format!(
        "codex-companion-main-test-{}-{}",
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
fn default_skill_root_candidates_include_agency_agents() {
    assert_eq!(
        default_skill_root_candidates(),
        vec![PathBuf::from(r"D:\downloads\agency-agents")]
    );
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
            versions: TaskCacheVersions::default(),
        },
    );
    cache.insert(
        "oldest".to_string(),
        TimedCacheEntry {
            value: "oldest".to_string(),
            loaded_at: Instant::now() - Duration::from_secs(3),
            versions: TaskCacheVersions::default(),
        },
    );
    cache.insert(
        "newer".to_string(),
        TimedCacheEntry {
            value: "newer".to_string(),
            loaded_at: Instant::now() - Duration::from_secs(2),
            versions: TaskCacheVersions::default(),
        },
    );
    cache.insert(
        "newest".to_string(),
        TimedCacheEntry {
            value: "newest".to_string(),
            loaded_at: Instant::now() - Duration::from_secs(1),
            versions: TaskCacheVersions::default(),
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
        index_generation: 1,
        ..RuntimeState::default()
    };
    let entry = TimedCacheEntry {
        value: (),
        loaded_at: Instant::now(),
        versions: TaskCacheVersions {
            index: 1,
            memory: 0,
            skills: 0,
        },
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

    assert!(!can_reuse_task_cache(&entry, &runtime, &config, None));
}

#[test]
fn loaded_index_requires_matching_persistent_generation() {
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
        execution_mode: "balanced".to_string(),
        prefer_full_access: true,
        max_parallel_workstreams: 4,
        skill_roots: Vec::new(),
        skill_file_globs: vec!["**/*.md".to_string()],
        max_skill_bytes: 131_072,
        max_skills_per_query: 4,
    };

    assert!(!can_reuse_loaded_index(
        Instant::now(),
        1,
        &config,
        Some(PersistentCacheGenerations {
            index: 2,
            memory: 0,
            skills: 0,
        })
    ));
}

#[test]
fn task_cache_requires_fresh_skill_catalog_when_skills_are_enabled() {
    let runtime = RuntimeState {
        index_loaded_at: Some(Instant::now()),
        index_generation: 1,
        skill_catalog_loaded_at: Some(Instant::now() - Duration::from_secs(10)),
        skills_generation: 1,
        ..RuntimeState::default()
    };
    let entry = TimedCacheEntry {
        value: (),
        loaded_at: Instant::now(),
        versions: TaskCacheVersions {
            index: 1,
            memory: 0,
            skills: 1,
        },
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

    assert!(!can_reuse_task_cache(&entry, &runtime, &config, None));
}

#[test]
fn task_cache_requires_matching_generations() {
    let runtime = RuntimeState {
        index_loaded_at: Some(Instant::now()),
        index_generation: 2,
        memory_generation: 1,
        ..RuntimeState::default()
    };
    let entry = TimedCacheEntry {
        value: (),
        loaded_at: Instant::now(),
        versions: TaskCacheVersions {
            index: 1,
            memory: 1,
            skills: 0,
        },
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
        execution_mode: "balanced".to_string(),
        prefer_full_access: true,
        max_parallel_workstreams: 4,
        skill_roots: Vec::new(),
        skill_file_globs: vec!["**/*.md".to_string()],
        max_skill_bytes: 131_072,
        max_skills_per_query: 4,
    };

    assert!(!can_reuse_task_cache(&entry, &runtime, &config, None));
}

#[test]
fn task_cache_requires_matching_persistent_generations() {
    let runtime = RuntimeState {
        index_loaded_at: Some(Instant::now()),
        index_generation: 1,
        memory_generation: 2,
        skill_catalog_loaded_at: Some(Instant::now()),
        skills_generation: 3,
        ..RuntimeState::default()
    };
    let entry = TimedCacheEntry {
        value: (),
        loaded_at: Instant::now(),
        versions: TaskCacheVersions {
            index: 1,
            memory: 2,
            skills: 3,
        },
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
        execution_mode: "balanced".to_string(),
        prefer_full_access: true,
        max_parallel_workstreams: 4,
        skill_roots: vec![PathBuf::from(r"D:\downloads\agency-agents")],
        skill_file_globs: vec!["**/*.md".to_string()],
        max_skill_bytes: 131_072,
        max_skills_per_query: 4,
    };
    let persistent_generations = PersistentCacheGenerations {
        index: 2,
        memory: 2,
        skills: 3,
    };

    assert!(!can_reuse_task_cache(
        &entry,
        &runtime,
        &config,
        Some(persistent_generations)
    ));
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
fn decomposition_keeps_boundary_files_serial_even_when_not_primary() {
    let bundle = ContextBundle {
        task: "update docs and runtime wiring".to_string(),
        overview: WorkspaceOverview {
            workspace_root: "/tmp/demo".to_string(),
            indexed_at: "2026-03-17T00:00:00Z".to_string(),
            total_indexed_files: 3,
            total_indexed_bytes: 100,
            major_languages: Vec::new(),
            top_directories: Vec::new(),
            key_files: vec!["README.md".to_string(), "src/lib.rs".to_string()],
            highlights: Vec::new(),
            scan_metrics: ScanMetrics::default(),
        },
        search_hits: vec![
            SearchHit {
                path: "docs/usage.md".to_string(),
                language: "markdown".to_string(),
                score: 24.0,
                line: Some(3),
                snippet: "# Usage".to_string(),
                summary: "user docs".to_string(),
                matching_symbols: vec!["Usage".to_string()],
            },
            SearchHit {
                path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                score: 20.0,
                line: Some(12),
                snippet: "pub fn activate() {}".to_string(),
                summary: "runtime entrypoint".to_string(),
                matching_symbols: vec!["activate".to_string()],
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
    let lib_stream = decomposition
        .workstreams
        .iter()
        .find(|stream| {
            stream
                .recommended_files
                .iter()
                .any(|path| path == "src/lib.rs")
        })
        .expect("lib.rs workstream should exist");

    assert!(!lib_stream.can_run_in_parallel);
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

#[tokio::test]
async fn load_memories_recovers_when_cache_is_corrupted() {
    let root = unique_temp_dir();
    let cache_dir = unique_temp_dir();
    let config = ServerConfig {
        cache_dir_override: Some(cache_dir),
        max_file_bytes: 262_144,
        max_indexed_files: 1_500,
        ignore_globs: Vec::new(),
        enable_git_tools: true,
        refresh_window_secs: 120,
        git_cache_ttl_secs: 15,
        bundle_cache_ttl_secs: 180,
        skill_cache_ttl_secs: 300,
        prewarm_on_start: false,
        execution_mode: "balanced".to_string(),
        prefer_full_access: true,
        max_parallel_workstreams: 4,
        skill_roots: Vec::new(),
        skill_file_globs: vec!["**/*.md".to_string()],
        max_skill_bytes: 131_072,
        max_skills_per_query: 4,
    };
    let state = AppState::new(root, config).expect("app state must initialize");

    fs::write(&state.cache.db_path, b"not a sqlite database")
        .expect("corrupted cache file must be written");

    let memories = state
        .load_memories()
        .await
        .expect("corrupted memory cache should be recreated");
    assert!(memories.entries.is_empty());
}

#[test]
fn resolve_root_rejects_file_paths() {
    let temp_dir = unique_temp_dir();
    let temp_file = temp_dir.join("workspace.txt");
    fs::write(&temp_file, "not a directory").expect("temp file must be created");

    let error = resolve_root(Some(temp_file)).expect_err("file path must be rejected");
    assert!(error.to_string().contains("not a directory"));
}
