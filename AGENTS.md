# Workspace Instructions

- Use `D:\downloads\agency-agents` as the default external skill library for Codex Companion in this workspace.
- When a task needs agent or skill lookup, prefer the markdown agents under `D:\downloads\agency-agents` before falling back to generic examples.
- Keep `skill_roots` pointed at `D:\downloads\agency-agents` unless the user explicitly asks to override or disable it.
- If `search_skills` returns no results, verify that the active Codex Companion server is running with `D:\downloads\agency-agents` in `skill_roots`.
