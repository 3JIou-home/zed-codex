use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use schemars::JsonSchema;
use serde::Deserialize;
use zed::settings::ContextServerSettings;
use zed_extension_api::process::Command as ProcessCommand;
use zed_extension_api::{
    self as zed, serde_json, Command, ContextServerConfiguration, ContextServerId,
    GithubReleaseOptions, Project, Result, SlashCommand, SlashCommandOutput,
    SlashCommandOutputSection, Worktree,
};

const CONTEXT_SERVER_ID: &str = "codex-companion";
const EXTENSION_MANIFEST: &str = include_str!("../extension.toml");

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
struct CodexCompanionSettings {
    server_path: Option<String>,
    cache_dir: Option<String>,
    release_repo: Option<String>,
    ignore_globs: Option<Vec<String>>,
    max_file_bytes: Option<usize>,
    max_indexed_files: Option<usize>,
    enable_git_tools: Option<bool>,
    refresh_window_secs: Option<u64>,
    git_cache_ttl_secs: Option<u64>,
    bundle_cache_ttl_secs: Option<u64>,
    prewarm_on_start: Option<bool>,
    execution_mode: Option<String>,
    prefer_full_access: Option<bool>,
    max_parallel_workstreams: Option<usize>,
    skill_roots: Option<Vec<String>>,
    skill_file_globs: Option<Vec<String>>,
    max_skill_bytes: Option<usize>,
    skill_cache_ttl_secs: Option<u64>,
    max_skills_per_query: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WorktreeSettingsFile {
    #[serde(default)]
    context_servers: HashMap<String, WorktreeContextServerEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WorktreeContextServerEntry {
    settings: Option<CodexCompanionSettings>,
}

struct CodexCompanionExtension {
    cached_server_path: Option<String>,
    cached_server_env: Vec<(String, String)>,
    cached_release_repo: Option<String>,
}

#[derive(Debug, Clone)]
struct ServerPathCandidate {
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct ResolvedLaunchConfig {
    server_path: String,
    server_env: Vec<(String, String)>,
}

impl CodexCompanionExtension {
    fn load_settings(&self, project: &Project) -> Result<CodexCompanionSettings> {
        let settings = ContextServerSettings::for_project(CONTEXT_SERVER_ID, project)?;
        let Some(raw_settings) = settings.settings else {
            return Ok(CodexCompanionSettings::default());
        };

        serde_json::from_value(raw_settings).map_err(|error| error.to_string())
    }

    fn load_worktree_settings(&self, worktree: &Worktree) -> Option<CodexCompanionSettings> {
        let root_path = worktree.root_path();
        let root = Path::new(&root_path);
        let settings_candidates = [
            root.join(".zed").join("settings.jsonc"),
            root.join(".zed").join("settings.json"),
            root.join(".zed").join("settings"),
        ];

        for path in settings_candidates {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            if let Some(settings) = parse_worktree_settings(&content) {
                return Some(settings);
            }
        }

        None
    }

    fn resolve_server_path(
        &mut self,
        server_path: Option<String>,
        release_repo: Option<String>,
    ) -> Result<String> {
        if let Some(path) = &self.cached_server_path {
            if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
                return Ok(path.clone());
            }
        }

        let mut searched_paths = Vec::new();
        for candidate in self.server_path_candidates(server_path) {
            let display = candidate.path.display().to_string();
            searched_paths.push(display.clone());
            if fs::metadata(&candidate.path).is_ok_and(|metadata| metadata.is_file()) {
                let path = candidate.path.to_string_lossy().to_string();
                self.cached_server_path = Some(path.clone());
                return Ok(path);
            }
        }

        let repo = normalize_non_empty(release_repo)
            .or_else(|| normalize_non_empty(env::var("CODEX_COMPANION_RELEASE_REPO").ok()))
            .or_else(default_release_repo);

        if let Some(repo) = repo {
            let downloaded = self.download_server_from_release(&repo)?;
            self.cached_server_path = Some(downloaded.clone());
            return Ok(downloaded);
        }

        Err(format!(
            "Codex Companion server binary was not found. Build it with `cargo build --release -p codex-companion-server`, set `server_path`, or configure `release_repo`. Automatic release discovery only works when `extension.toml` contains a real GitHub `repository` URL. Searched: {}",
            searched_paths.join(", ")
        ))
    }

    fn server_path_candidates(
        &self,
        explicit_server_path: Option<String>,
    ) -> Vec<ServerPathCandidate> {
        let mut candidates = Vec::new();

        if let Some(path) = explicit_server_path {
            Self::push_candidate(&mut candidates, path);
        }

        if let Ok(path) = env::var("CODEX_COMPANION_SERVER_PATH") {
            Self::push_candidate(&mut candidates, path);
        }

        Self::push_candidate(
            &mut candidates,
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("release")
                .join(self.binary_name()),
        );

        if let Ok(pwd) = env::var("PWD") {
            Self::push_candidate(
                &mut candidates,
                Path::new(&pwd)
                    .join("target")
                    .join("release")
                    .join(self.binary_name()),
            );
        }

        if let Ok(current_dir) = env::current_dir() {
            Self::push_candidate(
                &mut candidates,
                current_dir
                    .join("target")
                    .join("release")
                    .join(self.binary_name()),
            );
        }

        candidates
    }

    fn push_candidate(candidates: &mut Vec<ServerPathCandidate>, candidate: impl Into<PathBuf>) {
        let candidate = candidate.into();
        if candidates.iter().any(|existing| existing.path == candidate) {
            return;
        }
        candidates.push(ServerPathCandidate { path: candidate });
    }

    fn download_server_from_release(&self, repo: &str) -> Result<String> {
        let release = zed::latest_github_release(
            repo,
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;
        let (asset_name, download_type) = self.release_asset_name_and_type()?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| format!("release asset `{asset_name}` was not found in {repo}"))?;

        let download_root = self.download_root_dir();
        fs::create_dir_all(&download_root).map_err(|error| {
            format!(
                "failed to create Codex Companion download directory {}: {error}",
                download_root.display()
            )
        })?;
        let version_dir = download_root.join("downloads").join(format!(
            "{}-{}",
            release.version,
            asset_name_without_archive(&asset_name)
        ));
        let binary_path = version_dir.join(self.binary_name());
        let version_dir_display = version_dir.to_string_lossy().to_string();
        let binary_path_display = binary_path.to_string_lossy().to_string();

        if !fs::metadata(&binary_path).is_ok_and(|metadata| metadata.is_file()) {
            zed::download_file(&asset.download_url, &version_dir_display, download_type)
                .map_err(|error| format!("failed to download Codex Companion server: {error}"))?;
            zed::make_file_executable(&binary_path_display).ok();
        }

        Ok(binary_path_display)
    }

    fn release_asset_name_and_type(&self) -> Result<(String, zed::DownloadedFileType)> {
        let (os, arch) = zed::current_platform();
        let target = match (os, arch) {
            (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
            (zed::Os::Windows, zed::Architecture::Aarch64) => "aarch64-pc-windows-msvc",
            (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            (platform, architecture) => {
                return Err(format!(
                    "unsupported platform for Codex Companion: {platform:?}/{architecture:?}"
                ));
            }
        };

        let extension = match os {
            zed::Os::Windows => "zip",
            _ => "tar.gz",
        };
        let download_type = match os {
            zed::Os::Windows => zed::DownloadedFileType::Zip,
            _ => zed::DownloadedFileType::GzipTar,
        };

        Ok((
            format!("codex-companion-server-{target}.{extension}"),
            download_type,
        ))
    }

    fn binary_name(&self) -> &'static str {
        match zed::current_platform().0 {
            zed::Os::Windows => "codex-companion-server.exe",
            _ => "codex-companion-server",
        }
    }

    fn download_root_dir(&self) -> PathBuf {
        let platform_cache_dir = match zed::current_platform().0 {
            zed::Os::Windows => env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|path| path.join("codex-companion")),
            zed::Os::Mac => env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join("Library").join("Caches").join("codex-companion")),
            zed::Os::Linux => env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|path| path.join(".cache"))
                })
                .map(|path| path.join("codex-companion")),
        };

        platform_cache_dir.unwrap_or_else(|| env::temp_dir().join("codex-companion"))
    }

    fn server_env(&self, settings: &CodexCompanionSettings) -> Vec<(String, String)> {
        let mut env_vars = Vec::new();
        if let Some(server_path) = &settings.server_path {
            env_vars.push((
                "CODEX_COMPANION_SERVER_PATH".to_string(),
                server_path.clone(),
            ));
        }
        if let Some(release_repo) = normalize_non_empty(settings.release_repo.clone()) {
            env_vars.push(("CODEX_COMPANION_RELEASE_REPO".to_string(), release_repo));
        }
        if let Some(cache_dir) = &settings.cache_dir {
            env_vars.push(("CODEX_COMPANION_CACHE_DIR".to_string(), cache_dir.clone()));
        }
        if let Some(ignore_globs) = &settings.ignore_globs {
            if let Ok(encoded) = serde_json::to_string(ignore_globs) {
                env_vars.push(("CODEX_COMPANION_IGNORE_GLOBS_JSON".to_string(), encoded));
            }
        }
        if let Some(max_file_bytes) = settings.max_file_bytes {
            env_vars.push((
                "CODEX_COMPANION_MAX_FILE_BYTES".to_string(),
                max_file_bytes.to_string(),
            ));
        }
        if let Some(max_indexed_files) = settings.max_indexed_files {
            env_vars.push((
                "CODEX_COMPANION_MAX_INDEXED_FILES".to_string(),
                max_indexed_files.to_string(),
            ));
        }
        if let Some(enable_git_tools) = settings.enable_git_tools {
            env_vars.push((
                "CODEX_COMPANION_ENABLE_GIT_TOOLS".to_string(),
                enable_git_tools.to_string(),
            ));
        }
        if let Some(refresh_window_secs) = settings.refresh_window_secs {
            env_vars.push((
                "CODEX_COMPANION_REFRESH_WINDOW_SECS".to_string(),
                refresh_window_secs.to_string(),
            ));
        }
        if let Some(git_cache_ttl_secs) = settings.git_cache_ttl_secs {
            env_vars.push((
                "CODEX_COMPANION_GIT_CACHE_TTL_SECS".to_string(),
                git_cache_ttl_secs.to_string(),
            ));
        }
        if let Some(bundle_cache_ttl_secs) = settings.bundle_cache_ttl_secs {
            env_vars.push((
                "CODEX_COMPANION_BUNDLE_CACHE_TTL_SECS".to_string(),
                bundle_cache_ttl_secs.to_string(),
            ));
        }
        if let Some(prewarm_on_start) = settings.prewarm_on_start {
            env_vars.push((
                "CODEX_COMPANION_PREWARM_ON_START".to_string(),
                prewarm_on_start.to_string(),
            ));
        }
        if let Some(execution_mode) = &settings.execution_mode {
            env_vars.push((
                "CODEX_COMPANION_EXECUTION_MODE".to_string(),
                execution_mode.clone(),
            ));
        }
        if let Some(prefer_full_access) = settings.prefer_full_access {
            env_vars.push((
                "CODEX_COMPANION_PREFER_FULL_ACCESS".to_string(),
                prefer_full_access.to_string(),
            ));
        }
        if let Some(max_parallel_workstreams) = settings.max_parallel_workstreams {
            env_vars.push((
                "CODEX_COMPANION_MAX_PARALLEL_WORKSTREAMS".to_string(),
                max_parallel_workstreams.to_string(),
            ));
        }
        if let Some(skill_roots) = &settings.skill_roots {
            if let Ok(encoded) = serde_json::to_string(skill_roots) {
                env_vars.push(("CODEX_COMPANION_SKILL_ROOTS_JSON".to_string(), encoded));
            }
        }
        if let Some(skill_file_globs) = &settings.skill_file_globs {
            if let Ok(encoded) = serde_json::to_string(skill_file_globs) {
                env_vars.push(("CODEX_COMPANION_SKILL_FILE_GLOBS_JSON".to_string(), encoded));
            }
        }
        if let Some(max_skill_bytes) = settings.max_skill_bytes {
            env_vars.push((
                "CODEX_COMPANION_MAX_SKILL_BYTES".to_string(),
                max_skill_bytes.to_string(),
            ));
        }
        if let Some(skill_cache_ttl_secs) = settings.skill_cache_ttl_secs {
            env_vars.push((
                "CODEX_COMPANION_SKILL_CACHE_TTL_SECS".to_string(),
                skill_cache_ttl_secs.to_string(),
            ));
        }
        if let Some(max_skills_per_query) = settings.max_skills_per_query {
            env_vars.push((
                "CODEX_COMPANION_MAX_SKILLS_PER_QUERY".to_string(),
                max_skills_per_query.to_string(),
            ));
        }
        env_vars
    }

    fn resolve_launch_config(
        &mut self,
        settings: Option<&CodexCompanionSettings>,
    ) -> Result<ResolvedLaunchConfig> {
        let (server_env, explicit_server_path, release_repo) = match settings {
            Some(settings) => (
                self.server_env(settings),
                settings
                    .server_path
                    .clone()
                    .or_else(|| env::var("CODEX_COMPANION_SERVER_PATH").ok()),
                normalize_non_empty(settings.release_repo.clone())
                    .or_else(|| normalize_non_empty(env::var("CODEX_COMPANION_RELEASE_REPO").ok())),
            ),
            None => (
                self.cached_server_env.clone(),
                self.cached_env("CODEX_COMPANION_SERVER_PATH")
                    .or_else(|| env::var("CODEX_COMPANION_SERVER_PATH").ok()),
                self.cached_release_repo.clone().or_else(|| {
                    self.cached_env("CODEX_COMPANION_RELEASE_REPO").or_else(|| {
                        normalize_non_empty(env::var("CODEX_COMPANION_RELEASE_REPO").ok())
                    })
                }),
            ),
        };
        let server_path = self.resolve_server_path(explicit_server_path, release_repo.clone())?;

        if settings.is_some() {
            self.cached_server_env = server_env.clone();
            self.cached_release_repo = release_repo;
        }

        Ok(ResolvedLaunchConfig {
            server_path,
            server_env,
        })
    }

    fn run_cli(
        &mut self,
        cli_args: &[String],
        settings: Option<&CodexCompanionSettings>,
    ) -> std::result::Result<String, String> {
        let launch = self.resolve_launch_config(settings)?;
        let mut command = ProcessCommand::new(launch.server_path).envs(launch.server_env);
        for arg in cli_args {
            command = command.arg(arg);
        }

        let output = command.output().map_err(|error| error.to_string())?;
        if output.status != Some(0) {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(if stderr.is_empty() { stdout } else { stderr });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn slash_output(&self, label: &str, text: String) -> SlashCommandOutput {
        SlashCommandOutput {
            sections: vec![SlashCommandOutputSection {
                range: (0..text.len()).into(),
                label: label.to_string(),
            }],
            text,
        }
    }

    fn cached_env(&self, key: &str) -> Option<String> {
        self.cached_server_env
            .iter()
            .rev()
            .find_map(|(candidate, value)| (candidate == key).then(|| value.clone()))
    }
}

impl zed::Extension for CodexCompanionExtension {
    fn new() -> Self {
        Self {
            cached_server_path: None,
            cached_server_env: Vec::new(),
            cached_release_repo: None,
        }
    }

    fn context_server_command(
        &mut self,
        _context_server_id: &ContextServerId,
        project: &Project,
    ) -> Result<Command> {
        let settings = self.load_settings(project)?;
        let launch = self.resolve_launch_config(Some(&settings))?;

        Ok(Command {
            command: launch.server_path,
            args: vec![],
            env: launch.server_env,
        })
    }

    fn context_server_configuration(
        &mut self,
        _context_server_id: &ContextServerId,
        _project: &Project,
    ) -> Result<Option<ContextServerConfiguration>> {
        let installation_instructions =
            include_str!("../configuration/installation_instructions.md").to_string();
        let default_settings = include_str!("../configuration/default_settings.jsonc").to_string();
        let settings_schema = serde_json::to_string(&schemars::schema_for!(CodexCompanionSettings))
            .map_err(|error| error.to_string())?;

        Ok(Some(ContextServerConfiguration {
            installation_instructions,
            default_settings,
            settings_schema,
        }))
    }

    fn run_slash_command(
        &self,
        command: SlashCommand,
        args: Vec<String>,
        worktree: Option<&Worktree>,
    ) -> std::result::Result<SlashCommandOutput, String> {
        let worktree = worktree.ok_or_else(|| {
            "Codex Companion slash commands require a project worktree.".to_string()
        })?;
        let worktree_settings = self.load_worktree_settings(worktree);
        let root = worktree.root_path();
        let mut extension = CodexCompanionExtension {
            cached_server_path: self.cached_server_path.clone(),
            cached_server_env: self.cached_server_env.clone(),
            cached_release_repo: self.cached_release_repo.clone(),
        };

        match command.name.as_str() {
            "codex-context" => {
                if args.is_empty() {
                    return Err("`/codex-context` requires a task or query.".to_string());
                }
                let query = args.join(" ");
                let text = extension.run_cli(
                    &[
                        "bundle".to_string(),
                        "--root".to_string(),
                        root.clone(),
                        "--query".to_string(),
                        query,
                    ],
                    worktree_settings.as_ref(),
                )?;
                Ok(extension.slash_output("Codex Context", text))
            }
            "codex-memory" => {
                let mut cli_args = vec!["memory".to_string(), "--root".to_string(), root.clone()];
                if !args.is_empty() {
                    cli_args.push("--query".to_string());
                    cli_args.push(args.join(" "));
                }
                let text = extension.run_cli(&cli_args, worktree_settings.as_ref())?;
                Ok(extension.slash_output("Codex Memory", text))
            }
            "codex-cache" => {
                let text = extension.run_cli(
                    &["status".to_string(), "--root".to_string(), root.clone()],
                    worktree_settings.as_ref(),
                )?;
                Ok(extension.slash_output("Codex Cache", text))
            }
            "codex-refresh" => {
                let text = extension.run_cli(
                    &["index".to_string(), "--root".to_string(), root],
                    worktree_settings.as_ref(),
                )?;
                Ok(extension.slash_output("Codex Refresh", text))
            }
            "codex-warm" => {
                let text = extension.run_cli(
                    &["warm".to_string(), "--root".to_string(), root],
                    worktree_settings.as_ref(),
                )?;
                Ok(extension.slash_output("Codex Warmup", text))
            }
            "codex-plan" => {
                if args.is_empty() {
                    return Err("`/codex-plan` requires a task or query.".to_string());
                }
                let query = args.join(" ");
                let text = extension.run_cli(
                    &[
                        "plan".to_string(),
                        "--root".to_string(),
                        root.clone(),
                        "--query".to_string(),
                        query,
                    ],
                    worktree_settings.as_ref(),
                )?;
                Ok(extension.slash_output("Codex Plan", text))
            }
            "codex-orchestrate" => {
                if args.is_empty() {
                    return Err("`/codex-orchestrate` requires a task or query.".to_string());
                }
                let query = args.join(" ");
                let text = extension.run_cli(
                    &[
                        "orchestrate".to_string(),
                        "--root".to_string(),
                        root.clone(),
                        "--query".to_string(),
                        query,
                    ],
                    worktree_settings.as_ref(),
                )?;
                Ok(extension.slash_output("Codex Orchestration", text))
            }
            "codex-skills" => {
                let mut cli_args = vec!["skills".to_string(), "--root".to_string(), root.clone()];
                if !args.is_empty() {
                    cli_args.push("--query".to_string());
                    cli_args.push(args.join(" "));
                }
                let text = extension.run_cli(&cli_args, worktree_settings.as_ref())?;
                Ok(extension.slash_output("Codex Skills", text))
            }
            other => Err(format!("unknown Codex Companion slash command: {other}")),
        }
    }
}

fn asset_name_without_archive(asset_name: &str) -> String {
    asset_name
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".zip")
        .to_string()
}

fn normalize_non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_release_repo() -> Option<String> {
    EXTENSION_MANIFEST
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix("repository = ")
                .map(str::trim)
                .and_then(|value| value.strip_prefix('"'))
                .and_then(|value| value.strip_suffix('"'))
        })
        .and_then(github_repo_from_url)
}

fn github_repo_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let without_host = without_scheme.strip_prefix("github.com/")?;
    let without_git = without_host.trim_end_matches(".git");
    let mut parts = without_git.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();

    if owner.is_empty() || repo.is_empty() || owner.eq_ignore_ascii_case("example") {
        return None;
    }

    Some(format!("{owner}/{repo}"))
}

fn parse_worktree_settings(content: &str) -> Option<CodexCompanionSettings> {
    let sanitized = strip_trailing_commas(&strip_json_comments(content));
    let parsed = serde_json::from_str::<WorktreeSettingsFile>(&sanitized).ok()?;
    parsed
        .context_servers
        .get(CONTEXT_SERVER_ID)
        .and_then(|entry| entry.settings.clone())
}

fn strip_json_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if matches!(next, '\n' | '\r') {
                            output.push(next);
                            if next == '\r' && chars.peek() == Some(&'\n') {
                                output.push(chars.next().unwrap_or('\n'));
                            }
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    while let Some(next) = chars.next() {
                        if matches!(next, '\n' | '\r') {
                            output.push(next);
                            if next == '\r' && chars.peek() == Some(&'\n') {
                                output.push(chars.next().unwrap_or('\n'));
                            }
                        }
                        if previous == '*' && next == '/' {
                            break;
                        }
                        previous = next;
                    }
                    continue;
                }
                _ => {}
            }
        }

        output.push(ch);
    }

    output
}

fn strip_trailing_commas(input: &str) -> String {
    let chars = input.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            index += 1;
            continue;
        }

        if ch == ',' {
            let mut lookahead = index + 1;
            while lookahead < chars.len() && chars[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if lookahead < chars.len() && matches!(chars[lookahead], '}' | ']') {
                index += 1;
                continue;
            }
        }

        output.push(ch);
        index += 1;
    }

    output
}

zed::register_extension!(CodexCompanionExtension);

#[cfg(test)]
mod tests {
    use super::{
        github_repo_from_url, normalize_non_empty, parse_worktree_settings, strip_json_comments,
        strip_trailing_commas,
    };

    #[test]
    fn parse_worktree_settings_supports_jsonc_comments() {
        let settings = parse_worktree_settings(
            r#"{
                // comment
                "context_servers": {
                    "codex-companion": {
                        "settings": {
                            "release_repo": "owner/repo",
                            "max_skills_per_query": 6
                        }
                    }
                }
            }"#,
        )
        .expect("settings should parse");

        assert_eq!(settings.release_repo.as_deref(), Some("owner/repo"));
        assert_eq!(settings.max_skills_per_query, Some(6));
    }

    #[test]
    fn strip_json_comments_preserves_urls_inside_strings() {
        let stripped = strip_json_comments(
            r#"{
                "url": "https://example.com//releases",
                // comment
                "enabled": true
            }"#,
        );

        assert!(stripped.contains("https://example.com//releases"));
        assert!(stripped.contains("\"enabled\": true"));
    }

    #[test]
    fn strip_trailing_commas_preserves_commas_inside_strings() {
        let stripped = strip_trailing_commas(
            r#"{
                "title": "cache, memory, and search",
                "items": [
                    "one",
                    "two",
                ],
            }"#,
        );

        assert!(stripped.contains("cache, memory, and search"));
        assert!(!stripped.contains(",]"));
        assert!(!stripped.contains(",}"));
    }

    #[test]
    fn parse_worktree_settings_supports_jsonc_trailing_commas() {
        let settings = parse_worktree_settings(
            r#"{
                "context_servers": {
                    "codex-companion": {
                        "settings": {
                            "skill_roots": [
                                "D:\\downloads\\agency-agents",
                            ],
                            "max_skills_per_query": 6,
                        },
                    },
                },
            }"#,
        )
        .expect("settings should parse");

        assert_eq!(settings.max_skills_per_query, Some(6));
        assert_eq!(
            settings.skill_roots,
            Some(vec![r"D:\downloads\agency-agents".to_string()])
        );
    }

    #[test]
    fn normalize_non_empty_trims_values() {
        assert_eq!(
            normalize_non_empty(Some("  owner/repo  ".to_string())).as_deref(),
            Some("owner/repo")
        );
        assert_eq!(normalize_non_empty(Some("   ".to_string())), None);
    }

    #[test]
    fn github_repo_from_url_accepts_github_urls() {
        assert_eq!(
            github_repo_from_url("https://github.com/openai/zed-codex").as_deref(),
            Some("openai/zed-codex")
        );
        assert_eq!(
            github_repo_from_url("https://github.com/openai/zed-codex.git").as_deref(),
            Some("openai/zed-codex")
        );
    }

    #[test]
    fn github_repo_from_url_rejects_non_github_or_placeholder_urls() {
        assert_eq!(
            github_repo_from_url("https://gitlab.com/openai/zed-codex"),
            None
        );
        assert_eq!(
            github_repo_from_url("https://github.com/example/codex-companion"),
            None
        );
    }
}
