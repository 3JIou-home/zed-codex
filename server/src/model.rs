use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ScanMetrics {
    pub reused_files: usize,
    pub reindexed_files: usize,
    pub skipped_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileRecord {
    pub path: String,
    pub language: String,
    pub size: u64,
    pub modified_unix: u64,
    pub hash: String,
    pub preview: String,
    pub symbols: Vec<String>,
    pub indexed_text: String,
    pub line_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceIndex {
    pub format_version: u32,
    pub workspace_id: String,
    pub workspace_root: String,
    pub indexed_at: String,
    pub total_scanned_files: usize,
    pub total_indexed_bytes: u64,
    pub files: Vec<FileRecord>,
    pub scan_metrics: ScanMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LanguageCount {
    pub language: String,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DirectoryCount {
    pub directory: String,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceOverview {
    pub workspace_root: String,
    pub indexed_at: String,
    pub total_indexed_files: usize,
    pub total_indexed_bytes: u64,
    pub major_languages: Vec<LanguageCount>,
    pub top_directories: Vec<DirectoryCount>,
    pub key_files: Vec<String>,
    pub highlights: Vec<String>,
    pub scan_metrics: ScanMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub path: String,
    pub language: String,
    pub score: f64,
    pub line: Option<usize>,
    pub snippet: String,
    pub summary: String,
    pub matching_symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchResults {
    pub query: String,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemoryRecord {
    pub id: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemoryStore {
    pub workspace_id: String,
    pub workspace_root: String,
    pub updated_at: String,
    pub entries: Vec<MemoryRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemorySearchResults {
    pub query: Option<String>,
    pub matches: Vec<MemoryRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitSummary {
    pub available: bool,
    pub branch: Option<String>,
    pub status_lines: Vec<String>,
    pub recent_commits: Vec<String>,
    pub diff_stats: Vec<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CacheStatus {
    pub workspace_id: String,
    pub workspace_root: String,
    pub cache_dir: String,
    pub indexed_at: String,
    pub indexed_files: usize,
    pub indexed_bytes: u64,
    pub scan_metrics: ScanMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContextBundle {
    pub task: String,
    pub overview: WorkspaceOverview,
    pub search_hits: Vec<SearchHit>,
    pub memories: Vec<MemoryRecord>,
    pub recommended_skills: Vec<SkillMatch>,
    pub recent_changes: Option<GitSummary>,
    pub suggested_next_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WarmupStatus {
    pub workspace_root: String,
    pub elapsed_ms: u64,
    pub cache_status: CacheStatus,
    pub warmed_git: bool,
    pub warmed_memory: bool,
    pub warmed_skills: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskWorkstream {
    pub id: String,
    pub title: String,
    pub objective: String,
    pub rationale: String,
    pub recommended_files: Vec<String>,
    pub matching_symbols: Vec<String>,
    pub recommended_skills: Vec<String>,
    pub can_run_in_parallel: bool,
    pub handoff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskDecomposition {
    pub task: String,
    pub execution_mode: String,
    pub prefer_full_access: bool,
    pub can_parallelize: bool,
    pub summary: String,
    pub recommended_starting_tools: Vec<String>,
    pub shared_context: Vec<String>,
    pub recommended_skills: Vec<SkillMatch>,
    pub workstreams: Vec<TaskWorkstream>,
    pub coordination_notes: Vec<String>,
    pub first_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrchestrationStage {
    pub id: String,
    pub title: String,
    pub objective: String,
    pub workstream_ids: Vec<String>,
    pub run_in_parallel: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubagentSpec {
    pub workstream_id: String,
    pub title: String,
    pub agent_role: String,
    pub run_in_parallel: bool,
    pub objective: String,
    pub recommended_files: Vec<String>,
    pub matching_symbols: Vec<String>,
    pub shared_context: Vec<String>,
    pub recommended_skills: Vec<SkillMatch>,
    pub completion_criteria: Vec<String>,
    pub handoff: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskOrchestration {
    pub task: String,
    pub execution_mode: String,
    pub prefer_full_access: bool,
    pub summary: String,
    pub context_bundle: ContextBundle,
    pub decomposition: TaskDecomposition,
    pub stages: Vec<OrchestrationStage>,
    pub subagent_specs: Vec<SubagentSpec>,
    pub recommended_host_steps: Vec<String>,
    pub host_constraints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: String,
    pub source_root: String,
    pub category: String,
    pub emoji: Option<String>,
    pub vibe: Option<String>,
    pub tags: Vec<String>,
    pub preview: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillCatalog {
    pub roots: Vec<String>,
    pub indexed_at: String,
    pub total_skills: usize,
    pub skills: Vec<SkillRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillMatch {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: String,
    pub source_root: String,
    pub category: String,
    pub emoji: Option<String>,
    pub vibe: Option<String>,
    pub preview: String,
    pub score: f64,
    pub match_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillSearchResults {
    pub query: Option<String>,
    pub indexed_at: Option<String>,
    pub hits: Vec<SkillMatch>,
}
