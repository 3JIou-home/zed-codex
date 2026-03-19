use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
};

use agent_client_protocol::{self as acp, Client as _};
use chrono::Utc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::{
    codex_bridge::CodexSessionBridge, format_cache_status, format_context_bundle,
    format_memory_results, format_skill_results, format_task_decomposition,
    format_task_orchestration, format_warmup_status, AppState, ContextBundle, MemorySearchResults,
    ServerConfig, TaskDecomposition, TaskOrchestration,
};

const DEFAULT_RESULT_LIMIT: usize = 6;
const DEFAULT_MEMORY_LIMIT: usize = 4;
const DEFAULT_MEMORY_RECALL_LIMIT: usize = 8;

const MODE_AUTO: &str = "auto";
const MODE_CONTEXT: &str = "context";
const MODE_PLAN: &str = "plan";
const MODE_ORCHESTRATE: &str = "orchestrate";

#[derive(Clone)]
struct AgentSession {
    root: PathBuf,
    state: AppState,
    mode: acp::SessionModeId,
    mcp_servers: Vec<acp::McpServer>,
    codex: Rc<Mutex<Option<Rc<CodexSessionBridge>>>>,
}

#[derive(Clone, Copy)]
struct AutoArtifacts<'a> {
    fallback_text: &'a str,
    index: &'a crate::model::WorkspaceIndex,
    bundle: &'a ContextBundle,
    decomposition: &'a TaskDecomposition,
    orchestration: &'a TaskOrchestration,
    analysis_task: bool,
}

struct CodexRenderConfig {
    max_context_chars: usize,
    max_file_excerpt_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptAction {
    Auto(String),
    Help,
    Warm,
    Status,
    Context(String),
    Plan(String),
    Orchestrate(String),
    Skills(Option<String>),
    Memory(Option<String>),
}

impl PromptAction {
    fn summary(&self) -> &'static str {
        match self {
            Self::Auto(_) => "running the automatic multi-stage companion pipeline",
            Self::Help => "showing available companion commands and modes",
            Self::Warm => "warming workspace cache, git, memory, and skills",
            Self::Status => "reading the current cache and index status",
            Self::Context(_) => "building a cached task-focused context bundle",
            Self::Plan(_) => "decomposing the task into scoped workstreams",
            Self::Orchestrate(_) => "running full companion orchestration",
            Self::Skills(_) => "searching external skill libraries",
            Self::Memory(_) => "recalling durable project memory",
        }
    }
}

struct PromptExecution {
    text: String,
    plan: Option<acp::Plan>,
    stop_reason: acp::StopReason,
    title: String,
}

struct FlattenedPrompt {
    user_text: String,
}

#[derive(Clone, Default)]
struct DownstreamClientHandle {
    connection: Rc<RefCell<Option<Rc<acp::AgentSideConnection>>>>,
}

impl DownstreamClientHandle {
    fn set(&self, connection: Rc<acp::AgentSideConnection>) {
        self.connection.replace(Some(connection));
    }

    fn get(&self) -> anyhow::Result<Rc<acp::AgentSideConnection>> {
        self.connection
            .borrow()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("downstream ACP client connection is not ready"))
    }
}

struct CompanionAgent {
    config: ServerConfig,
    next_session_id: Cell<u64>,
    sessions: Mutex<HashMap<String, AgentSession>>,
    cancelled_sessions: Mutex<HashSet<String>>,
    downstream_client: DownstreamClientHandle,
    initialize_request: Mutex<Option<acp::InitializeRequest>>,
    session_update_tx: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
}

impl CompanionAgent {
    fn new(
        config: ServerConfig,
        downstream_client: DownstreamClientHandle,
        session_update_tx: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
    ) -> Self {
        Self {
            config,
            next_session_id: Cell::new(1),
            sessions: Mutex::new(HashMap::new()),
            cancelled_sessions: Mutex::new(HashSet::new()),
            downstream_client,
            initialize_request: Mutex::new(None),
            session_update_tx,
        }
    }

    fn next_session_id(&self) -> acp::SessionId {
        let next = self.next_session_id.get();
        self.next_session_id.set(next + 1);
        acp::SessionId::new(format!("codex-companion-{next}"))
    }

    async fn store_session(&self, session_id: &acp::SessionId, session: AgentSession) {
        self.sessions
            .lock()
            .await
            .insert(session_key(session_id), session);
    }

    async fn load_session(&self, session_id: &acp::SessionId) -> Result<AgentSession, acp::Error> {
        self.sessions
            .lock()
            .await
            .get(&session_key(session_id))
            .cloned()
            .ok_or_else(|| acp::Error::new(-32602, "unknown session"))
    }

    async fn set_mode(
        &self,
        session_id: &acp::SessionId,
        mode: acp::SessionModeId,
    ) -> Result<(), acp::Error> {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_key(session_id)) else {
            return Err(acp::Error::new(-32602, "unknown session"));
        };
        session.mode = mode;
        Ok(())
    }

    async fn mark_cancelled(&self, session_id: &acp::SessionId) {
        self.cancelled_sessions
            .lock()
            .await
            .insert(session_key(session_id));
    }

    async fn take_cancelled(&self, session_id: &acp::SessionId) -> bool {
        self.cancelled_sessions
            .lock()
            .await
            .remove(&session_key(session_id))
    }

    async fn send_update(
        &self,
        session_id: &acp::SessionId,
        update: acp::SessionUpdate,
    ) -> Result<(), acp::Error> {
        let (tx, rx) = oneshot::channel();
        self.session_update_tx
            .send((
                acp::SessionNotification::new(session_id.clone(), update),
                tx,
            ))
            .map_err(|_| acp::Error::internal_error())?;
        rx.await.map_err(|_| acp::Error::internal_error())
    }

    async fn send_text_message(
        &self,
        session_id: &acp::SessionId,
        text: impl Into<String>,
    ) -> Result<(), acp::Error> {
        self.send_update(
            session_id,
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::from(
                text.into(),
            ))),
        )
        .await
    }

    async fn send_thought(
        &self,
        session_id: &acp::SessionId,
        text: impl Into<String>,
    ) -> Result<(), acp::Error> {
        self.send_update(
            session_id,
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::from(
                text.into(),
            ))),
        )
        .await
    }

    async fn send_session_info(
        &self,
        session_id: &acp::SessionId,
        title: impl Into<String>,
    ) -> Result<(), acp::Error> {
        self.send_update(
            session_id,
            acp::SessionUpdate::SessionInfoUpdate(
                acp::SessionInfoUpdate::new()
                    .title(title.into())
                    .updated_at(Utc::now().to_rfc3339()),
            ),
        )
        .await
    }

    async fn send_available_commands(&self, session_id: &acp::SessionId) -> Result<(), acp::Error> {
        self.send_update(
            session_id,
            acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                available_commands(),
            )),
        )
        .await
    }

    async fn run_action(
        &self,
        session_id: &acp::SessionId,
        session: &AgentSession,
        action: PromptAction,
        flattened: &FlattenedPrompt,
    ) -> Result<PromptExecution, acp::Error> {
        let title = session_title(&session.root, &flattened.user_text, Some(&action));
        let state = &session.state;

        match action {
            PromptAction::Auto(query) => {
                self.send_thought(session_id, "Stage 1/4: building cached workspace context.")
                    .await?;
                let bundle = state
                    .build_context_bundle(
                        query.clone(),
                        DEFAULT_RESULT_LIMIT,
                        DEFAULT_MEMORY_LIMIT,
                        false,
                    )
                    .await
                    .map_err(internal_error)?;

                if self.take_cancelled(session_id).await {
                    return Ok(PromptExecution {
                        text: String::new(),
                        plan: None,
                        stop_reason: acp::StopReason::Cancelled,
                        title,
                    });
                }

                self.send_thought(
                    session_id,
                    "Stage 2/4: decomposing the task into workstreams.",
                )
                .await?;
                let decomposition = state
                    .decompose_task(
                        query.clone(),
                        DEFAULT_RESULT_LIMIT,
                        DEFAULT_MEMORY_LIMIT,
                        false,
                    )
                    .await
                    .map_err(internal_error)?;

                if self.take_cancelled(session_id).await {
                    return Ok(PromptExecution {
                        text: String::new(),
                        plan: None,
                        stop_reason: acp::StopReason::Cancelled,
                        title,
                    });
                }

                self.send_thought(
                    session_id,
                    "Stage 3/4: assembling orchestration and companion notes.",
                )
                .await?;
                let orchestration = state
                    .orchestrate_task(query, DEFAULT_RESULT_LIMIT, DEFAULT_MEMORY_LIMIT, false)
                    .await
                    .map_err(internal_error)?;
                let index = state.ensure_index(false).await.map_err(internal_error)?;
                let analysis_task = is_analysis_task(&orchestration.task);
                let fallback_text = if analysis_task {
                    format_auto_analysis(index.as_ref(), &bundle, &decomposition, &orchestration)
                } else {
                    format_auto_execution(&bundle, &decomposition, &orchestration)
                };
                let artifacts = AutoArtifacts {
                    fallback_text: &fallback_text,
                    index: index.as_ref(),
                    bundle: &bundle,
                    decomposition: &decomposition,
                    orchestration: &orchestration,
                    analysis_task,
                };
                let (text, stop_reason) = match self
                    .codex_response(session_id, session, &flattened.user_text, artifacts)
                    .await
                {
                    Ok(stop_reason) => (String::new(), stop_reason),
                    Err(error) => {
                        tracing::warn!("falling back to deterministic ACP output: {error}");
                        (
                            format!("{}\n\n{}", codex_fallback_notice(&error), fallback_text),
                            acp::StopReason::EndTurn,
                        )
                    }
                };

                Ok(PromptExecution {
                    plan: Some(complete_plan(plan_from_orchestration(&orchestration))),
                    text,
                    stop_reason,
                    title,
                })
            }
            PromptAction::Help => Ok(PromptExecution {
                text: help_text(&session.mode),
                plan: None,
                stop_reason: acp::StopReason::EndTurn,
                title,
            }),
            PromptAction::Warm => {
                let warmup = state.warmup(false).await.map_err(internal_error)?;
                Ok(PromptExecution {
                    text: format_warmup_status(&warmup),
                    plan: None,
                    stop_reason: acp::StopReason::EndTurn,
                    title,
                })
            }
            PromptAction::Status => {
                let status = state.cache_status(false).await.map_err(internal_error)?;
                Ok(PromptExecution {
                    text: format_cache_status(&status),
                    plan: None,
                    stop_reason: acp::StopReason::EndTurn,
                    title,
                })
            }
            PromptAction::Context(query) => {
                let bundle = state
                    .build_context_bundle(query, DEFAULT_RESULT_LIMIT, DEFAULT_MEMORY_LIMIT, false)
                    .await
                    .map_err(internal_error)?;
                Ok(PromptExecution {
                    plan: Some(plan_from_context_bundle(&bundle)),
                    text: format_context_bundle(&bundle),
                    stop_reason: acp::StopReason::EndTurn,
                    title,
                })
            }
            PromptAction::Plan(query) => {
                let decomposition = state
                    .decompose_task(query, DEFAULT_RESULT_LIMIT, DEFAULT_MEMORY_LIMIT, false)
                    .await
                    .map_err(internal_error)?;
                Ok(PromptExecution {
                    plan: Some(complete_plan(plan_from_decomposition(&decomposition))),
                    text: format_task_decomposition(&decomposition),
                    stop_reason: acp::StopReason::EndTurn,
                    title,
                })
            }
            PromptAction::Orchestrate(query) => {
                let orchestration = state
                    .orchestrate_task(query, DEFAULT_RESULT_LIMIT, DEFAULT_MEMORY_LIMIT, false)
                    .await
                    .map_err(internal_error)?;
                Ok(PromptExecution {
                    plan: Some(complete_plan(plan_from_orchestration(&orchestration))),
                    text: format_task_orchestration(&orchestration),
                    stop_reason: acp::StopReason::EndTurn,
                    title,
                })
            }
            PromptAction::Skills(query) => {
                let results = state
                    .search_skills(query, state.config.max_skills_per_query, false)
                    .await
                    .map_err(internal_error)?;
                Ok(PromptExecution {
                    text: format_skill_results(&results),
                    plan: None,
                    stop_reason: acp::StopReason::EndTurn,
                    title,
                })
            }
            PromptAction::Memory(query) => {
                let results = MemorySearchResults {
                    query: query.clone(),
                    matches: state
                        .recall(query, Vec::new(), DEFAULT_MEMORY_RECALL_LIMIT)
                        .await
                        .map_err(internal_error)?,
                };
                Ok(PromptExecution {
                    text: format_memory_results(&results),
                    plan: None,
                    stop_reason: acp::StopReason::EndTurn,
                    title,
                })
            }
        }
    }
}

impl CompanionAgent {
    async fn codex_response(
        &self,
        session_id: &acp::SessionId,
        session: &AgentSession,
        user_prompt: &str,
        artifacts: AutoArtifacts<'_>,
    ) -> anyhow::Result<acp::StopReason> {
        self.send_thought(
            session_id,
            "Stage 4/4: handing prepared workspace context to the installed Codex ACP backend.",
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let render_config = CodexRenderConfig::from_env();
        let prepared_prompt = render_codex_turn_input(user_prompt, artifacts, &render_config);

        let bridge = {
            let mut codex = session.codex.lock().await;
            if codex.is_none() {
                *codex = Some(Rc::new(
                    CodexSessionBridge::connect(
                        &session.root,
                        session_id,
                        self.downstream_client.get()?,
                        self.initialize_request.lock().await.clone(),
                        session.mcp_servers.clone(),
                    )
                    .await?,
                ));
            }

            codex
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Codex ACP backend was not initialized"))?
        };
        let response = bridge.run_turn(&prepared_prompt).await;
        if response.is_err() {
            let mut codex = session.codex.lock().await;
            *codex = None;
        }
        response
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for CompanionAgent {
    async fn initialize(
        &self,
        arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        *self.initialize_request.lock().await = Some(arguments);
        Ok(acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .agent_capabilities(
                acp::AgentCapabilities::new()
                    .prompt_capabilities(acp::PromptCapabilities::new().embedded_context(true))
                    .session_capabilities(acp::SessionCapabilities::new()),
            )
            .agent_info(
                acp::Implementation::new("codex-companion-acp-agent", env!("CARGO_PKG_VERSION"))
                    .title("Codex Companion ACP Agent"),
            ))
    }

    async fn authenticate(
        &self,
        _arguments: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        arguments: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let session_id = self.next_session_id();
        let state =
            AppState::new(arguments.cwd.clone(), self.config.clone()).map_err(internal_error)?;
        let session = AgentSession {
            root: arguments.cwd.clone(),
            state: state.clone(),
            mode: acp::SessionModeId::new(MODE_AUTO),
            mcp_servers: arguments.mcp_servers.clone(),
            codex: Rc::new(Mutex::new(None)),
        };
        self.store_session(&session_id, session).await;

        if state.should_prewarm_on_start() {
            let warm_state = state.clone();
            tokio::task::spawn_local(async move {
                let _ = warm_state.warmup(false).await;
            });
        }

        self.send_session_info(
            &session_id,
            session_title(&arguments.cwd, "", Some(&PromptAction::Help)),
        )
        .await?;
        self.send_available_commands(&session_id).await?;

        Ok(acp::NewSessionResponse::new(session_id)
            .modes(acp::SessionModeState::new(MODE_AUTO, available_modes())))
    }

    async fn prompt(
        &self,
        arguments: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let session = self.load_session(&arguments.session_id).await?;
        let flattened = flatten_prompt(&arguments.prompt);
        let action = parse_prompt_action(&flattened.user_text, &session.mode);

        self.send_thought(
            &arguments.session_id,
            format!(
                "Companion mode `{}`: {}.",
                session.mode.0.as_ref(),
                action.summary()
            ),
        )
        .await?;

        if let Some(initial_plan) = initial_plan_for_action(&action) {
            self.send_update(
                &arguments.session_id,
                acp::SessionUpdate::Plan(initial_plan),
            )
            .await?;
        }

        if self.take_cancelled(&arguments.session_id).await {
            return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
        }

        let execution = self
            .run_action(&arguments.session_id, &session, action, &flattened)
            .await?;

        if self.take_cancelled(&arguments.session_id).await {
            return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
        }

        self.send_session_info(&arguments.session_id, execution.title)
            .await?;

        if let Some(plan) = execution.plan {
            self.send_update(&arguments.session_id, acp::SessionUpdate::Plan(plan))
                .await?;
        }

        if !execution.text.is_empty() {
            self.send_text_message(&arguments.session_id, execution.text)
                .await?;
        }

        Ok(acp::PromptResponse::new(execution.stop_reason))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        self.mark_cancelled(&args.session_id).await;
        if let Ok(session) = self.load_session(&args.session_id).await {
            let bridge = {
                let codex = session.codex.lock().await;
                codex.clone()
            };
            if let Some(bridge) = bridge {
                if let Err(error) = bridge.cancel().await {
                    tracing::warn!(
                        "failed to forward cancel to upstream Codex ACP backend: {error}"
                    );
                }
            }
        }
        Ok(())
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        if !available_modes().iter().any(|mode| mode.id == args.mode_id) {
            return Err(acp::Error::new(-32602, "unsupported session mode"));
        }

        self.set_mode(&args.session_id, args.mode_id.clone())
            .await?;
        self.send_update(
            &args.session_id,
            acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(args.mode_id)),
        )
        .await?;
        Ok(acp::SetSessionModeResponse::default())
    }
}

pub async fn serve_stdio(config: ServerConfig) -> acp::Result<()> {
    let outgoing = tokio::io::stdout().compat_write();
    let incoming = tokio::io::stdin().compat();

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let downstream_client = DownstreamClientHandle::default();
            let (conn, handle_io) = acp::AgentSideConnection::new(
                CompanionAgent::new(config, downstream_client.clone(), tx),
                outgoing,
                incoming,
                |fut| {
                    tokio::task::spawn_local(fut);
                },
            );
            let conn = Rc::new(conn);
            downstream_client.set(conn.clone());

            tokio::task::spawn_local(async move {
                while let Some((session_notification, ack)) = rx.recv().await {
                    let result = conn.session_notification(session_notification).await;
                    if let Err(error) = result {
                        tracing::error!("failed to deliver ACP session notification: {error}");
                        break;
                    }
                    let _ = ack.send(());
                }
            });

            handle_io.await
        })
        .await
}

fn available_modes() -> Vec<acp::SessionMode> {
    vec![
        acp::SessionMode::new(MODE_AUTO, "Auto").description(
            "Run the internal companion pipeline automatically, then hand the prepared context to an upstream Codex ACP backend when available.",
        ),
        acp::SessionMode::new(MODE_CONTEXT, "Context").description(
            "Build a cached workspace context bundle for the current task or question.",
        ),
        acp::SessionMode::new(MODE_PLAN, "Plan")
            .description("Split the task into scoped workstreams and coordination notes."),
        acp::SessionMode::new(MODE_ORCHESTRATE, "Orchestrate").description(
            "Run the full companion pipeline: context, skills, decomposition, and host steps.",
        ),
    ]
}

fn available_commands() -> Vec<acp::AvailableCommand> {
    vec![
        command(
            "help",
            "Show companion modes, boundaries, and available commands",
            None,
        ),
        command(
            "auto",
            "Run the default companion pipeline and return the final synthesized result",
            Some("Task or question"),
        ),
        command(
            "context",
            "Build a cached workspace context bundle for a task",
            Some("Task or question"),
        ),
        command(
            "plan",
            "Decompose a larger task into workstreams and coordination notes",
            Some("Task to split into workstreams"),
        ),
        command(
            "orchestrate",
            "Run the full companion orchestration pipeline",
            Some("Task to orchestrate"),
        ),
        command(
            "skills",
            "Search external skill libraries configured for the companion",
            Some("Optional skill query"),
        ),
        command(
            "memory",
            "Recall durable project memory for the current workspace",
            Some("Optional memory query"),
        ),
        command("status", "Show cache and index status", None),
        command("warm", "Prewarm cache, git, memory, and skills", None),
    ]
}

fn command(name: &str, description: &str, hint: Option<&str>) -> acp::AvailableCommand {
    let input = hint.map(|value| {
        acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(value))
    });
    acp::AvailableCommand::new(name, description).input(input)
}

fn flatten_prompt(blocks: &[acp::ContentBlock]) -> FlattenedPrompt {
    let mut text_parts = Vec::new();
    let mut referenced_resources = Vec::new();

    for block in blocks {
        match block {
            acp::ContentBlock::Text(text) => {
                let trimmed = text.text.trim();
                if !trimmed.is_empty() {
                    text_parts.push(trimmed.to_string());
                }
            }
            acp::ContentBlock::ResourceLink(resource) => {
                referenced_resources.push(
                    resource
                        .title
                        .clone()
                        .unwrap_or_else(|| resource.name.clone()),
                );
            }
            acp::ContentBlock::Resource(resource) => match &resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(contents) => {
                    referenced_resources.push(contents.uri.clone());
                }
                acp::EmbeddedResourceResource::BlobResourceContents(contents) => {
                    referenced_resources.push(contents.uri.clone());
                }
                _ => {}
            },
            _ => {}
        }
    }

    if !referenced_resources.is_empty() {
        text_parts.push(format!(
            "Referenced resources: {}",
            referenced_resources.join(", ")
        ));
    }

    FlattenedPrompt {
        user_text: text_parts.join("\n\n").trim().to_string(),
    }
}

fn parse_prompt_action(input: &str, mode: &acp::SessionModeId) -> PromptAction {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PromptAction::Help;
    }

    if let Some(action) = parse_explicit_command(trimmed) {
        return action;
    }

    match mode.0.as_ref() {
        MODE_AUTO => PromptAction::Auto(trimmed.to_string()),
        MODE_PLAN => PromptAction::Plan(trimmed.to_string()),
        MODE_ORCHESTRATE => PromptAction::Orchestrate(trimmed.to_string()),
        _ => PromptAction::Context(trimmed.to_string()),
    }
}

fn parse_explicit_command(input: &str) -> Option<PromptAction> {
    let trimmed = input.trim();
    let stripped = trimmed.strip_prefix('/')?;
    let (raw_command, remainder) = split_command(stripped);

    let command = raw_command
        .strip_prefix("codex-")
        .unwrap_or(raw_command)
        .trim()
        .to_lowercase();
    let remainder = remainder.trim();

    match command.as_str() {
        "auto" => Some(PromptAction::Auto(remainder.to_string())),
        "help" => Some(PromptAction::Help),
        "warm" => Some(PromptAction::Warm),
        "status" | "cache" => Some(PromptAction::Status),
        "context" => Some(PromptAction::Context(remainder.to_string())),
        "plan" => Some(PromptAction::Plan(remainder.to_string())),
        "orchestrate" => Some(PromptAction::Orchestrate(remainder.to_string())),
        "skills" => Some(optional_query_action(remainder, PromptAction::Skills)),
        "memory" => Some(optional_query_action(remainder, PromptAction::Memory)),
        _ => None,
    }
}

fn optional_query_action(
    remainder: &str,
    constructor: impl FnOnce(Option<String>) -> PromptAction,
) -> PromptAction {
    if remainder.is_empty() {
        constructor(None)
    } else {
        constructor(Some(remainder.to_string()))
    }
}

fn split_command(input: &str) -> (&str, &str) {
    match input.find(char::is_whitespace) {
        Some(index) => (&input[..index], &input[index + 1..]),
        None => (input, ""),
    }
}

fn help_text(mode: &acp::SessionModeId) -> String {
    format!(
        "# Codex Companion ACP Agent\n\n\
Current mode: `{}`\n\n\
This agent exposes the companion's cache, memory, skills, planning, and orchestration pipeline as an ACP session.\n\
It is local-first: a normal prompt runs the internal pipeline automatically, then hands the prepared context to an upstream Codex ACP backend when available instead of stopping at an intermediate orchestration dump.\n\n\
## Modes\n\
- `auto`: default mode, runs the internal pipeline end-to-end and forwards the prepared context to the upstream Codex ACP backend\n\
- `context`: build a cached context bundle for the task\n\
- `plan`: split a task into workstreams and coordination notes\n\
- `orchestrate`: run the full context + skills + decomposition pipeline\n\n\
## Commands\n\
- `/auto <task>`\n\
- `/context <task>`\n\
- `/plan <task>`\n\
- `/orchestrate <task>`\n\
- `/skills [query]`\n\
- `/memory [query]`\n\
- `/status`\n\
- `/warm`\n\
- `/help`\n\n\
If you type a normal prompt without a leading command, `auto` mode runs the stages internally and returns the final response. Use the explicit commands only when you need to inspect an intermediate stage."
    ,
        mode.0.as_ref()
    )
}

fn initial_plan_for_action(action: &PromptAction) -> Option<acp::Plan> {
    let entries = match action {
        PromptAction::Auto(_) => vec![
            acp::PlanEntry::new(
                "Build cached workspace context",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::InProgress,
            ),
            acp::PlanEntry::new(
                "Decompose the task into safe workstreams",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::Pending,
            ),
            acp::PlanEntry::new(
                "Assemble orchestration and companion notes",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::Pending,
            ),
            acp::PlanEntry::new(
                "Run the prepared task through the upstream Codex ACP backend",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::Pending,
            ),
        ],
        PromptAction::Context(_) => vec![
            acp::PlanEntry::new(
                "Refresh the cached workspace view",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::InProgress,
            ),
            acp::PlanEntry::new(
                "Recall durable memory and recent repo context",
                acp::PlanEntryPriority::Medium,
                acp::PlanEntryStatus::Pending,
            ),
            acp::PlanEntry::new(
                "Assemble the task-focused context bundle",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::Pending,
            ),
        ],
        PromptAction::Plan(_) | PromptAction::Orchestrate(_) => vec![
            acp::PlanEntry::new(
                "Collect cached workspace context",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::InProgress,
            ),
            acp::PlanEntry::new(
                "Split the task into scoped workstreams",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::Pending,
            ),
            acp::PlanEntry::new(
                "Prepare host steps and coordination notes",
                acp::PlanEntryPriority::Medium,
                acp::PlanEntryStatus::Pending,
            ),
        ],
        _ => return None,
    };

    Some(acp::Plan::new(entries))
}

fn plan_from_context_bundle(bundle: &ContextBundle) -> acp::Plan {
    let entries = if bundle.suggested_next_actions.is_empty() {
        vec![acp::PlanEntry::new(
            "Review the cached overview and highest-scoring files",
            acp::PlanEntryPriority::High,
            acp::PlanEntryStatus::Completed,
        )]
    } else {
        bundle
            .suggested_next_actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                acp::PlanEntry::new(
                    action.clone(),
                    if index == 0 {
                        acp::PlanEntryPriority::High
                    } else {
                        acp::PlanEntryPriority::Medium
                    },
                    acp::PlanEntryStatus::Completed,
                )
            })
            .collect()
    };
    acp::Plan::new(entries)
}

fn plan_from_decomposition(decomposition: &TaskDecomposition) -> acp::Plan {
    let entries = decomposition
        .workstreams
        .iter()
        .enumerate()
        .map(|(index, workstream)| {
            acp::PlanEntry::new(
                format!("{}: {}", workstream.title, workstream.objective),
                if index == 0 {
                    acp::PlanEntryPriority::High
                } else if workstream.can_run_in_parallel {
                    acp::PlanEntryPriority::Medium
                } else {
                    acp::PlanEntryPriority::Low
                },
                if index == 0 {
                    acp::PlanEntryStatus::InProgress
                } else {
                    acp::PlanEntryStatus::Pending
                },
            )
        })
        .collect();
    acp::Plan::new(entries)
}

fn plan_from_orchestration(orchestration: &TaskOrchestration) -> acp::Plan {
    let entries = if !orchestration.stages.is_empty() {
        orchestration
            .stages
            .iter()
            .enumerate()
            .map(|(index, stage)| {
                acp::PlanEntry::new(
                    format!("{}: {}", stage.title, stage.objective),
                    if index == 0 {
                        acp::PlanEntryPriority::High
                    } else if stage.run_in_parallel {
                        acp::PlanEntryPriority::Medium
                    } else {
                        acp::PlanEntryPriority::Low
                    },
                    if index == 0 {
                        acp::PlanEntryStatus::InProgress
                    } else {
                        acp::PlanEntryStatus::Pending
                    },
                )
            })
            .collect()
    } else {
        vec![acp::PlanEntry::new(
            orchestration.summary.clone(),
            acp::PlanEntryPriority::High,
            acp::PlanEntryStatus::Completed,
        )]
    };
    acp::Plan::new(entries)
}

fn complete_plan(mut plan: acp::Plan) -> acp::Plan {
    for entry in &mut plan.entries {
        entry.status = acp::PlanEntryStatus::Completed;
    }
    plan
}

fn format_auto_execution(
    bundle: &ContextBundle,
    decomposition: &TaskDecomposition,
    orchestration: &TaskOrchestration,
) -> String {
    let mut output = String::new();
    output.push_str("# Codex Companion Auto\n\n");
    output.push_str(&format!("Task: {}\n", orchestration.task));
    output.push_str(&format!("Summary: {}\n", orchestration.summary));
    output.push_str(&format!(
        "Execution mode: `{}`\nPrefer full access: `{}`\n\n",
        orchestration.execution_mode, orchestration.prefer_full_access
    ));

    let parallel_ready = decomposition
        .workstreams
        .iter()
        .filter(|workstream| workstream.can_run_in_parallel)
        .count();
    output.push_str("## Completed Stages\n");
    output.push_str("- Workspace context assembled from cache, git, memory, and skills.\n");
    output.push_str(&format!(
        "- Task decomposition completed: {} workstream(s), {} parallel-ready.\n",
        decomposition.workstreams.len(),
        parallel_ready
    ));
    output.push_str(&format!(
        "- Final orchestration prepared: {} stage(s), {} delegate brief(s).\n",
        orchestration.stages.len(),
        orchestration.subagent_specs.len()
    ));

    output.push_str("\n## Repo Snapshot\n");
    output.push_str(&format!(
        "- Root: `{}`\n- Indexed files: {}\n- Indexed bytes: {}\n",
        bundle.overview.workspace_root,
        bundle.overview.total_indexed_files,
        bundle.overview.total_indexed_bytes
    ));
    if !bundle.overview.major_languages.is_empty() {
        let languages = bundle
            .overview
            .major_languages
            .iter()
            .take(5)
            .map(|language| format!("{} ({})", language.language, language.files))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("- Major languages: {}\n", languages));
    }
    if let Some(git) = &bundle.recent_changes {
        if let Some(branch) = &git.branch {
            output.push_str(&format!("- Branch: {}\n", branch));
        }
        if !git.status_lines.is_empty() {
            output.push_str(&format!(
                "- Local changes visible: {}\n",
                git.status_lines.len()
            ));
        }
    }

    let relevant_files = if !bundle.search_hits.is_empty() {
        bundle
            .search_hits
            .iter()
            .map(|hit| (hit.path.clone(), Some(hit.summary.clone())))
            .collect::<Vec<_>>()
    } else if !decomposition.shared_context.is_empty() {
        decomposition
            .shared_context
            .iter()
            .map(|path| (path.clone(), None))
            .collect::<Vec<_>>()
    } else {
        bundle
            .overview
            .key_files
            .iter()
            .map(|path| (path.clone(), None))
            .collect::<Vec<_>>()
    };

    if !relevant_files.is_empty() {
        output.push_str("\n## Relevant Files\n");
        for (path, summary) in relevant_files.into_iter().take(6) {
            match summary {
                Some(summary) if !summary.is_empty() => {
                    output.push_str(&format!("- `{}`: {}\n", path, summary))
                }
                _ => output.push_str(&format!("- `{}`\n", path)),
            }
        }
    }

    if !decomposition.workstreams.is_empty() {
        output.push_str("\n## Workstreams\n");
        for workstream in decomposition.workstreams.iter().take(4) {
            output.push_str(&format!(
                "- {} [{}]: {}\n",
                workstream.title, workstream.id, workstream.objective
            ));
        }
    }

    let mut next_steps = bundle.suggested_next_actions.clone();
    for action in &decomposition.first_actions {
        if !next_steps.contains(action) {
            next_steps.push(action.clone());
        }
    }
    for action in &orchestration.recommended_host_steps {
        if !next_steps.contains(action) {
            next_steps.push(action.clone());
        }
    }

    if !next_steps.is_empty() {
        output.push_str("\n## Recommended Next Steps\n");
        for action in next_steps.into_iter().take(6) {
            output.push_str(&format!("- {}\n", action));
        }
    }

    output
}

fn is_analysis_task(task: &str) -> bool {
    let task = task.trim().to_lowercase();
    [
        "analysis",
        "analyze",
        "review",
        "audit",
        "inspect",
        "architecture",
        "codebase",
        "repo",
        "анализ",
        "разбор",
        "ревью",
        "аудит",
        "код",
        "репозитор",
        "архитектур",
    ]
    .iter()
    .any(|needle| task.contains(needle))
}

fn format_auto_analysis(
    index: &crate::model::WorkspaceIndex,
    bundle: &ContextBundle,
    decomposition: &TaskDecomposition,
    orchestration: &TaskOrchestration,
) -> String {
    let mut output = String::new();
    output.push_str("# Codex Companion Analysis\n\n");
    output.push_str(&format!("Task: {}\n", orchestration.task));
    output.push_str(&format!(
        "Summary: Deterministic repository analysis completed from {} indexed files.\n",
        index.files.len()
    ));
    output.push_str(&format!(
        "Execution mode: `{}`\nPrefer full access: `{}`\n\n",
        orchestration.execution_mode, orchestration.prefer_full_access
    ));

    output.push_str("## Architecture\n");
    if index.files.iter().any(|file| file.path == "src/lib.rs")
        && index
            .files
            .iter()
            .any(|file| file.path == "server/src/main.rs")
    {
        output.push_str(
            "- The workspace is split between a Zed extension surface (`src/lib.rs`) and a companion server crate under `server/`.\n",
        );
    }
    if index
        .files
        .iter()
        .any(|file| file.path == "server/src/lib.rs")
        && index
            .files
            .iter()
            .any(|file| file.path == "server/src/bin/acp_agent.rs")
    {
        output.push_str(
            "- The server runtime is now shared through `server/src/lib.rs`, with separate MCP and ACP entrypoints layered on top.\n",
        );
    }
    if !bundle.overview.major_languages.is_empty() {
        let languages = bundle
            .overview
            .major_languages
            .iter()
            .take(5)
            .map(|language| format!("{} ({})", language.language, language.files))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("- Major languages: {}\n", languages));
    }
    if !bundle.overview.top_directories.is_empty() {
        let directories = bundle
            .overview
            .top_directories
            .iter()
            .take(4)
            .map(|directory| format!("{} ({})", directory.directory, directory.files))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("- Main directories: {}\n", directories));
    }

    let hotspots = analysis_hotspots(index);
    if !hotspots.is_empty() {
        output.push_str("\n## Findings\n");
        output.push_str("- The main maintainability hotspots are concentrated in a small set of large Rust modules:\n");
        for file in hotspots.iter().take(4) {
            output.push_str(&format!(
                "  - `{}`: {} lines, {} bytes.\n",
                file.path, file.line_count, file.size
            ));
        }
    } else {
        output.push_str("\n## Findings\n");
    }

    if decomposition.workstreams.len() <= 1 {
        output.push_str(
            "- The task planner found only one safe workstream, which usually means the current architecture is fairly coupled or the request is still too broad.\n",
        );
    } else {
        output.push_str(&format!(
            "- The planner found {} workstreams, so the codebase already has some separable scopes for follow-up work.\n",
            decomposition.workstreams.len()
        ));
    }

    let test_files = index
        .files
        .iter()
        .filter(|file| is_test_path(&file.path))
        .count();
    if test_files > 0 {
        output.push_str(&format!(
            "- Tests are split into dedicated module-local files ({} test file(s) detected), which keeps production modules slimmer.\n",
            test_files
        ));
    }

    if let Some(git) = &bundle.recent_changes {
        if !git.status_lines.is_empty() {
            output.push_str(&format!(
                "- The worktree is currently dirty ({} visible change(s)), so this analysis reflects in-progress local edits, not only the last committed baseline.\n",
                git.status_lines.len()
            ));
        }
    }

    let focus_files = analysis_focus_files(index, decomposition, orchestration);
    if !focus_files.is_empty() {
        output.push_str("\n## Key Files\n");
        for file in focus_files.iter().take(6) {
            output.push_str(&format!(
                "- `{}`: {}\n",
                file.path,
                describe_analysis_role(file)
            ));
        }
    }

    let mut next_steps = Vec::new();
    next_steps.push(
        "Start code reading from the key files below instead of top-level docs only.".to_string(),
    );
    if !decomposition.first_actions.is_empty() {
        next_steps.extend(decomposition.first_actions.iter().cloned());
    }
    if !bundle.suggested_next_actions.is_empty() {
        next_steps.extend(bundle.suggested_next_actions.iter().cloned());
    }

    if !next_steps.is_empty() {
        next_steps.dedup();
        output.push_str("\n## Recommended Next Steps\n");
        for step in next_steps.into_iter().take(6) {
            output.push_str(&format!("- {}\n", step));
        }
    }

    output
}

impl CodexRenderConfig {
    fn from_env() -> Self {
        Self {
            max_context_chars: read_env_usize("CODEX_COMPANION_CODEX_MAX_CONTEXT_CHARS", 36_000),
            max_file_excerpt_chars: read_env_usize(
                "CODEX_COMPANION_CODEX_MAX_FILE_EXCERPT_CHARS",
                4_000,
            ),
        }
    }
}

fn render_codex_turn_input(
    user_prompt: &str,
    artifacts: AutoArtifacts<'_>,
    render_config: &CodexRenderConfig,
) -> String {
    let mut packet = String::new();
    packet.push_str(
        "Companion-prepared workspace context follows. Treat it as cached guidance and a repo map, but verify against the actual files before making claims, edits, or test statements.\n",
    );
    packet.push_str(
        "Respond in the same language as the user unless the task clearly asks otherwise.\n",
    );
    if artifacts.analysis_task {
        packet.push_str(
            "The user is asking for real code analysis. Provide concrete findings first, ordered by severity or importance, with file paths when possible. After findings, give a short architecture summary and the most useful next steps.\n",
        );
    } else {
        packet.push_str(
            "Use the companion notes as preparation, then continue the task normally with the full Codex tool loop.\n",
        );
    }

    packet.push_str("\n## User Request\n");
    packet.push_str(user_prompt.trim());

    packet.push_str("\n\n## Prepared Companion Notes\n");
    packet.push_str(artifacts.fallback_text.trim());

    packet.push_str("\n\n## Workspace Snapshot\n");
    packet.push_str(&format!(
        "- Root: `{}`\n- Indexed files: {}\n- Indexed bytes: {}\n- Execution mode: `{}`\n- Prefer full access: `{}`\n",
        artifacts.bundle.overview.workspace_root,
        artifacts.bundle.overview.total_indexed_files,
        artifacts.bundle.overview.total_indexed_bytes,
        artifacts.orchestration.execution_mode,
        artifacts.orchestration.prefer_full_access
    ));

    if !artifacts.bundle.overview.major_languages.is_empty() {
        let languages = artifacts
            .bundle
            .overview
            .major_languages
            .iter()
            .take(6)
            .map(|language| format!("{} ({})", language.language, language.files))
            .collect::<Vec<_>>()
            .join(", ");
        packet.push_str(&format!("- Languages: {}\n", languages));
    }
    if !artifacts.decomposition.workstreams.is_empty() {
        packet.push_str(&format!(
            "- Workstreams: {}\n",
            artifacts.decomposition.workstreams.len()
        ));
    }

    packet.push_str("\n## Indexed File Excerpts\n");
    let file_context = render_model_file_context(
        artifacts.index,
        artifacts.decomposition,
        artifacts.orchestration,
        render_config.max_file_excerpt_chars,
        artifacts.analysis_task,
    );
    packet.push_str(&file_context);

    truncate_chars(&packet, render_config.max_context_chars)
}

fn render_model_file_context(
    index: &crate::model::WorkspaceIndex,
    decomposition: &TaskDecomposition,
    orchestration: &TaskOrchestration,
    max_file_excerpt_chars: usize,
    analysis_task: bool,
) -> String {
    let mut output = String::new();
    let files = if analysis_task {
        analysis_focus_files(index, decomposition, orchestration)
    } else {
        analysis_focus_files(index, decomposition, orchestration)
            .into_iter()
            .take(4)
            .collect::<Vec<_>>()
    };

    for file in files.into_iter().take(6) {
        output.push_str(&format!(
            "### {}\n- Role: {}\n- Symbols: {}\n- Excerpt:\n```text\n{}\n```\n\n",
            file.path,
            describe_analysis_role(file),
            if file.symbols.is_empty() {
                "-".to_string()
            } else {
                file.symbols
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            truncate_chars(
                if file.indexed_text.trim().is_empty() {
                    &file.preview
                } else {
                    &file.indexed_text
                },
                max_file_excerpt_chars
            )
        ));
    }

    output
}

fn analysis_hotspots(index: &crate::model::WorkspaceIndex) -> Vec<&crate::model::FileRecord> {
    let mut files = index
        .files
        .iter()
        .filter(|file| file.language == "rust" && !is_test_path(&file.path))
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        right
            .line_count
            .cmp(&left.line_count)
            .then_with(|| right.size.cmp(&left.size))
    });
    files
}

fn analysis_focus_files<'a>(
    index: &'a crate::model::WorkspaceIndex,
    decomposition: &TaskDecomposition,
    orchestration: &TaskOrchestration,
) -> Vec<&'a crate::model::FileRecord> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();

    let mut push_file = |path: &str| {
        if seen.insert(path.to_string()) {
            if let Some(file) = index.files.iter().find(|file| file.path == path) {
                selected.push(file);
            }
        }
    };

    for path in [
        "src/lib.rs",
        "server/src/main.rs",
        "server/src/lib.rs",
        "server/src/bin/acp_agent.rs",
        "server/src/state.rs",
        "server/src/planning.rs",
        "server/src/cache.rs",
        "server/src/acp.rs",
    ] {
        push_file(path);
    }

    for path in &decomposition.shared_context {
        push_file(path);
    }

    for spec in &orchestration.subagent_specs {
        for path in &spec.recommended_files {
            push_file(path);
        }
    }

    for file in analysis_hotspots(index) {
        if selected.len() >= 8 {
            break;
        }
        if seen.insert(file.path.clone()) {
            selected.push(file);
        }
    }

    selected
}

fn describe_analysis_role(file: &crate::model::FileRecord) -> String {
    let path = file.path.as_str();
    if path == "src/lib.rs" {
        "Zed extension entrypoint and integration surface".to_string()
    } else if path == "server/src/main.rs" {
        "MCP server entrypoint and transport wiring".to_string()
    } else if path == "server/src/lib.rs" {
        "shared server core used by both MCP and ACP runtimes".to_string()
    } else if path == "server/src/bin/acp_agent.rs" {
        "ACP agent binary entrypoint".to_string()
    } else if path.ends_with("/state.rs") || path.ends_with("state.rs") {
        "runtime state, cache lifecycle, and task pipeline coordination".to_string()
    } else if path.ends_with("/planning.rs") || path.ends_with("planning.rs") {
        "task decomposition and orchestration heuristics".to_string()
    } else if path.ends_with("/cache.rs") || path.ends_with("cache.rs") {
        "SQLite-backed persistence and cache recovery path".to_string()
    } else if path.ends_with("/acp.rs") || path.ends_with("acp.rs") {
        "ACP session handling and auto-mode response assembly".to_string()
    } else if path.ends_with("/skills.rs") || path.ends_with("skills.rs") {
        "external skill catalog indexing and search".to_string()
    } else if path.ends_with("/indexer.rs") || path.ends_with("indexer.rs") {
        "workspace indexing and cached search".to_string()
    } else if !file.symbols.is_empty() {
        format!(
            "contains symbols such as {}",
            file.symbols
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!("{} lines, {} bytes", file.line_count, file.size)
    }
}

fn is_test_path(path: &str) -> bool {
    path.ends_with("/tests.rs")
        || path.ends_with("\\tests.rs")
        || path.contains("/tests/")
        || path.contains("\\tests\\")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return value.to_string();
    }

    let keep = max_chars.saturating_sub(3);
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn codex_fallback_notice(error: &anyhow::Error) -> String {
    format!(
        "_Codex backend handoff is unavailable right now ({error}). Falling back to the local deterministic companion output._"
    )
}

fn read_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn session_key(session_id: &acp::SessionId) -> String {
    session_id.0.to_string()
}

fn session_title(root: &Path, prompt: &str, action: Option<&PromptAction>) -> String {
    let workspace = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("workspace");
    let prefix = match action {
        Some(PromptAction::Auto(_)) => "Auto",
        Some(PromptAction::Plan(_)) => "Plan",
        Some(PromptAction::Orchestrate(_)) => "Orchestrate",
        Some(PromptAction::Skills(_)) => "Skills",
        Some(PromptAction::Memory(_)) => "Memory",
        Some(PromptAction::Status) => "Status",
        Some(PromptAction::Warm) => "Warmup",
        _ => "Context",
    };

    let summary = prompt
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('/')
        .chars()
        .take(48)
        .collect::<String>();

    if summary.is_empty() {
        format!("{workspace} {prefix}")
    } else {
        format!("{workspace} {prefix}: {summary}")
    }
}

fn internal_error(error: impl std::fmt::Display) -> acp::Error {
    acp::Error::new(-32603, error.to_string())
}

#[cfg(test)]
mod tests;
