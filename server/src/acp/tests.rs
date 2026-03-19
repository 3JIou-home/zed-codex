use std::path::PathBuf;

use agent_client_protocol as acp;

use super::{
    is_analysis_task, parse_explicit_command, parse_prompt_action, plan_from_context_bundle,
    plan_from_decomposition, plan_from_orchestration, session_title, PromptAction, MODE_AUTO,
    MODE_CONTEXT, MODE_ORCHESTRATE, MODE_PLAN,
};
use crate::{
    model::{OrchestrationStage, ScanMetrics, TaskWorkstream, WorkspaceOverview},
    ContextBundle, TaskDecomposition, TaskOrchestration,
};

fn test_overview() -> WorkspaceOverview {
    WorkspaceOverview {
        workspace_root: "repo".to_string(),
        indexed_at: "2026-03-18T00:00:00Z".to_string(),
        total_indexed_files: 0,
        total_indexed_bytes: 0,
        major_languages: Vec::new(),
        top_directories: Vec::new(),
        key_files: Vec::new(),
        highlights: Vec::new(),
        scan_metrics: ScanMetrics {
            reused_files: 0,
            reindexed_files: 0,
            skipped_files: 0,
        },
    }
}

#[test]
fn parse_explicit_command_supports_codex_prefix() {
    assert_eq!(
        parse_explicit_command("/codex-orchestrate tighten cache recovery"),
        Some(PromptAction::Orchestrate(
            "tighten cache recovery".to_string()
        ))
    );
}

#[test]
fn parse_explicit_command_supports_auto() {
    assert_eq!(
        parse_explicit_command("/auto inspect server"),
        Some(PromptAction::Auto("inspect server".to_string()))
    );
}

#[test]
fn parse_prompt_action_uses_mode_for_plain_text() {
    let auto_mode = acp::SessionModeId::new(MODE_AUTO);
    let context_mode = acp::SessionModeId::new(MODE_CONTEXT);
    let plan_mode = acp::SessionModeId::new(MODE_PLAN);
    let orchestrate_mode = acp::SessionModeId::new(MODE_ORCHESTRATE);

    assert_eq!(
        parse_prompt_action("inspect workspace", &auto_mode),
        PromptAction::Auto("inspect workspace".to_string())
    );
    assert_eq!(
        parse_prompt_action("inspect workspace", &context_mode),
        PromptAction::Context("inspect workspace".to_string())
    );
    assert_eq!(
        parse_prompt_action("split work", &plan_mode),
        PromptAction::Plan("split work".to_string())
    );
    assert_eq!(
        parse_prompt_action("ship it", &orchestrate_mode),
        PromptAction::Orchestrate("ship it".to_string())
    );
}

#[test]
fn analysis_task_detection_supports_russian_and_english_queries() {
    assert!(is_analysis_task("проведи анализ кода"));
    assert!(is_analysis_task("analyze the codebase"));
    assert!(!is_analysis_task("generate a release archive"));
}

#[test]
fn plan_from_context_bundle_marks_actions_complete() {
    let bundle = ContextBundle {
        task: "audit".to_string(),
        overview: test_overview(),
        search_hits: Vec::new(),
        memories: Vec::new(),
        recommended_skills: Vec::new(),
        recent_changes: None,
        suggested_next_actions: vec![
            "Review cache health".to_string(),
            "Inspect memory reuse".to_string(),
        ],
    };

    let plan = plan_from_context_bundle(&bundle);

    assert_eq!(plan.entries.len(), 2);
    assert_eq!(plan.entries[0].status, acp::PlanEntryStatus::Completed);
    assert_eq!(plan.entries[0].priority, acp::PlanEntryPriority::High);
    assert_eq!(plan.entries[1].priority, acp::PlanEntryPriority::Medium);
}

#[test]
fn plan_from_decomposition_tracks_parallelism() {
    let decomposition = TaskDecomposition {
        task: "refactor".to_string(),
        execution_mode: "balanced".to_string(),
        prefer_full_access: true,
        can_parallelize: true,
        summary: "refactor summary".to_string(),
        recommended_starting_tools: Vec::new(),
        shared_context: Vec::new(),
        recommended_skills: Vec::new(),
        workstreams: vec![
            TaskWorkstream {
                id: "ws-1".to_string(),
                title: "State".to_string(),
                objective: "Extract shared state".to_string(),
                rationale: String::new(),
                recommended_files: Vec::new(),
                matching_symbols: Vec::new(),
                recommended_skills: Vec::new(),
                can_run_in_parallel: false,
                handoff: String::new(),
            },
            TaskWorkstream {
                id: "ws-2".to_string(),
                title: "UI".to_string(),
                objective: "Update user-facing prompts".to_string(),
                rationale: String::new(),
                recommended_files: Vec::new(),
                matching_symbols: Vec::new(),
                recommended_skills: Vec::new(),
                can_run_in_parallel: true,
                handoff: String::new(),
            },
        ],
        coordination_notes: Vec::new(),
        first_actions: Vec::new(),
    };

    let plan = plan_from_decomposition(&decomposition);

    assert_eq!(plan.entries.len(), 2);
    assert_eq!(plan.entries[0].status, acp::PlanEntryStatus::InProgress);
    assert_eq!(plan.entries[1].priority, acp::PlanEntryPriority::Medium);
}

#[test]
fn plan_from_orchestration_uses_stage_titles() {
    let orchestration = TaskOrchestration {
        task: "ship".to_string(),
        execution_mode: "balanced".to_string(),
        prefer_full_access: true,
        summary: "orchestration summary".to_string(),
        context_bundle: ContextBundle {
            task: "ship".to_string(),
            overview: test_overview(),
            search_hits: Vec::new(),
            memories: Vec::new(),
            recommended_skills: Vec::new(),
            recent_changes: None,
            suggested_next_actions: Vec::new(),
        },
        decomposition: TaskDecomposition {
            task: "ship".to_string(),
            execution_mode: "balanced".to_string(),
            prefer_full_access: true,
            can_parallelize: true,
            summary: String::new(),
            recommended_starting_tools: Vec::new(),
            shared_context: Vec::new(),
            recommended_skills: Vec::new(),
            workstreams: Vec::new(),
            coordination_notes: Vec::new(),
            first_actions: Vec::new(),
        },
        stages: vec![
            OrchestrationStage {
                id: "stage-1".to_string(),
                title: "Inspect".to_string(),
                objective: "Inspect repo state".to_string(),
                workstream_ids: vec!["ws-1".to_string()],
                run_in_parallel: false,
            },
            OrchestrationStage {
                id: "stage-2".to_string(),
                title: "Execute".to_string(),
                objective: "Run coordinated edits".to_string(),
                workstream_ids: vec!["ws-2".to_string()],
                run_in_parallel: true,
            },
        ],
        subagent_specs: Vec::new(),
        recommended_host_steps: Vec::new(),
        host_constraints: Vec::new(),
    };

    let plan = plan_from_orchestration(&orchestration);

    assert_eq!(plan.entries.len(), 2);
    assert!(plan.entries[0].content.contains("Inspect"));
    assert_eq!(plan.entries[1].priority, acp::PlanEntryPriority::Medium);
}

#[test]
fn session_title_uses_workspace_and_prefix() {
    let title = session_title(
        &PathBuf::from(r"D:\downloads\zed-codex"),
        "/plan split repo",
        Some(&PromptAction::Plan("split repo".to_string())),
    );

    assert_eq!(title, "zed-codex Plan: plan split repo");
}
