use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    load_memory_store, load_persistent_generations, load_skill_catalog, load_workspace_index,
    prepare_workspace_query_terms, resolve_workspace_cache, save_memory_store, save_skill_catalog,
    save_workspace_index, search_workspace_candidates, workspace_id,
};
use crate::model::{
    MemoryRecord, MemoryStore, ScanMetrics, SkillCatalog, SkillRecord, WorkspaceIndex,
};

fn unique_temp_dir() -> PathBuf {
    let unique = format!(
        "codex-companion-cache-test-{}-{}",
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

#[cfg(windows)]
#[test]
fn workspace_id_normalizes_case_on_windows() {
    assert_eq!(
        workspace_id(Path::new(r"C:\Users\Demo\Repo")),
        workspace_id(Path::new(r"c:\users\demo\repo"))
    );
}

#[cfg(not(windows))]
#[test]
fn workspace_id_preserves_case_on_case_sensitive_platforms() {
    assert_ne!(
        workspace_id(Path::new("/tmp/Repo")),
        workspace_id(Path::new("/tmp/repo"))
    );
}

#[test]
fn prepare_workspace_query_terms_caps_and_deduplicates() {
    let query_terms = (0..40)
        .map(|index| format!("term-{index}"))
        .chain(["term-1".to_string(), " ".to_string(), "x".to_string()])
        .collect::<Vec<_>>();

    let prepared = prepare_workspace_query_terms(&query_terms);

    assert_eq!(prepared.len(), 24);
    assert_eq!(prepared.first().map(String::as_str), Some("term-0"));
    assert_eq!(prepared.get(1).map(String::as_str), Some("term-1"));
    assert!(!prepared.iter().any(|term| term.trim().is_empty()));
}

#[test]
fn sqlite_workspace_cache_roundtrips_index_and_generations() {
    let root = unique_temp_dir();
    let cache_dir = unique_temp_dir();
    let cache = resolve_workspace_cache(&root, Some(&cache_dir)).expect("cache must resolve");
    let options_signature = "index-options-v1";
    let index = WorkspaceIndex {
        format_version: 2,
        workspace_id: cache.workspace_id.clone(),
        workspace_root: root.display().to_string(),
        indexed_at: "2026-03-18T00:00:00Z".to_string(),
        total_scanned_files: 1,
        total_indexed_bytes: 12,
        files: vec![crate::model::FileRecord {
            path: "src/main.rs".to_string(),
            language: "rust".to_string(),
            size: 12,
            modified_unix_nanos: 42,
            hash: "abc".to_string(),
            preview: "fn main() {}".to_string(),
            symbols: vec!["main".to_string()],
            indexed_text: "fn main() {}".to_string(),
            line_count: 1,
        }],
        scan_metrics: ScanMetrics {
            reused_files: 0,
            reindexed_files: 1,
            skipped_files: 0,
        },
    };

    let generation =
        save_workspace_index(&cache, options_signature, &index).expect("index must save");
    assert_eq!(generation, 1);

    let loaded = load_workspace_index(&cache, options_signature)
        .expect("index must load")
        .expect("index must exist");
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.value.files.len(), 1);
    assert_eq!(loaded.value.files[0].modified_unix_nanos, 42);
    assert!(load_workspace_index(&cache, "different-options")
        .expect("mismatched signature load must succeed")
        .is_none());

    let generations = load_persistent_generations(&cache).expect("generations must be readable");
    assert_eq!(generations.index, 1);
    assert_eq!(generations.memory, 0);
    assert_eq!(generations.skills, 0);

    let search_hits = search_workspace_candidates(&cache, options_signature, &["main".into()], 5)
        .expect("workspace search candidates must load");
    assert_eq!(
        search_hits.first().map(|hit| hit.path.as_str()),
        Some("src/main.rs")
    );
}

#[test]
fn sqlite_workspace_cache_roundtrips_memories() {
    let root = unique_temp_dir();
    let cache_dir = unique_temp_dir();
    let cache = resolve_workspace_cache(&root, Some(&cache_dir)).expect("cache must resolve");
    let store = MemoryStore {
        workspace_id: cache.workspace_id.clone(),
        workspace_root: root.display().to_string(),
        updated_at: "2026-03-18T00:00:00Z".to_string(),
        entries: vec![MemoryRecord {
            id: "memo-1".to_string(),
            title: "Decision".to_string(),
            content: "Use SQLite".to_string(),
            tags: vec!["cache".to_string()],
            importance: "high".to_string(),
            created_at: "2026-03-18T00:00:00Z".to_string(),
            updated_at: "2026-03-18T00:00:00Z".to_string(),
        }],
    };

    let generation = save_memory_store(&cache, &store).expect("memory store must save");
    assert_eq!(generation, 1);

    let loaded = load_memory_store(&cache)
        .expect("memory store must load")
        .expect("memory store must exist");
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.value.entries.len(), 1);
    assert_eq!(loaded.value.entries[0].title, "Decision");
    assert_eq!(loaded.value.entries[0].tags, vec!["cache".to_string()]);
}

#[test]
fn sqlite_workspace_cache_roundtrips_skills_and_generations() {
    let root = unique_temp_dir();
    let cache_dir = unique_temp_dir();
    let cache = resolve_workspace_cache(&root, Some(&cache_dir)).expect("cache must resolve");
    let options_signature = "skill-options-v1";
    let catalog = SkillCatalog {
        roots: vec![r"D:\downloads\agency-agents".to_string()],
        indexed_at: "2026-03-18T00:00:00Z".to_string(),
        total_skills: 1,
        skills: vec![SkillRecord {
            id: "skill-1".to_string(),
            name: "Software Architect".to_string(),
            description: "Design durable systems".to_string(),
            path: "engineering/software-architect.md".to_string(),
            source_root: r"D:\downloads\agency-agents".to_string(),
            category: "engineering".to_string(),
            emoji: Some("architect".to_string()),
            vibe: Some("trade-off conscious".to_string()),
            tags: vec!["architecture".to_string(), "cache".to_string()],
            preview: "Use SQLite with WAL.".to_string(),
            content: "# Software Architect".to_string(),
        }],
    };

    let generation =
        save_skill_catalog(&cache, options_signature, &catalog).expect("skill catalog must save");
    assert_eq!(generation, 1);

    let loaded = load_skill_catalog(&cache, options_signature)
        .expect("skill catalog must load")
        .expect("skill catalog must exist");
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.value.total_skills, 1);
    assert_eq!(loaded.value.skills[0].name, "Software Architect");
    assert!(load_skill_catalog(&cache, "different-skill-options")
        .expect("mismatched skill signature load must succeed")
        .is_none());

    let generations = load_persistent_generations(&cache).expect("generations must be readable");
    assert_eq!(generations.index, 0);
    assert_eq!(generations.memory, 0);
    assert_eq!(generations.skills, 1);
}

#[test]
fn load_memory_store_recovers_from_corrupted_database() {
    let root = unique_temp_dir();
    let cache_dir = unique_temp_dir();
    let cache = resolve_workspace_cache(&root, Some(&cache_dir)).expect("cache must resolve");

    fs::write(&cache.db_path, b"not a sqlite database")
        .expect("corrupted cache file must be written");

    let loaded = load_memory_store(&cache).expect("corrupted cache should be reset");
    assert!(loaded.is_none());

    let generations =
        load_persistent_generations(&cache).expect("generations should be readable after reset");
    assert_eq!(generations.index, 0);
    assert_eq!(generations.memory, 0);
    assert_eq!(generations.skills, 0);
}

#[test]
fn save_memory_store_recovers_from_corrupted_database() {
    let root = unique_temp_dir();
    let cache_dir = unique_temp_dir();
    let cache = resolve_workspace_cache(&root, Some(&cache_dir)).expect("cache must resolve");
    let store = MemoryStore {
        workspace_id: cache.workspace_id.clone(),
        workspace_root: root.display().to_string(),
        updated_at: "2026-03-18T00:00:00Z".to_string(),
        entries: vec![MemoryRecord {
            id: "memo-2".to_string(),
            title: "Recovered".to_string(),
            content: "Store after reset".to_string(),
            tags: vec!["recovery".to_string()],
            importance: "normal".to_string(),
            created_at: "2026-03-18T00:00:00Z".to_string(),
            updated_at: "2026-03-18T00:00:00Z".to_string(),
        }],
    };

    fs::write(&cache.db_path, b"not a sqlite database")
        .expect("corrupted cache file must be written");

    let generation = save_memory_store(&cache, &store).expect("save should recreate cache");
    assert_eq!(generation, 1);

    let loaded = load_memory_store(&cache)
        .expect("memory store must load after recreation")
        .expect("memory store must exist after recreation");
    assert_eq!(loaded.value.entries.len(), 1);
    assert_eq!(loaded.value.entries[0].title, "Recovered");
}
