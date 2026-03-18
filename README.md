# Codex Companion for Zed

Codex Companion is a Zed extension that augments Zed's built-in Codex support with:

- persistent project memory across sessions
- a cached workspace index with incremental reuse
- background prewarm for cache, git, and memory
- configurable external skill libraries (plain `.md` agent packs are supported)
- task-focused context bundles for faster prompts
- first-class task orchestration with subagent-ready briefs
- task decomposition into scoped workstreams
- git-aware context tools
- Zed extension slash commands that reuse the same local cache engine

It does not replace Zed's native Codex thread support. Instead, it gives Codex a better local MCP server.

It works across text-based repositories in many languages, but it is not a language-server replacement. The cached index is language-agnostic at the file level, while symbol extraction and "key file" heuristics are still heuristic and strongest on common engineering stacks.

It is also not a universal task engine for arbitrary work. The core product is repo- and file-centric: search, memory, decomposition, and orchestration are optimized for software and documentation workflows. External markdown skill packs can broaden domain guidance, but the engine still reasons primarily from files, symbols, and recent repo state.

The `configuration/default_settings.jsonc` file is a bundled default-settings preview for Zed's context-server UI, not the live settings file you edit. Put overrides in `Agent Panel -> Settings`, `agent: open settings`, or workspace `.zed/settings.jsonc`.

Slash-command note: the MCP server receives full project/profile settings from Zed, but extension slash commands only get a `Worktree`. In practice, `/codex-*` always sees workspace `.zed/settings*`, and it reuses profile-level server settings only after the companion server has already been started in the current extension instance.

Thread history is also owned by Zed's native Agent UI. `codex-companion` cannot auto-delete old threads when a new one is created; use Zed actions such as `agent: remove selected thread` or `agent: remove history`.

## What you get

- `workspace_overview`: fast summary of the current repo
- `warm_workspace`: prewarm index, git, and memories for the next turn
- `search_workspace`: cached search across files, symbols, and snippets
- `search_skills`: search external skill libraries configured under `skill_roots`
- `build_context_bundle`: overview + search hits + memories + recent changes
- `orchestrate_task`: run skill lookup, context building, decomposition, and subagent briefs in one step
- `decompose_task`: split a large task into workstreams and coordination notes
- `remember_memory` and `recall_memory`: durable project memory
- `recent_changes`: git status and recent commits
- Zed extension slash commands:
  - `/codex-context`
  - `/codex-memory`
  - `/codex-cache`
  - `/codex-refresh`
  - `/codex-warm`
  - `/codex-plan`
  - `/codex-orchestrate`
  - `/codex-skills`
- in Codex ACP threads, call the MCP tools directly instead: `cache_status`, `build_context_bundle`, `orchestrate_task`, `decompose_task`, `search_skills`, `remember_memory`

## Repo layout

- `src/lib.rs`: Zed extension entrypoint
- `server/`: companion MCP server and CLI
- `configuration/`: first-run instructions and default settings schema
- `docs/TUTORIAL.ru.md`: step-by-step setup guide in Russian

## Local setup

1. Install a current Rust toolchain via `rustup`.
2. Build the companion server:

```bash
cargo build --release -p codex-companion-server
```

If Zed later says it is still waiting for the context service, either set an explicit
binary path in the `codex-companion` settings:

```json
{
  "context_servers": {
    "codex-companion": {
      "settings": {
        "server_path": "C:\\path\\to\\zed-codex\\target\\release\\codex-companion-server.exe"
      }
    }
  }
}
```

or point `release_repo` at a GitHub repository that publishes matching `codex-companion-server-*` release assets. If you also set the same GitHub repository URL in `extension.toml`, the extension can infer `release_repo` automatically. In this repo, treat `release_repo` as the publish-safe default until a real canonical `repository` URL is added to the manifest.

3. In Zed, run `zed: extensions`.
4. Choose `Install Dev Extension`.
5. Point Zed to this repository.
6. Open the Agent Panel settings and enable `codex-companion`.
7. Start a new Codex thread and keep the companion server enabled for that profile.

If Zed shows a `default_settings.jsonc` preview for the context server, treat it as reference text only. Change real values in the agent settings UI or `.zed/settings.jsonc`.

Detailed setup steps are in [docs/TUTORIAL.ru.md](docs/TUTORIAL.ru.md).

## Performance and autonomy

The companion now includes:

- background prewarm on server start
- in-memory TTL cache for git summaries
- in-memory TTL cache for task bundles and decompositions
- in-memory TTL cache for external skill catalogs
- an execution mode hint: `careful`, `balanced`, or `autonomous`
- an advisory `prefer_full_access` mode for trusted workspaces
- orchestration output designed for host agents that can parallelize work

Important boundary: the MCP extension now emits subagent-ready orchestration contracts, but it still cannot itself grant sandbox bypass, shell full-access, or force ACP subagent execution. Those capabilities are controlled by the host agent and Zed.

For Zed's native tool approvals, the official setting is `agent.tool_permissions.default`, where `"allow"` auto-approves tool actions. MCP tools can also be configured individually with keys like `mcp:codex-companion:decompose_task`.

Example:

```json
{
  "agent": {
    "tool_permissions": {
      "default": "allow",
      "tools": {
        "mcp:codex-companion:warm_workspace": {
          "default": "allow"
        },
        "mcp:codex-companion:decompose_task": {
          "default": "allow"
        }
      }
    }
  }
}
```

If your Codex ACP host also exposes its own approval/full-access mode, enable that there as well. The companion will then steer Codex toward using that mode, but it does not override the host's security model.

## External skills

You can point Codex Companion at external skill libraries with:

```json
{
  "context_servers": {
    "codex-companion": {
      "settings": {
        "skill_roots": [
          "C:\\path\\to\\agency-agents"
        ],
        "skill_file_globs": [
          "**/*.md"
        ],
        "max_skills_per_query": 6
      }
    }
  }
}
```

This works with markdown agent packs such as `agency-agents`, not just `SKILL.md`-style repos. Matching skills are now included in task bundles, decomposition output, and per-workstream orchestration briefs, and can be queried directly with `/codex-skills` in Zed's slash-command surfaces, or with `search_skills` in Codex ACP threads.
Skill packs broaden guidance and playbooks, but they do not change the companion's core execution model: it still plans from workspace files, cached snippets, memories, and repo state.
For larger tasks, `orchestrate_task` is the intended entrypoint: it binds skill lookup, shared context, workstreams, and subagent-ready prompts into one result.
Important boundary: Codex ACP parses its own `/...` commands. If you are inside a Codex ACP thread, use the MCP tools directly instead of `/codex-cache`, `/codex-plan`, or `/codex-orchestrate`.
The extension slash commands reuse the same configured companion environment after the companion server has been started, and they always read workspace `.zed/settings*` directly. If profile-only settings are critical for slash commands, start the companion server once or mirror those settings in `.zed/settings.jsonc`.

## Release path

For local/dev usage, the extension looks for a locally built server binary at:

- `server_path` from the context server settings, if provided
- `target/release/codex-companion-server`
- `target/release/codex-companion-server.exe`

If you publish server binaries to GitHub Releases, set `release_repo` in the context server settings so the extension can download the latest matching archive automatically. If `extension.toml` also points at that GitHub repository, the extension can infer the same `owner/name` by default. Until a real `repository` value is committed to the manifest, explicit `release_repo` is the reliable publish path.

The included release workflow builds server archives for:

- Linux x86_64
- Linux aarch64
- Windows x86_64
- Windows aarch64
- macOS x86_64
- macOS aarch64

Any other platform can still build from source with the same `cargo build --release -p codex-companion-server` command.
