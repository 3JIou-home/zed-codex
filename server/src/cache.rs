use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use blake3::Hasher;
use directories::ProjectDirs;
use rusqlite::{params, types::Type, Connection, OptionalExtension, Transaction};
use serde::de::DeserializeOwned;

use crate::model::{FileRecord, MemoryStore, SkillCatalog, SkillRecord, WorkspaceIndex};

const CACHE_DB_FILENAME: &str = "workspace-cache.sqlite";
const CACHE_SCHEMA_VERSION: u32 = 2;
const SCHEMA_VERSION_KEY: &str = "schema_version";
const INDEX_GENERATION_KEY: &str = "index_generation";
const MEMORY_GENERATION_KEY: &str = "memory_generation";
const SKILLS_GENERATION_KEY: &str = "skills_generation";

#[derive(Debug, Clone)]
pub struct WorkspaceCache {
    pub workspace_id: String,
    pub workspace_dir: PathBuf,
    pub db_path: PathBuf,
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

pub fn resolve_workspace_cache(
    workspace_root: &Path,
    cache_override: Option<&PathBuf>,
) -> Result<WorkspaceCache> {
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
    open_cache_db(&db_path)?;

    Ok(WorkspaceCache {
        workspace_id,
        workspace_dir,
        db_path,
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
    let connection = open_cache_db(&cache.db_path)?;
    Ok(PersistentCacheGenerations {
        index: read_generation(&connection, INDEX_GENERATION_KEY)?,
        memory: read_generation(&connection, MEMORY_GENERATION_KEY)?,
        skills: read_generation(&connection, SKILLS_GENERATION_KEY)?,
    })
}

pub fn load_workspace_index(
    cache: &WorkspaceCache,
    options_signature: &str,
) -> Result<Option<CachedValue<WorkspaceIndex>>> {
    let connection = open_cache_db(&cache.db_path)?;
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
    let mut connection = open_cache_db(&cache.db_path)?;
    let transaction = connection.transaction()?;
    let scan_metrics_json = serde_json::to_string(&index.scan_metrics)
        .context("failed to serialize workspace index scan metrics")?;

    transaction.execute("DELETE FROM workspace_index_files", [])?;
    transaction.execute("DELETE FROM workspace_index_meta", [])?;
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
        let mut statement = transaction.prepare(
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

        for file in &index.files {
            statement.execute(params![
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
        }
    }

    let generation = bump_generation(&transaction, INDEX_GENERATION_KEY)?;
    transaction.commit()?;
    Ok(generation)
}

pub fn load_memory_store(cache: &WorkspaceCache) -> Result<Option<CachedValue<MemoryStore>>> {
    let connection = open_cache_db(&cache.db_path)?;
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
    let mut connection = open_cache_db(&cache.db_path)?;
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
    let connection = open_cache_db(&cache.db_path)?;
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
    let mut connection = open_cache_db(&cache.db_path)?;
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

fn open_cache_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let connection =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .context("failed to enable WAL mode for workspace cache")?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .context("failed to configure SQLite synchronous mode")?;
    connection
        .pragma_update(None, "temp_store", "MEMORY")
        .context("failed to configure SQLite temp_store mode")?;
    connection
        .pragma_update(None, "foreign_keys", 1)
        .context("failed to enable SQLite foreign keys")?;

    ensure_schema(&connection)?;
    Ok(connection)
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
             DROP TABLE IF EXISTS memory_entries;
             DROP TABLE IF EXISTS memory_store;
             DROP TABLE IF EXISTS skill_records;
             DROP TABLE IF EXISTS skill_catalog_meta;
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
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        load_memory_store, load_persistent_generations, load_skill_catalog, load_workspace_index,
        resolve_workspace_cache, save_memory_store, save_skill_catalog, save_workspace_index,
        workspace_id,
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

        let generations =
            load_persistent_generations(&cache).expect("generations must be readable");
        assert_eq!(generations.index, 1);
        assert_eq!(generations.memory, 0);
        assert_eq!(generations.skills, 0);
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

        let generation = save_skill_catalog(&cache, options_signature, &catalog)
            .expect("skill catalog must save");
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

        let generations =
            load_persistent_generations(&cache).expect("generations must be readable");
        assert_eq!(generations.index, 0);
        assert_eq!(generations.memory, 0);
        assert_eq!(generations.skills, 1);
    }
}
