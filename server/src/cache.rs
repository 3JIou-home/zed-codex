use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{bail, Context, Result};
use blake3::Hasher;
use directories::ProjectDirs;
use rusqlite::{
    params, params_from_iter,
    types::{Type, Value},
    Connection, OpenFlags, OptionalExtension, Transaction,
};
use serde::de::DeserializeOwned;
use tracing::warn;

use crate::model::{FileRecord, MemoryStore, SkillCatalog, SkillRecord, WorkspaceIndex};
use crate::text::tokenize_search_terms;

const CACHE_DB_FILENAME: &str = "workspace-cache.sqlite";
const CACHE_SCHEMA_VERSION: u32 = 3;
const SCHEMA_VERSION_KEY: &str = "schema_version";
const INDEX_GENERATION_KEY: &str = "index_generation";
const MEMORY_GENERATION_KEY: &str = "memory_generation";
const SKILLS_GENERATION_KEY: &str = "skills_generation";
const MAX_WORKSPACE_QUERY_TERMS: usize = 24;

#[derive(Debug, Clone)]
pub struct WorkspaceCache {
    pub workspace_id: String,
    pub workspace_dir: PathBuf,
    pub db_path: PathBuf,
    schema_initialized: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PersistentCacheGenerations {
    pub index: u64,
    pub memory: u64,
    pub skills: u64,
}

#[derive(Debug, Clone)]
pub struct CachedValue<T> {
    pub value: T,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSearchCandidate {
    pub path: String,
    pub term_score: f64,
    pub matched_terms: usize,
}

pub fn resolve_workspace_cache(
    workspace_root: &Path,
    cache_override: Option<&PathBuf>,
) -> Result<WorkspaceCache> {
    if !workspace_root.exists() {
        bail!(
            "workspace root does not exist: {}",
            workspace_root.display()
        );
    }
    if !workspace_root.is_dir() {
        bail!(
            "workspace root is not a directory: {}",
            workspace_root.display()
        );
    }

    let base_dir = match cache_override {
        Some(path) => path.clone(),
        None => ProjectDirs::from("dev", "codex", "codex-companion")
            .context("failed to determine a cache directory for Codex Companion")?
            .cache_dir()
            .to_path_buf(),
    };

    fs::create_dir_all(&base_dir)
        .with_context(|| format!("failed to create cache directory {}", base_dir.display()))?;

    let workspace_id = workspace_id(workspace_root);
    let workspace_dir = base_dir.join("workspaces").join(&workspace_id);
    fs::create_dir_all(&workspace_dir).with_context(|| {
        format!(
            "failed to create workspace cache directory {}",
            workspace_dir.display()
        )
    })?;

    let db_path = workspace_dir.join(CACHE_DB_FILENAME);
    let schema_initialized = Arc::new(AtomicBool::new(false));
    open_cache_write_db_path(&db_path, &schema_initialized)?;

    Ok(WorkspaceCache {
        workspace_id,
        workspace_dir,
        db_path,
        schema_initialized,
    })
}

pub fn workspace_id(workspace_root: &Path) -> String {
    let mut hasher = Hasher::new();
    let normalized = workspace_root.to_string_lossy().replace('\\', "/");
    let normalized = if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    };
    hasher.update(normalized.as_bytes());
    hasher.finalize().to_hex().to_string()
}

pub fn load_persistent_generations(cache: &WorkspaceCache) -> Result<PersistentCacheGenerations> {
    with_cache_recovery(cache, "load persistent cache generations", || {
        let connection = open_cache_read_db(cache)?;
        read_all_generations(&connection)
    })
}

pub fn load_workspace_index(
    cache: &WorkspaceCache,
    options_signature: &str,
) -> Result<Option<CachedValue<WorkspaceIndex>>> {
    with_cache_recovery(cache, "load workspace index", || {
        load_workspace_index_inner(cache, options_signature)
    })
}

fn load_workspace_index_inner(
    cache: &WorkspaceCache,
    options_signature: &str,
) -> Result<Option<CachedValue<WorkspaceIndex>>> {
    let connection = open_cache_read_db(cache)?;
    let generation = read_generation(&connection, INDEX_GENERATION_KEY)?;
    let Some((workspace_id, workspace_root, indexed_at, total_scanned_files, total_indexed_bytes, scan_metrics, format_version, cached_signature)) =
        connection
            .query_row(
                "SELECT workspace_id, workspace_root, indexed_at, total_scanned_files, total_indexed_bytes, scan_metrics_json, format_version, options_signature
                 FROM workspace_index_meta
                 WHERE singleton = 1",
                [],
                |row| {
                    let scan_metrics_json = row.get::<_, String>(5)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u64>(3)? as usize,
                        row.get::<_, u64>(4)?,
                        parse_json_column(5, &scan_metrics_json)?,
                        row.get::<_, u64>(6)? as u32,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?
    else {
        return Ok(None);
    };
    if cached_signature != options_signature {
        return Ok(None);
    }

    let mut statement = connection.prepare(
        "SELECT path, language, size, modified_unix_nanos, hash, preview, symbols_json, indexed_text, line_count
         FROM workspace_index_files
         ORDER BY path",
    )?;
    let files = statement
        .query_map([], |row| {
            let symbols_json = row.get::<_, String>(6)?;
            Ok(FileRecord {
                path: row.get(0)?,
                language: row.get(1)?,
                size: row.get(2)?,
                modified_unix_nanos: row.get(3)?,
                hash: row.get(4)?,
                preview: row.get(5)?,
                symbols: parse_json_column(6, &symbols_json)?,
                indexed_text: row.get(7)?,
                line_count: row.get::<_, u64>(8)? as usize,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(Some(CachedValue {
        value: WorkspaceIndex {
            format_version,
            workspace_id,
            workspace_root,
            indexed_at,
            total_scanned_files,
            total_indexed_bytes,
            files,
            scan_metrics,
        },
        generation,
    }))
}

pub fn save_workspace_index(
    cache: &WorkspaceCache,
    options_signature: &str,
    index: &WorkspaceIndex,
) -> Result<u64> {
    with_cache_recovery(cache, "save workspace index", || {
        save_workspace_index_inner(cache, options_signature, index)
    })
}

fn save_workspace_index_inner(
    cache: &WorkspaceCache,
    options_signature: &str,
    index: &WorkspaceIndex,
) -> Result<u64> {
    let mut connection = open_cache_write_db(cache)?;
    let transaction = connection.transaction()?;
    let scan_metrics_json = serde_json::to_string(&index.scan_metrics)
        .context("failed to serialize workspace index scan metrics")?;

    transaction.execute("DELETE FROM workspace_index_files", [])?;
    transaction.execute("DELETE FROM workspace_index_meta", [])?;
    transaction.execute("DELETE FROM workspace_index_terms", [])?;
    transaction.execute(
        "INSERT INTO workspace_index_meta (
             singleton,
             workspace_id,
             workspace_root,
             indexed_at,
             total_scanned_files,
             total_indexed_bytes,
             scan_metrics_json,
             format_version,
             options_signature
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            index.workspace_id,
            index.workspace_root,
            index.indexed_at,
            index.total_scanned_files,
            index.total_indexed_bytes,
            scan_metrics_json,
            index.format_version,
            options_signature,
        ],
    )?;

    {
        let mut file_statement = transaction.prepare(
            "INSERT INTO workspace_index_files (
                 path,
                 language,
                 size,
                 modified_unix_nanos,
                 hash,
                 preview,
                 symbols_json,
                 indexed_text,
                 line_count
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut term_statement = transaction.prepare(
            "INSERT INTO workspace_index_terms (
                 path,
                 term,
                 field,
                 hits
             ) VALUES (?1, ?2, ?3, ?4)",
        )?;

        for file in &index.files {
            file_statement.execute(params![
                file.path,
                file.language,
                file.size,
                file.modified_unix_nanos,
                file.hash,
                file.preview,
                serde_json::to_string(&file.symbols)
                    .context("failed to serialize workspace index symbols")?,
                file.indexed_text,
                file.line_count as u64,
            ])?;
            insert_search_terms(&mut term_statement, file)?;
        }
    }

    let generation = bump_generation(&transaction, INDEX_GENERATION_KEY)?;
    transaction.commit()?;
    Ok(generation)
}

pub fn load_memory_store(cache: &WorkspaceCache) -> Result<Option<CachedValue<MemoryStore>>> {
    with_cache_recovery(cache, "load memory store", || {
        load_memory_store_inner(cache)
    })
}

fn load_memory_store_inner(cache: &WorkspaceCache) -> Result<Option<CachedValue<MemoryStore>>> {
    let connection = open_cache_read_db(cache)?;
    let generation = read_generation(&connection, MEMORY_GENERATION_KEY)?;
    let Some((workspace_id, workspace_root, updated_at)) = connection
        .query_row(
            "SELECT workspace_id, workspace_root, updated_at
             FROM memory_store
             WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };

    let mut statement = connection.prepare(
        "SELECT id, title, content, tags_json, importance, created_at, updated_at
         FROM memory_entries
         ORDER BY position ASC",
    )?;
    let entries = statement
        .query_map([], |row| {
            let tags_json = row.get::<_, String>(3)?;
            Ok(crate::model::MemoryRecord {
                id: row.get(0)?,
                title: row.get(1)?,
                content: row.get(2)?,
                tags: parse_json_column(3, &tags_json)?,
                importance: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(Some(CachedValue {
        value: MemoryStore {
            workspace_id,
            workspace_root,
            updated_at,
            entries,
        },
        generation,
    }))
}

pub fn save_memory_store(cache: &WorkspaceCache, store: &MemoryStore) -> Result<u64> {
    with_cache_recovery(cache, "save memory store", || {
        save_memory_store_inner(cache, store)
    })
}

fn save_memory_store_inner(cache: &WorkspaceCache, store: &MemoryStore) -> Result<u64> {
    let mut connection = open_cache_write_db(cache)?;
    let transaction = connection.transaction()?;

    transaction.execute("DELETE FROM memory_entries", [])?;
    transaction.execute("DELETE FROM memory_store", [])?;
    transaction.execute(
        "INSERT INTO memory_store (singleton, workspace_id, workspace_root, updated_at)
         VALUES (1, ?1, ?2, ?3)",
        params![store.workspace_id, store.workspace_root, store.updated_at],
    )?;

    {
        let mut statement = transaction.prepare(
            "INSERT INTO memory_entries (
                 id,
                 position,
                 title,
                 content,
                 tags_json,
                 importance,
                 created_at,
                 updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;

        for (position, entry) in store.entries.iter().enumerate() {
            statement.execute(params![
                entry.id,
                position as u64,
                entry.title,
                entry.content,
                serde_json::to_string(&entry.tags).context("failed to serialize memory tags")?,
                entry.importance,
                entry.created_at,
                entry.updated_at,
            ])?;
        }
    }

    let generation = bump_generation(&transaction, MEMORY_GENERATION_KEY)?;
    transaction.commit()?;
    Ok(generation)
}

pub fn load_skill_catalog(
    cache: &WorkspaceCache,
    options_signature: &str,
) -> Result<Option<CachedValue<SkillCatalog>>> {
    with_cache_recovery(cache, "load skill catalog", || {
        load_skill_catalog_inner(cache, options_signature)
    })
}

fn load_skill_catalog_inner(
    cache: &WorkspaceCache,
    options_signature: &str,
) -> Result<Option<CachedValue<SkillCatalog>>> {
    let connection = open_cache_read_db(cache)?;
    let generation = read_generation(&connection, SKILLS_GENERATION_KEY)?;
    let Some((indexed_at, total_skills, roots_json, cached_signature)) = connection
        .query_row(
            "SELECT indexed_at, total_skills, roots_json, options_signature
             FROM skill_catalog_meta
             WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)? as usize,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    if cached_signature != options_signature {
        return Ok(None);
    }

    let mut statement = connection.prepare(
        "SELECT id, name, description, path, source_root, category, emoji, vibe, tags_json, preview, content
         FROM skill_records
         ORDER BY category, name, path",
    )?;
    let skills = statement
        .query_map([], |row| {
            let tags_json = row.get::<_, String>(8)?;
            Ok(SkillRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                path: row.get(3)?,
                source_root: row.get(4)?,
                category: row.get(5)?,
                emoji: row.get(6)?,
                vibe: row.get(7)?,
                tags: parse_json_column(8, &tags_json)?,
                preview: row.get(9)?,
                content: row.get(10)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(Some(CachedValue {
        value: SkillCatalog {
            roots: parse_json_column(2, &roots_json)?,
            indexed_at,
            total_skills,
            skills,
        },
        generation,
    }))
}

pub fn save_skill_catalog(
    cache: &WorkspaceCache,
    options_signature: &str,
    catalog: &SkillCatalog,
) -> Result<u64> {
    with_cache_recovery(cache, "save skill catalog", || {
        save_skill_catalog_inner(cache, options_signature, catalog)
    })
}

fn save_skill_catalog_inner(
    cache: &WorkspaceCache,
    options_signature: &str,
    catalog: &SkillCatalog,
) -> Result<u64> {
    let mut connection = open_cache_write_db(cache)?;
    let transaction = connection.transaction()?;

    transaction.execute("DELETE FROM skill_records", [])?;
    transaction.execute("DELETE FROM skill_catalog_meta", [])?;
    transaction.execute(
        "INSERT INTO skill_catalog_meta (
             singleton,
             indexed_at,
             total_skills,
             roots_json,
             options_signature
         ) VALUES (1, ?1, ?2, ?3, ?4)",
        params![
            catalog.indexed_at,
            catalog.total_skills as u64,
            serde_json::to_string(&catalog.roots)
                .context("failed to serialize skill catalog roots")?,
            options_signature,
        ],
    )?;

    {
        let mut statement = transaction.prepare(
            "INSERT INTO skill_records (
                 id,
                 name,
                 description,
                 path,
                 source_root,
                 category,
                 emoji,
                 vibe,
                 tags_json,
                 preview,
                 content
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;

        for skill in &catalog.skills {
            statement.execute(params![
                skill.id,
                skill.name,
                skill.description,
                skill.path,
                skill.source_root,
                skill.category,
                skill.emoji,
                skill.vibe,
                serde_json::to_string(&skill.tags).context("failed to serialize skill tags")?,
                skill.preview,
                skill.content,
            ])?;
        }
    }

    let generation = bump_generation(&transaction, SKILLS_GENERATION_KEY)?;
    transaction.commit()?;
    Ok(generation)
}

pub fn search_workspace_candidates(
    cache: &WorkspaceCache,
    options_signature: &str,
    query_terms: &[String],
    limit: usize,
) -> Result<Vec<WorkspaceSearchCandidate>> {
    with_cache_recovery(cache, "search workspace candidates", || {
        search_workspace_candidates_inner(cache, options_signature, query_terms, limit)
    })
}

fn search_workspace_candidates_inner(
    cache: &WorkspaceCache,
    options_signature: &str,
    query_terms: &[String],
    limit: usize,
) -> Result<Vec<WorkspaceSearchCandidate>> {
    let query_terms = prepare_workspace_query_terms(query_terms);
    if query_terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    let connection = open_cache_read_db(cache)?;
    let Some(cached_signature) = connection
        .query_row(
            "SELECT options_signature
             FROM workspace_index_meta
             WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(Vec::new());
    };
    if cached_signature != options_signature {
        return Ok(Vec::new());
    }

    let placeholders = (0..query_terms.len())
        .map(|index| format!("?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let limit_placeholder = query_terms.len() + 1;
    let sql = format!(
        "SELECT path,
                SUM(
                    CASE field
                        WHEN 'path' THEN hits * 9.0
                        WHEN 'symbol' THEN hits * 7.0
                        WHEN 'preview' THEN hits * 3.5
                        ELSE hits * 1.0
                    END
                ) AS term_score,
                COUNT(DISTINCT term) AS matched_terms
         FROM workspace_index_terms
         WHERE term IN ({placeholders})
         GROUP BY path
         ORDER BY matched_terms DESC, term_score DESC, path ASC
         LIMIT ?{limit_placeholder}"
    );

    let mut params = query_terms
        .iter()
        .cloned()
        .map(Value::from)
        .collect::<Vec<_>>();
    params.push(Value::Integer(limit as i64));

    let mut statement = connection.prepare(&sql)?;
    let candidates = statement
        .query_map(params_from_iter(params), |row| {
            Ok(WorkspaceSearchCandidate {
                path: row.get(0)?,
                term_score: row.get(1)?,
                matched_terms: row.get::<_, i64>(2)? as usize,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)?;
    Ok(candidates)
}

fn prepare_workspace_query_terms(query_terms: &[String]) -> Vec<String> {
    let mut prepared = Vec::new();
    let mut seen = HashSet::new();

    for term in query_terms {
        let normalized = term.trim().to_lowercase();
        if normalized.len() < 2 || !seen.insert(normalized.clone()) {
            continue;
        }
        prepared.push(normalized);
        if prepared.len() >= MAX_WORKSPACE_QUERY_TERMS {
            break;
        }
    }

    prepared
}

fn insert_search_terms(statement: &mut rusqlite::Statement<'_>, file: &FileRecord) -> Result<()> {
    for (field, terms) in [
        ("path", count_search_terms(&file.path, None)),
        ("preview", count_search_terms(&file.preview, None)),
        ("symbol", count_search_terms(&file.symbols.join(" "), None)),
        ("body", count_search_terms(&file.indexed_text, None)),
    ] {
        for (term, hits) in terms {
            statement.execute(params![file.path, term, field, hits as u64])?;
        }
    }

    Ok(())
}

fn count_search_terms(text: &str, unique_limit: Option<usize>) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for token in tokenize_search_terms(text) {
        if let Some(limit) = unique_limit {
            if counts.len() >= limit && !counts.contains_key(&token) {
                continue;
            }
        }
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

fn with_cache_recovery<T>(
    cache: &WorkspaceCache,
    operation: &str,
    mut action: impl FnMut() -> Result<T>,
) -> Result<T> {
    match action() {
        Ok(value) => Ok(value),
        Err(error) => {
            if !is_cache_corruption_error(&error) {
                return Err(error);
            }

            warn!(
                "resetting corrupted workspace cache {} after `{}` failed: {error:#}",
                cache.db_path.display(),
                operation
            );
            reset_cache_storage(cache)?;
            action().with_context(|| format!("{operation} after cache reset"))
        }
    }
}

fn is_cache_corruption_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string().to_lowercase();
        message.contains("not a database")
            || message.contains("file is not a database")
            || message.contains("database disk image is malformed")
            || message.contains("malformed database schema")
    })
}

fn reset_cache_storage(cache: &WorkspaceCache) -> Result<()> {
    reset_cache_storage_path(&cache.db_path, &cache.schema_initialized)
}

fn reset_cache_storage_path(path: &Path, schema_initialized: &Arc<AtomicBool>) -> Result<()> {
    schema_initialized.store(false, Ordering::Release);

    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove corrupted cache file {}",
                        candidate.display()
                    )
                })
            }
        }
    }

    Ok(())
}

fn open_cache_read_db(cache: &WorkspaceCache) -> Result<Connection> {
    let schema_initialized = cache.schema_initialized.load(Ordering::Acquire);
    if !schema_initialized {
        return open_cache_write_db(cache);
    }
    open_cache_db(&cache.db_path, ConnectionMode::ReadOnly, schema_initialized)
}

fn open_cache_write_db(cache: &WorkspaceCache) -> Result<Connection> {
    open_cache_write_db_path(&cache.db_path, &cache.schema_initialized)
}

fn open_cache_write_db_path(
    path: &Path,
    schema_initialized: &Arc<AtomicBool>,
) -> Result<Connection> {
    match open_cache_write_db_path_inner(path, schema_initialized) {
        Ok(connection) => Ok(connection),
        Err(error) => {
            if !is_cache_corruption_error(&error) {
                return Err(error);
            }

            warn!(
                "resetting corrupted workspace cache {} while opening write connection: {error:#}",
                path.display()
            );
            reset_cache_storage_path(path, schema_initialized)?;
            open_cache_write_db_path_inner(path, schema_initialized)
                .with_context(|| format!("failed to reinitialize cache at {}", path.display()))
        }
    }
}

fn open_cache_write_db_path_inner(
    path: &Path,
    schema_initialized: &Arc<AtomicBool>,
) -> Result<Connection> {
    let connection = open_cache_db(
        path,
        ConnectionMode::ReadWrite,
        schema_initialized.load(Ordering::Acquire),
    )?;
    if !schema_initialized.load(Ordering::Acquire) {
        ensure_schema(&connection)?;
        schema_initialized.store(true, Ordering::Release);
    }
    Ok(connection)
}

#[derive(Clone, Copy)]
enum ConnectionMode {
    ReadOnly,
    ReadWrite,
}

fn open_cache_db(
    path: &Path,
    mode: ConnectionMode,
    schema_initialized: bool,
) -> Result<Connection> {
    match mode {
        ConnectionMode::ReadOnly => {
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("failed to open {} in read-only mode", path.display()))
        }
        ConnectionMode::ReadWrite => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }

            let connection = Connection::open(path)
                .with_context(|| format!("failed to open {}", path.display()))?;
            if !schema_initialized {
                connection
                    .pragma_update(None, "journal_mode", "WAL")
                    .context("failed to enable WAL mode for workspace cache")?;
            }
            connection
                .pragma_update(None, "synchronous", "NORMAL")
                .context("failed to configure SQLite synchronous mode")?;
            connection
                .pragma_update(None, "temp_store", "MEMORY")
                .context("failed to configure SQLite temp_store mode")?;
            Ok(connection)
        }
    }
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );",
    )?;

    let schema_version = read_metadata_value(connection, SCHEMA_VERSION_KEY)?
        .map(|value| {
            value
                .parse::<u32>()
                .with_context(|| format!("invalid cached schema_version `{value}`"))
        })
        .transpose()?;

    if schema_version != Some(CACHE_SCHEMA_VERSION) {
        connection.execute_batch(
            "DROP TABLE IF EXISTS workspace_index_files;
             DROP TABLE IF EXISTS workspace_index_meta;
             DROP TABLE IF EXISTS workspace_index_terms;
             DROP TABLE IF EXISTS memory_entries;
             DROP TABLE IF EXISTS memory_store;
             DROP TABLE IF EXISTS skill_records;
             DROP TABLE IF EXISTS skill_catalog_meta;
             DROP INDEX IF EXISTS idx_workspace_index_terms_term;
             DELETE FROM metadata;",
        )?;
    }

    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_index_meta (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             workspace_id TEXT NOT NULL,
             workspace_root TEXT NOT NULL,
             indexed_at TEXT NOT NULL,
             total_scanned_files INTEGER NOT NULL,
             total_indexed_bytes INTEGER NOT NULL,
             scan_metrics_json TEXT NOT NULL,
             format_version INTEGER NOT NULL,
             options_signature TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workspace_index_files (
             path TEXT PRIMARY KEY,
             language TEXT NOT NULL,
             size INTEGER NOT NULL,
             modified_unix_nanos INTEGER NOT NULL,
             hash TEXT NOT NULL,
             preview TEXT NOT NULL,
             symbols_json TEXT NOT NULL,
             indexed_text TEXT NOT NULL,
             line_count INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workspace_index_terms (
             path TEXT NOT NULL,
             term TEXT NOT NULL,
             field TEXT NOT NULL,
             hits INTEGER NOT NULL,
             PRIMARY KEY (path, term, field)
         );
         CREATE INDEX IF NOT EXISTS idx_workspace_index_terms_term
             ON workspace_index_terms (term);
         CREATE TABLE IF NOT EXISTS memory_store (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             workspace_id TEXT NOT NULL,
             workspace_root TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS memory_entries (
             id TEXT PRIMARY KEY,
             position INTEGER NOT NULL,
             title TEXT NOT NULL,
             content TEXT NOT NULL,
             tags_json TEXT NOT NULL,
             importance TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS skill_catalog_meta (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             indexed_at TEXT NOT NULL,
             total_skills INTEGER NOT NULL,
             roots_json TEXT NOT NULL,
             options_signature TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS skill_records (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             description TEXT NOT NULL,
             path TEXT NOT NULL,
             source_root TEXT NOT NULL,
             category TEXT NOT NULL,
             emoji TEXT,
             vibe TEXT,
             tags_json TEXT NOT NULL,
             preview TEXT NOT NULL,
             content TEXT NOT NULL
         );",
    )?;

    upsert_metadata(connection, SCHEMA_VERSION_KEY, CACHE_SCHEMA_VERSION)?;
    ensure_generation(connection, INDEX_GENERATION_KEY)?;
    ensure_generation(connection, MEMORY_GENERATION_KEY)?;
    ensure_generation(connection, SKILLS_GENERATION_KEY)?;
    Ok(())
}

fn ensure_generation(connection: &Connection, key: &str) -> Result<()> {
    if read_metadata_value(connection, key)?.is_none() {
        upsert_metadata(connection, key, 0u64)?;
    }
    Ok(())
}

fn read_generation(connection: &Connection, key: &str) -> Result<u64> {
    read_metadata_value(connection, key)?
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("invalid cached generation `{value}` for `{key}`"))
        })
        .transpose()
        .map(|value| value.unwrap_or_default())
}

fn read_all_generations(connection: &Connection) -> Result<PersistentCacheGenerations> {
    let mut generations = PersistentCacheGenerations::default();
    let mut statement = connection.prepare(
        "SELECT key, value
         FROM metadata
         WHERE key IN (?1, ?2, ?3)",
    )?;
    let rows = statement.query_map(
        params![
            INDEX_GENERATION_KEY,
            MEMORY_GENERATION_KEY,
            SKILLS_GENERATION_KEY
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;

    for row in rows {
        let (key, value) = row?;
        let parsed = value
            .parse::<u64>()
            .with_context(|| format!("invalid cached generation `{value}` for `{key}`"))?;
        match key.as_str() {
            INDEX_GENERATION_KEY => generations.index = parsed,
            MEMORY_GENERATION_KEY => generations.memory = parsed,
            SKILLS_GENERATION_KEY => generations.skills = parsed,
            _ => {}
        }
    }

    Ok(generations)
}

fn read_metadata_value(connection: &Connection, key: &str) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read cache metadata")
}

fn upsert_metadata(connection: &Connection, key: &str, value: impl ToString) -> Result<()> {
    connection.execute(
        "INSERT INTO metadata (key, value)
         VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value.to_string()],
    )?;
    Ok(())
}

fn bump_generation(transaction: &Transaction<'_>, key: &str) -> Result<u64> {
    let current = transaction
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("invalid cached generation `{value}` for `{key}`"))
        })
        .transpose()?
        .unwrap_or_default();
    let next = current.saturating_add(1);
    transaction.execute(
        "INSERT INTO metadata (key, value)
         VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, next.to_string()],
    )?;
    Ok(next)
}

fn parse_json_column<T>(column_index: usize, value: &str) -> rusqlite::Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column_index, Type::Text, Box::new(error))
    })
}

#[cfg(test)]
mod tests;
