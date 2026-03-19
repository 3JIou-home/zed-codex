use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    rc::Rc,
};

use agent_client_protocol::{self as acp, Agent as _};
use anyhow::{anyhow, bail, Context, Result};
use tokio::process::{Child, Command};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

const ENV_CODEX_ACP_BIN: &str = "CODEX_COMPANION_CODEX_ACP_BIN";
const ENV_CODEX_BIN_LEGACY: &str = "CODEX_COMPANION_CODEX_BIN";

pub struct CodexSessionBridge {
    _workspace_root: PathBuf,
    upstream_session_id: acp::SessionId,
    connection: Rc<acp::ClientSideConnection>,
    process: Option<Child>,
}

impl CodexSessionBridge {
    pub async fn connect(
        workspace_root: &Path,
        outer_session_id: &acp::SessionId,
        downstream_client: Rc<acp::AgentSideConnection>,
        downstream_initialize: Option<acp::InitializeRequest>,
        mcp_servers: Vec<acp::McpServer>,
    ) -> Result<Self> {
        let executable = resolve_codex_acp_executable()?;
        let mut command = executable.command();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        tracing::info!(
            "starting upstream Codex ACP backend using {}",
            executable.description
        );
        let mut process = command.spawn().with_context(|| {
            format!(
                "failed to start upstream Codex ACP backend using {}",
                executable.description
            )
        })?;

        let outgoing = process
            .stdin
            .take()
            .context("upstream Codex ACP backend did not expose stdin")?
            .compat_write();
        let incoming = process
            .stdout
            .take()
            .context("upstream Codex ACP backend did not expose stdout")?
            .compat();
        let proxy_client = UpstreamClientProxy {
            outer_session_id: outer_session_id.clone(),
            downstream_client,
        };
        let (connection, handle_io) =
            acp::ClientSideConnection::new(proxy_client, outgoing, incoming, |fut| {
                tokio::task::spawn_local(fut);
            });
        let connection = Rc::new(connection);
        tokio::task::spawn_local(async move {
            if let Err(error) = handle_io.await {
                tracing::warn!("upstream Codex ACP transport stopped: {error}");
            }
        });

        let initialize_request = upstream_initialize_request(downstream_initialize);
        let _initialize_response = connection
            .initialize(initialize_request)
            .await
            .map_err(acp_error)?;

        let session_response = connection
            .new_session(
                acp::NewSessionRequest::new(workspace_root.to_path_buf()).mcp_servers(mcp_servers),
            )
            .await
            .map_err(acp_error)?;

        Ok(Self {
            _workspace_root: workspace_root.to_path_buf(),
            upstream_session_id: session_response.session_id,
            connection,
            process: Some(process),
        })
    }

    pub async fn run_turn(&self, prompt: &str) -> Result<acp::StopReason> {
        let response = self
            .connection
            .prompt(acp::PromptRequest::new(
                self.upstream_session_id.clone(),
                vec![acp::ContentBlock::from(prompt.to_string())],
            ))
            .await
            .map_err(acp_error)?;
        Ok(response.stop_reason)
    }

    pub async fn cancel(&self) -> Result<()> {
        self.connection
            .cancel(acp::CancelNotification::new(
                self.upstream_session_id.clone(),
            ))
            .await
            .map_err(acp_error)
    }
}

impl Drop for CodexSessionBridge {
    fn drop(&mut self) {
        if let Some(process) = &mut self.process {
            let _ = process.start_kill();
        }
    }
}

#[derive(Clone)]
struct UpstreamClientProxy {
    outer_session_id: acp::SessionId,
    downstream_client: Rc<acp::AgentSideConnection>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for UpstreamClientProxy {
    async fn request_permission(
        &self,
        mut args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.request_permission(args).await
    }

    async fn session_notification(&self, mut args: acp::SessionNotification) -> acp::Result<()> {
        let Some(update) = forwarded_session_update(args.update) else {
            return Ok(());
        };
        args.session_id = self.outer_session_id.clone();
        args.update = update;
        self.downstream_client.session_notification(args).await
    }

    async fn write_text_file(
        &self,
        mut args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.write_text_file(args).await
    }

    async fn read_text_file(
        &self,
        mut args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.read_text_file(args).await
    }

    async fn create_terminal(
        &self,
        mut args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.create_terminal(args).await
    }

    async fn terminal_output(
        &self,
        mut args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.terminal_output(args).await
    }

    async fn release_terminal(
        &self,
        mut args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.release_terminal(args).await
    }

    async fn wait_for_terminal_exit(
        &self,
        mut args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.wait_for_terminal_exit(args).await
    }

    async fn kill_terminal(
        &self,
        mut args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        args.session_id = self.outer_session_id.clone();
        self.downstream_client.kill_terminal(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        self.downstream_client.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        self.downstream_client.ext_notification(args).await
    }
}

struct CodexExecutable {
    program: OsString,
    description: String,
}

impl CodexExecutable {
    fn command(&self) -> Command {
        Command::new(&self.program)
    }
}

fn resolve_codex_acp_executable() -> Result<CodexExecutable> {
    if let Some(program) = read_nonempty_env(ENV_CODEX_ACP_BIN) {
        return Ok(CodexExecutable {
            program: OsString::from(program.clone()),
            description: program,
        });
    }

    if let Some(program) = read_nonempty_env(ENV_CODEX_BIN_LEGACY) {
        return Ok(CodexExecutable {
            program: OsString::from(program.clone()),
            description: program,
        });
    }

    if command_exists("codex-acp") {
        return Ok(CodexExecutable {
            program: OsString::from("codex-acp"),
            description: "codex-acp".to_string(),
        });
    }

    if let Some(path) = installed_zed_codex_acp_binary() {
        let description = path.display().to_string();
        return Ok(CodexExecutable {
            program: path.into_os_string(),
            description,
        });
    }

    bail!(
        "could not find an upstream Codex ACP binary; set `{ENV_CODEX_ACP_BIN}` or install the Codex ACP agent in Zed"
    );
}

fn upstream_initialize_request(
    downstream_initialize: Option<acp::InitializeRequest>,
) -> acp::InitializeRequest {
    let mut request = downstream_initialize
        .unwrap_or_else(|| acp::InitializeRequest::new(acp::ProtocolVersion::V1));
    request.client_info = Some(
        acp::Implementation::new("codex-companion-acp-agent", env!("CARGO_PKG_VERSION"))
            .title("Codex Companion ACP Agent"),
    );
    request
}

fn forwarded_session_update(update: acp::SessionUpdate) -> Option<acp::SessionUpdate> {
    match update {
        acp::SessionUpdate::UserMessageChunk(_) => None,
        acp::SessionUpdate::AvailableCommandsUpdate(_) => None,
        acp::SessionUpdate::CurrentModeUpdate(_) => None,
        acp::SessionUpdate::ConfigOptionUpdate(_) => None,
        other => Some(other),
    }
}

fn installed_zed_codex_acp_binary() -> Option<PathBuf> {
    let local_app_data = env::var_os("LOCALAPPDATA")?;
    let registry_root = PathBuf::from(local_app_data)
        .join("Zed")
        .join("external_agents")
        .join("registry")
        .join("codex-acp");
    latest_registry_binary(&registry_root)
}

fn latest_registry_binary(registry_root: &Path) -> Option<PathBuf> {
    let binary_name = if cfg!(windows) {
        "codex-acp.exe"
    } else {
        "codex-acp"
    };

    let mut candidates = fs::read_dir(registry_root)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path().join(binary_name)))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.pop()
}

fn command_exists(command: &str) -> bool {
    let candidate = Path::new(command);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return candidate.is_file();
    }

    let search_path = env::var_os("PATH").unwrap_or_default();
    let path_exts = if cfg!(windows) {
        env::var_os("PATHEXT")
            .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"))
            .to_string_lossy()
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| ext.to_string())
            .collect::<Vec<_>>()
    } else {
        vec![String::new()]
    };

    env::split_paths(&search_path).any(|directory| {
        path_exts.iter().any(|ext| {
            let file_name = if ext.is_empty()
                || command
                    .to_ascii_uppercase()
                    .ends_with(&ext.to_ascii_uppercase())
            {
                command.to_string()
            } else {
                format!("{command}{ext}")
            };
            directory.join(file_name).is_file()
        })
    })
}

fn read_nonempty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn acp_error(error: acp::Error) -> anyhow::Error {
    anyhow!(error.to_string())
}

#[cfg(test)]
mod tests;
