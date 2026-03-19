use crate::model::{
    CacheStatus, ContextBundle, MemorySearchResults, SkillSearchResults, TaskDecomposition,
    TaskOrchestration, WarmupStatus,
};

pub fn format_cache_status(status: &CacheStatus) -> String {
    format!(
        "# Codex Companion Cache\n\n\
Workspace: `{}`\n\
Workspace ID: `{}`\n\
Cache dir: `{}`\n\
Indexed at: `{}`\n\n\
- Indexed files: {}\n\
- Indexed bytes: {}\n\
- Reused files: {}\n\
- Reindexed files: {}\n\
- Skipped files: {}\n",
        status.workspace_root,
        status.workspace_id,
        status.cache_dir,
        status.indexed_at,
        status.indexed_files,
        status.indexed_bytes,
        status.scan_metrics.reused_files,
        status.scan_metrics.reindexed_files,
        status.scan_metrics.skipped_files,
    )
}

pub fn format_warmup_status(status: &WarmupStatus) -> String {
    format!(
        "# Codex Companion Warmup\n\n\
Workspace: `{}`\n\
Elapsed: {} ms\n\
Git warmed: {}\n\
Memory warmed: {}\n\
Skills warmed: {}\n\n{}",
        status.workspace_root,
        status.elapsed_ms,
        status.warmed_git,
        status.warmed_memory,
        status.warmed_skills,
        format_cache_status(&status.cache_status)
    )
}

pub fn format_context_bundle(bundle: &ContextBundle) -> String {
    let mut output = String::new();
    output.push_str("# Codex Context Bundle\n\n");
    output.push_str(&format!("Task: {}\n\n", bundle.task));
    output.push_str("## Workspace Overview\n");
    output.push_str(&format!(
        "- Root: `{}`\n- Indexed at: `{}`\n- Files: {}\n- Bytes: {}\n",
        bundle.overview.workspace_root,
        bundle.overview.indexed_at,
        bundle.overview.total_indexed_files,
        bundle.overview.total_indexed_bytes
    ));

    if !bundle.overview.major_languages.is_empty() {
        output.push_str("\n## Languages\n");
        for language in &bundle.overview.major_languages {
            output.push_str(&format!("- {}: {}\n", language.language, language.files));
        }
    }

    if !bundle.search_hits.is_empty() {
        output.push_str("\n## Relevant Files\n");
        for hit in &bundle.search_hits {
            output.push_str(&format!(
                "- `{}` (score {:.1})\n{}\n{}\n",
                hit.path, hit.score, hit.summary, hit.snippet
            ));
        }
    }

    if !bundle.memories.is_empty() {
        output.push_str("\n## Recalled Memory\n");
        for memory in &bundle.memories {
            output.push_str(&format!(
                "- **{}** [{}]\n{}\n",
                memory.title,
                memory.tags.join(", "),
                memory.content
            ));
        }
    }

    if !bundle.recommended_skills.is_empty() {
        output.push_str("\n## Recommended Skills\n");
        for skill in &bundle.recommended_skills {
            output.push_str(&format!(
                "- {} ({})\n{}\n{}\n",
                skill.name, skill.category, skill.description, skill.path
            ));
        }
    }

    if let Some(git) = &bundle.recent_changes {
        if git.available {
            output.push_str("\n## Recent Changes\n");
            if let Some(branch) = &git.branch {
                output.push_str(&format!("- Branch: {}\n", branch));
            }
            for line in &git.status_lines {
                output.push_str(&format!("- {}\n", line));
            }
            for commit in &git.recent_commits {
                output.push_str(&format!("- {}\n", commit));
            }
        }
    }

    if !bundle.suggested_next_actions.is_empty() {
        output.push_str("\n## Suggested Next Actions\n");
        for action in &bundle.suggested_next_actions {
            output.push_str(&format!("- {}\n", action));
        }
    }

    output
}

pub fn format_task_orchestration(orchestration: &TaskOrchestration) -> String {
    let mut output = String::new();
    output.push_str("# Codex Task Orchestration\n\n");
    output.push_str(&format!("Task: {}\n", orchestration.task));
    output.push_str(&format!("Summary: {}\n", orchestration.summary));
    output.push_str(&format!(
        "Execution mode: `{}`\nPrefer full access: `{}`\n\n",
        orchestration.execution_mode, orchestration.prefer_full_access
    ));

    if !orchestration.stages.is_empty() {
        output.push_str("## Stages\n");
        for stage in &orchestration.stages {
            output.push_str(&format!(
                "- {} [{}]\n  Objective: {}\n  Workstreams: {}\n  Parallel: {}\n",
                stage.title,
                stage.id,
                stage.objective,
                if stage.workstream_ids.is_empty() {
                    "-".to_string()
                } else {
                    stage.workstream_ids.join(", ")
                },
                stage.run_in_parallel
            ));
        }
        output.push('\n');
    }

    if !orchestration.subagent_specs.is_empty() {
        output.push_str("## Subagent Specs\n");
        for spec in &orchestration.subagent_specs {
            let skill_summary = if spec.recommended_skills.is_empty() {
                "-".to_string()
            } else {
                spec.recommended_skills
                    .iter()
                    .map(|skill| format!("{} ({})", skill.name, skill.path))
                    .collect::<Vec<_>>()
                    .join(", ")
            };

            output.push_str(&format!(
                "- {} [{} | role: {}]\n  Objective: {}\n  Files: {}\n  Symbols: {}\n  Skills: {}\n  Parallel: {}\n  Completion: {}\n  Prompt: {}\n",
                spec.title,
                spec.workstream_id,
                spec.agent_role,
                spec.objective,
                if spec.recommended_files.is_empty() {
                    "-".to_string()
                } else {
                    spec.recommended_files.join(", ")
                },
                if spec.matching_symbols.is_empty() {
                    "-".to_string()
                } else {
                    spec.matching_symbols.join(", ")
                },
                skill_summary,
                spec.run_in_parallel,
                spec.completion_criteria.join(" | "),
                spec.prompt
            ));
        }
        output.push('\n');
    }

    if !orchestration.recommended_host_steps.is_empty() {
        output.push_str("## Host Steps\n");
        for step in &orchestration.recommended_host_steps {
            output.push_str(&format!("- {}\n", step));
        }
        output.push('\n');
    }

    if !orchestration.host_constraints.is_empty() {
        output.push_str("## Host Constraints\n");
        for note in &orchestration.host_constraints {
            output.push_str(&format!("- {}\n", note));
        }
        output.push('\n');
    }

    output.push_str("## Context Bundle Snapshot\n");
    output.push_str(&format_context_bundle(&orchestration.context_bundle));
    output.push('\n');
    output.push_str("## Decomposition Snapshot\n");
    output.push_str(&format_task_decomposition(&orchestration.decomposition));

    output
}

pub fn format_task_decomposition(decomposition: &TaskDecomposition) -> String {
    let mut output = String::new();
    output.push_str("# Codex Task Decomposition\n\n");
    output.push_str(&format!("Task: {}\n", decomposition.task));
    output.push_str(&format!("Summary: {}\n", decomposition.summary));
    output.push_str(&format!(
        "Execution mode: `{}`\nPrefer full access: `{}`\nCan parallelize: `{}`\n\n",
        decomposition.execution_mode,
        decomposition.prefer_full_access,
        decomposition.can_parallelize
    ));

    if !decomposition.recommended_starting_tools.is_empty() {
        output.push_str("## Starting Tools\n");
        for tool in &decomposition.recommended_starting_tools {
            output.push_str(&format!("- {}\n", tool));
        }
        output.push('\n');
    }

    if !decomposition.shared_context.is_empty() {
        output.push_str("## Shared Context\n");
        for item in &decomposition.shared_context {
            output.push_str(&format!("- {}\n", item));
        }
        output.push('\n');
    }

    output.push_str("## Workstreams\n");
    for workstream in &decomposition.workstreams {
        output.push_str(&format!(
            "- {} [{}]\n  Objective: {}\n  Rationale: {}\n  Files: {}\n  Symbols: {}\n  Skills: {}\n  Parallel: {}\n  Handoff: {}\n",
            workstream.title,
            workstream.id,
            workstream.objective,
            workstream.rationale,
            if workstream.recommended_files.is_empty() {
                "-".to_string()
            } else {
                workstream.recommended_files.join(", ")
            },
            if workstream.matching_symbols.is_empty() {
                "-".to_string()
            } else {
                workstream.matching_symbols.join(", ")
            },
            if workstream.recommended_skills.is_empty() {
                "-".to_string()
            } else {
                workstream.recommended_skills.join(", ")
            },
            workstream.can_run_in_parallel,
            workstream.handoff
        ));
    }

    if !decomposition.recommended_skills.is_empty() {
        output.push_str("\n## Recommended Skills\n");
        for skill in &decomposition.recommended_skills {
            output.push_str(&format!("- {} ({})\n", skill.name, skill.path));
        }
    }

    if !decomposition.coordination_notes.is_empty() {
        output.push_str("\n## Coordination Notes\n");
        for note in &decomposition.coordination_notes {
            output.push_str(&format!("- {}\n", note));
        }
    }

    if !decomposition.first_actions.is_empty() {
        output.push_str("\n## First Actions\n");
        for action in &decomposition.first_actions {
            output.push_str(&format!("- {}\n", action));
        }
    }

    output
}

pub fn format_memory_results(results: &MemorySearchResults) -> String {
    let mut output = String::new();
    output.push_str("# Codex Memory Recall\n\n");
    if let Some(query) = &results.query {
        output.push_str(&format!("Query: `{}`\n\n", query));
    }

    if results.matches.is_empty() {
        output.push_str("No stored memories matched.\n");
        return output;
    }

    for memory in &results.matches {
        output.push_str(&format!(
            "## {}\n- Tags: {}\n- Importance: {}\n- Updated: {}\n\n{}\n\n",
            memory.title,
            memory.tags.join(", "),
            memory.importance,
            memory.updated_at,
            memory.content
        ));
    }

    output
}

pub fn format_skill_results(results: &SkillSearchResults) -> String {
    let mut output = String::new();
    output.push_str("# Codex Skills\n\n");
    if let Some(query) = &results.query {
        output.push_str(&format!("Query: `{}`\n\n", query));
    }

    if results.hits.is_empty() {
        output.push_str("No external skills matched.\n");
        return output;
    }

    for skill in &results.hits {
        output.push_str(&format!(
            "## {} ({})\n- Path: {}\n- Score: {:.1}\n- Match reasons: {}\n\n{}\n\n",
            skill.name,
            skill.category,
            skill.path,
            skill.score,
            if skill.match_reasons.is_empty() {
                "-".to_string()
            } else {
                skill.match_reasons.join(", ")
            },
            skill.description
        ));
    }

    output
}
