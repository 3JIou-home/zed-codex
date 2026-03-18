use super::{build_workspace_overview, extract_symbols, search_workspace};
use crate::model::{FileRecord, ScanMetrics, WorkspaceIndex};
use crate::text::tokenize_query;

#[test]
fn tokenize_splits_code_like_input() {
    let tokens = tokenize_query("review login-flow.ts for cache invalidation");
    assert!(!tokens.is_empty());
    assert!(tokens.contains(&"cache".to_string()));
    assert!(tokens.contains(&"login-flow".to_string()));
    assert!(tokens.contains(&"login".to_string()));
    assert!(tokens.contains(&"flow".to_string()));
}

#[test]
fn extract_symbols_finds_common_patterns() {
    let text = r#"
            pub async fn refresh_cache() {}
            struct WorkspaceState {}
            ## Heading
        "#;

    let symbols = extract_symbols(text);
    assert!(symbols.iter().any(|symbol| symbol == "refresh_cache"));
    assert!(symbols.iter().any(|symbol| symbol == "WorkspaceState"));
    assert!(symbols.len() >= 2);
}

#[test]
fn extract_symbols_supports_common_non_rust_patterns() {
    let text = r#"
            export const buildContext = async () => {};
            suspend fun refreshIndex() {}
            public static void Main(String[] args) {}
        "#;

    let symbols = extract_symbols(text);
    assert!(symbols.iter().any(|symbol| symbol == "buildContext"));
    assert!(symbols.iter().any(|symbol| symbol == "refreshIndex"));
    assert!(symbols.iter().any(|symbol| symbol == "Main"));
}

#[test]
fn search_workspace_prefers_matching_paths() {
    let index = WorkspaceIndex {
        format_version: 2,
        workspace_id: "workspace".to_string(),
        workspace_root: "/tmp/demo".to_string(),
        indexed_at: "2026-03-17T00:00:00Z".to_string(),
        total_scanned_files: 2,
        total_indexed_bytes: 100,
        scan_metrics: ScanMetrics::default(),
        files: vec![
            FileRecord {
                path: "src/cache.rs".to_string(),
                language: "rust".to_string(),
                size: 50,
                modified_unix_nanos: 0,
                hash: "a".to_string(),
                preview: "refresh cache and store index".to_string(),
                symbols: vec!["refresh_cache".to_string()],
                indexed_text: "fn refresh_cache() {}".to_string(),
                line_count: 1,
            },
            FileRecord {
                path: "src/main.rs".to_string(),
                language: "rust".to_string(),
                size: 50,
                modified_unix_nanos: 0,
                hash: "b".to_string(),
                preview: "application entrypoint".to_string(),
                symbols: vec!["main".to_string()],
                indexed_text: "fn main() {}".to_string(),
                line_count: 1,
            },
        ],
    };

    let hits = search_workspace(&index, "cache", 10);
    assert_eq!(
        hits.first().map(|hit| hit.path.as_str()),
        Some("src/cache.rs")
    );
}

#[test]
fn search_workspace_prefers_exact_symbol_and_better_snippet() {
    let index = WorkspaceIndex {
        format_version: 2,
        workspace_id: "workspace".to_string(),
        workspace_root: "/tmp/demo".to_string(),
        indexed_at: "2026-03-17T00:00:00Z".to_string(),
        total_scanned_files: 2,
        total_indexed_bytes: 100,
        scan_metrics: ScanMetrics::default(),
        files: vec![
            FileRecord {
                path: "src/helpers.rs".to_string(),
                language: "rust".to_string(),
                size: 50,
                modified_unix_nanos: 0,
                hash: "a".to_string(),
                preview: "utility helpers".to_string(),
                symbols: vec!["refresh_cache".to_string()],
                indexed_text: "fn noop() {}\nfn refresh_cache() { invalidate cache entries; }\n"
                    .to_string(),
                line_count: 2,
            },
            FileRecord {
                path: "src/notes.rs".to_string(),
                language: "rust".to_string(),
                size: 50,
                modified_unix_nanos: 0,
                hash: "b".to_string(),
                preview: "mentions refresh and cache separately".to_string(),
                symbols: vec!["notes".to_string()],
                indexed_text: "refresh\ncache\n".to_string(),
                line_count: 2,
            },
        ],
    };

    let hits = search_workspace(&index, "refresh cache", 10);
    assert_eq!(
        hits.first().map(|hit| hit.path.as_str()),
        Some("src/helpers.rs")
    );
    assert_eq!(hits.first().and_then(|hit| hit.line), Some(2));
}

#[test]
fn workspace_overview_sorts_languages_by_file_count() {
    let index = WorkspaceIndex {
        format_version: 2,
        workspace_id: "workspace".to_string(),
        workspace_root: "/tmp/demo".to_string(),
        indexed_at: "2026-03-17T00:00:00Z".to_string(),
        total_scanned_files: 4,
        total_indexed_bytes: 100,
        scan_metrics: ScanMetrics::default(),
        files: vec![
            FileRecord {
                path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                size: 10,
                modified_unix_nanos: 0,
                hash: "1".to_string(),
                preview: "rust".to_string(),
                symbols: Vec::new(),
                indexed_text: String::new(),
                line_count: 1,
            },
            FileRecord {
                path: "src/main.rs".to_string(),
                language: "rust".to_string(),
                size: 10,
                modified_unix_nanos: 0,
                hash: "2".to_string(),
                preview: "rust".to_string(),
                symbols: Vec::new(),
                indexed_text: String::new(),
                line_count: 1,
            },
            FileRecord {
                path: "README.md".to_string(),
                language: "markdown".to_string(),
                size: 10,
                modified_unix_nanos: 0,
                hash: "3".to_string(),
                preview: "markdown".to_string(),
                symbols: Vec::new(),
                indexed_text: String::new(),
                line_count: 1,
            },
            FileRecord {
                path: "docs/guide.md".to_string(),
                language: "markdown".to_string(),
                size: 10,
                modified_unix_nanos: 0,
                hash: "4".to_string(),
                preview: "markdown".to_string(),
                symbols: Vec::new(),
                indexed_text: String::new(),
                line_count: 1,
            },
        ],
    };

    let overview = build_workspace_overview(&index);
    assert_eq!(
        overview
            .major_languages
            .iter()
            .map(|language| (language.language.as_str(), language.files))
            .collect::<Vec<_>>(),
        vec![("markdown", 2), ("rust", 2)]
    );
}
