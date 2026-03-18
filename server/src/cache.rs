use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use blake3::Hasher;
use directories::ProjectDirs;
use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, Clone)]
pub struct WorkspaceCache {
    pub workspace_id: String,
    pub workspace_dir: PathBuf,
    pub index_file: PathBuf,
    pub memory_file: PathBuf,
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

    Ok(WorkspaceCache {
        workspace_id,
        index_file: workspace_dir.join("workspace-index.json"),
        memory_file: workspace_dir.join("memory-store.json"),
        workspace_dir,
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

pub fn load_json<T>(path: &Path) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    if !path.exists() {
        return Ok(None);
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse JSON from {}", path.display()))?;
    Ok(Some(parsed))
}

pub fn save_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize JSON for {}", path.display()))?;
    let tmp_path = path.with_extension("tmp");

    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to move {} into place", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::workspace_id;

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
}
