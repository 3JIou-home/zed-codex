use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::Path,
    sync::OnceLock,
};

use anyhow::{Context, Result};
use chrono::Utc;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use tracing::warn;

use crate::{
    cache::{
        load_workspace_index, save_workspace_index, search_workspace_candidates, WorkspaceCache,
    },
    model::{
        DirectoryCount, FileRecord, LanguageCount, ScanMetrics, SearchHit, WorkspaceIndex,
        WorkspaceOverview,
    },
    text::tokenize_query,
};

const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct RefreshedWorkspaceIndex {
    pub index: WorkspaceIndex,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub max_file_bytes: usize,
    pub max_indexed_files: usize,
    pub ignore_globs: Vec<String>,
}

pub fn index_options_signature(options: &IndexOptions) -> String {
    blake3::hash(
        format!(
            "{}:{}:{}",
            options.max_file_bytes,
            options.max_indexed_files,
            options.ignore_globs.join("\u{0}")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

pub fn refresh_workspace_index(
    workspace_root: &Path,
    cache: &WorkspaceCache,
    options: &IndexOptions,
) -> Result<RefreshedWorkspaceIndex> {
    let options_signature = index_options_signature(options);
    let previous_index = match load_workspace_index(cache, &options_signature) {
        Ok(value) => value.map(|value| value.value),
        Err(error) => {
            warn!(
                "failed to load cached workspace index from {}: {error}",
                cache.db_path.display()
            );
            None
        }
    }
    .filter(|index| index.format_version == FORMAT_VERSION);
    let mut previous_by_path = HashMap::new();
    if let Some(index) = previous_index {
        for file in index.files {
            previous_by_path.insert(file.path.clone(), file);
        }
    }

    let ignore_set = build_ignore_set(&options.ignore_globs)?;
    let mut files = Vec::new();
    let mut total_scanned_files = 0usize;
    let mut total_indexed_bytes = 0u64;
    let mut scan_metrics = ScanMetrics::default();

    let mut walker = WalkBuilder::new(workspace_root);
    walker.standard_filters(true);
    walker.hidden(false);
    walker.follow_links(false);

    for entry in walker.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                scan_metrics.skipped_files += 1;
                continue;
            }
        };

        if !entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false)
        {
            continue;
        }

        total_scanned_files += 1;

        let absolute_path = entry.path();
        let relative_path = normalize_relative_path(workspace_root, absolute_path)?;
        if should_ignore(&relative_path, &ignore_set) {
            scan_metrics.skipped_files += 1;
            continue;
        }

        if files.len() >= options.max_indexed_files {
            scan_metrics.skipped_files += 1;
            continue;
        }

        let metadata = fs::metadata(absolute_path)
            .with_context(|| format!("failed to read metadata for {}", absolute_path.display()))?;
        let modified_unix_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or_default();

        if metadata.len() as usize > options.max_file_bytes || metadata.len() == 0 {
            scan_metrics.skipped_files += 1;
            continue;
        }

        if let Some(previous) = previous_by_path.get(&relative_path) {
            if previous.size == metadata.len()
                && previous.modified_unix_nanos == modified_unix_nanos
            {
                total_indexed_bytes += previous.size;
                scan_metrics.reused_files += 1;
                files.push(previous.clone());
                continue;
            }
        }

        let bytes = fs::read(absolute_path)
            .with_context(|| format!("failed to read {}", absolute_path.display()))?;
        if is_probably_binary(&bytes) {
            scan_metrics.skipped_files += 1;
            continue;
        }

        let text = String::from_utf8_lossy(&bytes).into_owned();
        let record = FileRecord {
            path: relative_path,
            language: detect_language(absolute_path),
            size: metadata.len(),
            modified_unix_nanos,
            hash: blake3::hash(&bytes).to_hex().to_string(),
            preview: make_preview(&text),
            symbols: extract_symbols(&text),
            indexed_text: text.clone(),
            line_count: text.lines().count(),
        };

        total_indexed_bytes += record.size;
        scan_metrics.reindexed_files += 1;
        files.push(record);
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));

    let index = WorkspaceIndex {
        format_version: FORMAT_VERSION,
        workspace_id: cache.workspace_id.clone(),
        workspace_root: workspace_root.display().to_string(),
        indexed_at: Utc::now().to_rfc3339(),
        total_scanned_files,
        total_indexed_bytes,
        files,
        scan_metrics,
    };

    let generation = save_workspace_index(cache, &options_signature, &index)?;
    Ok(RefreshedWorkspaceIndex { index, generation })
}

pub fn build_workspace_overview(index: &WorkspaceIndex) -> WorkspaceOverview {
    let mut languages = BTreeMap::<String, usize>::new();
    let mut directories = BTreeMap::<String, usize>::new();
    let mut key_files = Vec::new();
    let mut highlights = Vec::new();

    for file in &index.files {
        *languages.entry(file.language.clone()).or_default() += 1;
        let directory = first_directory(&file.path).unwrap_or_else(|| ".".to_string());
        *directories.entry(directory).or_default() += 1;

        if is_key_file(&file.path) {
            key_files.push(file.path.clone());
        }

        if !file.symbols.is_empty() && highlights.len() < 12 {
            highlights.push(format!("{}: {}", file.path, file.symbols.join(", ")));
        }
    }

    let mut major_languages = languages
        .into_iter()
        .map(|(language, files)| LanguageCount { language, files })
        .collect::<Vec<_>>();
    major_languages.sort_by(|left, right| {
        right
            .files
            .cmp(&left.files)
            .then_with(|| left.language.cmp(&right.language))
    });
    major_languages.truncate(8);

    let mut top_directories = directories
        .into_iter()
        .map(|(directory, files)| DirectoryCount { directory, files })
        .collect::<Vec<_>>();
    top_directories.sort_by(|left, right| {
        right
            .files
            .cmp(&left.files)
            .then_with(|| left.directory.cmp(&right.directory))
    });
    top_directories.truncate(10);

    WorkspaceOverview {
        workspace_root: index.workspace_root.clone(),
        indexed_at: index.indexed_at.clone(),
        total_indexed_files: index.files.len(),
        total_indexed_bytes: index.total_indexed_bytes,
        major_languages,
        top_directories,
        key_files: key_files.into_iter().take(12).collect(),
        highlights,
        scan_metrics: index.scan_metrics.clone(),
    }
}

pub fn search_workspace(index: &WorkspaceIndex, query: &str, limit: usize) -> Vec<SearchHit> {
    let normalized_query = query.trim().to_lowercase();
    if normalized_query.is_empty() {
        return Vec::new();
    }

    let tokens = tokenize_query(query);
    let file_refs = index.files.iter().collect::<Vec<_>>();
    search_workspace_in_files(
        &file_refs,
        &normalized_query,
        &tokens,
        limit,
        &HashMap::new(),
    )
}

pub fn search_workspace_cached(
    index: &WorkspaceIndex,
    cache: &WorkspaceCache,
    options: &IndexOptions,
    query: &str,
    limit: usize,
) -> Vec<SearchHit> {
    let normalized_query = query.trim().to_lowercase();
    if normalized_query.is_empty() {
        return Vec::new();
    }

    let tokens = tokenize_query(query);
    let options_signature = index_options_signature(options);
    let candidate_lookup = match search_workspace_candidates(
        cache,
        &options_signature,
        &tokens,
        candidate_limit(limit),
    ) {
        Ok(candidates) => candidates
            .into_iter()
            .map(|candidate| {
                let boost = candidate.term_score + candidate.matched_terms as f64 * 2.0;
                (candidate.path, boost)
            })
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            warn!(
                "failed to query workspace search candidates from {}: {error}",
                cache.db_path.display()
            );
            HashMap::new()
        }
    };

    if candidate_lookup.is_empty() {
        return search_workspace(index, query, limit);
    }

    let candidate_paths = candidate_lookup.keys().cloned().collect::<HashSet<_>>();
    let candidate_files = index
        .files
        .iter()
        .filter(|file| candidate_paths.contains(&file.path))
        .collect::<Vec<_>>();
    search_workspace_in_files(
        &candidate_files,
        &normalized_query,
        &tokens,
        limit,
        &candidate_lookup,
    )
}

fn build_ignore_set(extra_globs: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in [
        "_tmp*/**",
        ".git/**",
        ".hg/**",
        ".svn/**",
        "node_modules/**",
        "target/**",
        "dist/**",
        "build/**",
        ".next/**",
        ".turbo/**",
        ".venv/**",
        "venv/**",
        "__pycache__/**",
    ] {
        builder.add(Glob::new(pattern)?);
    }

    for pattern in extra_globs {
        builder.add(Glob::new(pattern)?);
    }

    builder.build().context("failed to compile ignore globs")
}

fn should_ignore(relative_path: &str, ignore_set: &GlobSet) -> bool {
    ignore_set.is_match(relative_path)
}

fn normalize_relative_path(root: &Path, absolute_path: &Path) -> Result<String> {
    let relative = absolute_path
        .strip_prefix(root)
        .with_context(|| format!("{} is not in {}", absolute_path.display(), root.display()))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn detect_language(path: &Path) -> String {
    match path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
    {
        "Cargo.toml" => "toml".to_string(),
        "package.json" => "json".to_string(),
        "README.md" | "README" => "markdown".to_string(),
        "Dockerfile" => "docker".to_string(),
        _ => match path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
        {
            "c" | "h" => "c",
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
            "cs" => "csharp",
            "sh" | "bash" | "zsh" => "shell",
            "ps1" => "powershell",
            "rs" => "rust",
            "ts" | "tsx" => "typescript",
            "js" | "jsx" | "mjs" | "cjs" => "javascript",
            "py" => "python",
            "go" => "go",
            "java" => "java",
            "kt" => "kotlin",
            "scala" => "scala",
            "swift" => "swift",
            "rb" => "ruby",
            "php" => "php",
            "lua" => "lua",
            "dart" => "dart",
            "r" => "r",
            "md" => "markdown",
            "json" => "json",
            "toml" => "toml",
            "yaml" | "yml" => "yaml",
            "css" | "scss" => "css",
            "html" => "html",
            "vue" => "vue",
            "svelte" => "svelte",
            "sql" => "sql",
            "proto" => "proto",
            "xml" => "xml",
            "" => "text",
            other => other,
        }
        .to_string(),
    }
}

fn make_preview(text: &str) -> String {
    let preview = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join(" | ");

    let mut preview = preview;
    if preview.len() > 320 {
        preview.truncate(320);
        preview.push_str("...");
    }
    preview
}

fn extract_symbols(text: &str) -> Vec<String> {
    static SYMBOL_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = SYMBOL_PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"(?m)^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
            Regex::new(r"(?m)^\s*func\s+(?:\([^)]+\)\s*)?([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
            Regex::new(r"(?m)^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
            Regex::new(r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_][A-Za-z0-9_]*)")
                .unwrap(),
            Regex::new(
                r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_][A-Za-z0-9_]*)\s*=>",
            )
            .unwrap(),
            Regex::new(
                r"(?m)^\s*(?:(?:public|private|protected|internal|open|suspend|inline|tailrec|operator|override)\s+)*fun\s+([A-Za-z_][A-Za-z0-9_]*)",
            )
            .unwrap(),
            Regex::new(r"(?m)^\s*(?:public|private|protected|static|async)\s+function\s+([A-Za-z_][A-Za-z0-9_]*)")
                .unwrap(),
            Regex::new(r"(?m)^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(\)\s*\{").unwrap(),
            Regex::new(
                r"(?m)^\s*(?:export\s+)?(?:class|struct|trait|interface|enum|type)\s+([A-Za-z_][A-Za-z0-9_]*)",
            )
            .unwrap(),
            Regex::new(
                r"(?m)^\s*(?:(?:public|private|protected|internal|static|final|virtual|override|abstract|synchronized|sealed|async)\s+)+[A-Za-z_][A-Za-z0-9_<>, ?\[\]]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
            )
            .unwrap(),
            Regex::new(r"(?m)^\s{0,3}#+\s+(.{1,80})$").unwrap(),
        ]
    });

    let mut symbols = BTreeSet::new();
    for pattern in patterns {
        for captures in pattern.captures_iter(text) {
            if let Some(name) = captures.get(1) {
                let value = name.as_str().trim();
                if !value.is_empty() {
                    symbols.insert(value.to_string());
                }
            }
            if symbols.len() >= 24 {
                break;
            }
        }
        if symbols.len() >= 24 {
            break;
        }
    }

    symbols.into_iter().collect()
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }

    let sample_len = bytes.len().min(2048);
    let sample = &bytes[..sample_len];
    let nul_bytes = sample.iter().filter(|byte| **byte == 0).count();
    if nul_bytes > 0 {
        return true;
    }

    let suspicious = sample
        .iter()
        .filter(|byte| matches!(**byte, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F))
        .count();
    suspicious as f64 / sample_len as f64 > 0.15
}

fn search_workspace_in_files(
    files: &[&FileRecord],
    normalized_query: &str,
    tokens: &[String],
    limit: usize,
    candidate_boosts: &HashMap<String, f64>,
) -> Vec<SearchHit> {
    let mut hits = files
        .iter()
        .filter_map(|file| {
            let score = score_file(
                file,
                normalized_query,
                tokens,
                candidate_boosts.get(&file.path).copied(),
            );
            if score <= 0.0 {
                return None;
            }

            let (line, snippet) = best_snippet(&file.indexed_text, normalized_query, tokens);
            let matching_symbols = file
                .symbols
                .iter()
                .filter(|symbol| {
                    let symbol_lower = symbol.to_lowercase();
                    symbol_lower == normalized_query
                        || symbol_lower.contains(normalized_query)
                        || tokens.iter().any(|token| symbol_lower.contains(token))
                })
                .take(6)
                .cloned()
                .collect::<Vec<_>>();

            Some(SearchHit {
                path: file.path.clone(),
                language: file.language.clone(),
                score,
                line,
                snippet,
                summary: file.preview.clone(),
                matching_symbols,
            })
        })
        .collect::<Vec<_>>();

    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.path.cmp(&right.path))
    });
    hits.truncate(limit);
    hits
}

fn candidate_limit(limit: usize) -> usize {
    limit.saturating_mul(8).max(24)
}

fn score_file(
    file: &FileRecord,
    normalized_query: &str,
    tokens: &[String],
    candidate_boost: Option<f64>,
) -> f64 {
    let path = file.path.to_lowercase();
    let preview = file.preview.to_lowercase();
    let file_name = file
        .path
        .rsplit('/')
        .next()
        .unwrap_or(file.path.as_str())
        .to_lowercase();
    let symbols_joined = file.symbols.join(" ").to_lowercase();
    let body = file.indexed_text.to_lowercase();

    let mut score = 0.0;
    if path.contains(normalized_query) {
        score += 18.0;
    }
    if preview.contains(normalized_query) {
        score += 10.0;
    }
    if body.contains(normalized_query) {
        score += 12.0;
    }
    if file_name == normalized_query {
        score += 22.0;
    }
    if tokenize_query(&path)
        .iter()
        .any(|segment| segment == normalized_query)
    {
        score += 10.0;
    }
    if file
        .symbols
        .iter()
        .map(|symbol| symbol.to_lowercase())
        .any(|symbol| symbol == normalized_query)
    {
        score += 20.0;
    }

    let mut body_token_matches = 0usize;

    for token in tokens {
        if path.contains(token) {
            score += 7.0;
        }
        if symbols_joined.contains(token) {
            score += 5.0;
        }
        if preview.contains(token) {
            score += 3.0;
        }
        if body.contains(token) {
            score += 1.5;
            body_token_matches += 1;
        }
    }

    if !tokens.is_empty() {
        let distinct_matches = tokens
            .iter()
            .filter(|token| {
                path.contains(*token)
                    || symbols_joined.contains(*token)
                    || preview.contains(*token)
                    || body.contains(*token)
            })
            .count();
        if distinct_matches == tokens.len() {
            score += 9.0;
        } else if distinct_matches > 1 {
            score += distinct_matches as f64 * 2.5;
        }
    }
    if body_token_matches > 1 {
        score += body_token_matches as f64;
    }
    if let Some(boost) = candidate_boost {
        score += boost.min(32.0);
    }

    score
}

fn best_snippet(text: &str, normalized_query: &str, tokens: &[String]) -> (Option<usize>, String) {
    let lines = text.lines().collect::<Vec<_>>();
    let mut best_match = None;

    for (index, line) in lines.iter().enumerate() {
        let score = score_snippet_line(line, normalized_query, tokens);
        if score > 0.0 {
            match best_match {
                Some((best_score, _, _)) if best_score >= score => {}
                _ => best_match = Some((score, index, *line)),
            }
        }
    }

    if let Some((_, index, _)) = best_match {
        let start = index.saturating_sub(1);
        let end = (index + 1).min(lines.len().saturating_sub(1));
        let snippet = (start..=end)
            .map(|line_index| format!("{:>4}: {}", line_index + 1, lines[line_index].trim_end()))
            .collect::<Vec<_>>()
            .join("\n");
        return (Some(index + 1), snippet);
    }

    (None, make_preview(text))
}

fn score_snippet_line(line: &str, normalized_query: &str, tokens: &[String]) -> f64 {
    let lower = line.to_lowercase();
    let mut score = 0.0;
    if lower.contains(normalized_query) {
        score += 10.0;
    }
    let token_hits = tokens
        .iter()
        .filter(|token| !token.is_empty() && lower.contains(*token))
        .count();
    score += token_hits as f64 * 2.5;
    if !tokens.is_empty() && token_hits == tokens.len() {
        score += 4.0;
    }
    score
}

fn first_directory(path: &str) -> Option<String> {
    path.split('/').next().map(|segment| segment.to_string())
}

fn is_key_file(path: &str) -> bool {
    matches!(
        path,
        "README"
            | "README.md"
            | "Cargo.toml"
            | "package.json"
            | "pnpm-workspace.yaml"
            | "turbo.json"
            | "pyproject.toml"
            | "requirements.txt"
            | "docker-compose.yml"
            | "docker-compose.yaml"
            | ".env.example"
            | "tsconfig.json"
            | "justfile"
            | "Makefile"
    ) || path.starts_with(".github/")
}

#[cfg(test)]
mod tests;
