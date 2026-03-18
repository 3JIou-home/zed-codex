Codex Companion is a companion layer for Zed's built-in Codex support.

1. Make sure Zed itself can already open a Codex thread.
2. For a local dev install, build the server once from this repo:
   `cargo build --release -p codex-companion-server`
   If Zed still cannot start the service, set `server_path` to the absolute
   path of the built binary.
3. Install this repository as a dev extension in Zed.
4. Open `Agent Panel -> Settings`, enable the `codex-companion` context server, and keep it enabled for your Codex profile.
5. Optional: set `release_repo` if you publish prebuilt server binaries and want auto-downloads instead of local builds. If your `extension.toml` repository already points at that GitHub repo, the extension can infer the same `owner/name` automatically.

Settings note:
- The `default_settings.jsonc` preview shown by Zed is reference material, not the live file you edit.
- Change `codex-companion` settings in `Agent Panel -> Settings`, `agent: open settings`, or your workspace `.zed/settings.jsonc`.

The extension adds:
- a local MCP/context server with cached workspace indexing and persistent memory
- Zed extension slash commands: `/codex-context`, `/codex-memory`, `/codex-cache`, `/codex-refresh`, `/codex-warm`, `/codex-plan`, `/codex-orchestrate`, `/codex-skills`
- warmup, orchestration, decomposition, and skill tools: `warm_workspace`, `orchestrate_task`, `decompose_task`, `search_skills`

Important UX note: Codex ACP threads parse their own `/...` commands and may not expose extension slash commands there. In ACP threads, ask Codex to call MCP tools directly, e.g. `cache_status`, `build_context_bundle`, or `orchestrate_task`.

Thread note:
- Codex Companion does not own Zed's native thread list and cannot auto-delete old threads when you create a new one.
- Remove threads with Zed's built-in actions such as `agent: remove selected thread` or `agent: remove history`.

For faster execution in trusted workspaces:
- set `prewarm_on_start` to `true`
- use `execution_mode = "autonomous"` if you want stronger execution hints
- use Zed's `agent.tool_permissions.default = "allow"` if you want tools auto-approved
- set `skill_roots` if you want Codex Companion to search local markdown agent packs such as `C:\path\to\agency-agents` or `/opt/agency-agents`

Important: Codex Companion can now emit subagent-ready orchestration briefs, but the actual sandbox/full-access mode and true ACP subagents are controlled by the host agent, not by this MCP extension.
