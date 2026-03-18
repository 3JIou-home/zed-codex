use std::collections::{HashMap, HashSet};

use crate::{
    config::ServerConfig,
    model::{
        ContextBundle, OrchestrationStage, SearchHit, SkillMatch, SubagentSpec, TaskDecomposition,
        TaskOrchestration, TaskWorkstream,
    },
};

pub(crate) fn build_suggested_next_actions(
    task: &str,
    search_hits: &[SearchHit],
    recommended_skills: &[SkillMatch],
    execution_mode: &str,
    prefer_full_access: bool,
) -> Vec<String> {
    let mut actions = vec![
        "Use warm_workspace at the start of a fresh session to prime cache, git, and memories."
            .to_string(),
    ];

    if !search_hits.is_empty() {
        actions.push("Open the highest-scoring files from search hits before editing.".to_string());
    }

    if search_hits.len() > 3 || task.split_whitespace().count() > 8 {
        actions.push(
            "Call orchestrate_task first so skills, context, workstreams, and delegate briefs stay aligned in one result."
                .to_string(),
        );
    }

    if !recommended_skills.is_empty() {
        actions.push(
            "Use orchestrate_task or search_skills before planning implementation details so each workstream is grounded in a concrete skill match."
                .to_string(),
        );
    }

    if prefer_full_access {
        actions.push(
            "If the host offers full-access or auto-approved tools for a trusted workspace, enable it before heavy edits, tests, or multi-file refactors."
                .to_string(),
        );
    }

    actions.push(format!(
        "Execution mode is `{execution_mode}`: adapt pacing and autonomy to match that preference."
    ));
    actions.push(
        "Write durable architectural or workflow decisions with remember_memory.".to_string(),
    );
    actions
}

pub(crate) fn build_workstream_skill_query(
    task: &str,
    workstream: &TaskWorkstream,
) -> Option<String> {
    let mut parts = vec![task.trim().to_string(), workstream.title.clone()];

    if !workstream.objective.trim().is_empty() {
        parts.push(workstream.objective.clone());
    }
    if let Some(file) = workstream.recommended_files.first() {
        parts.push(file.clone());
    }
    if let Some(symbol) = workstream.matching_symbols.first() {
        parts.push(symbol.clone());
    }

    let query = parts
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if query.is_empty() {
        None
    } else {
        Some(query)
    }
}

pub(crate) fn fallback_workstream_skills(
    recommended_skills: &[SkillMatch],
    workstream: &TaskWorkstream,
) -> Vec<SkillMatch> {
    if !workstream.recommended_skills.is_empty() {
        let wanted = workstream
            .recommended_skills
            .iter()
            .map(|name| name.to_lowercase())
            .collect::<HashSet<_>>();
        let matches = recommended_skills
            .iter()
            .filter(|skill| wanted.contains(&skill.name.to_lowercase()))
            .cloned()
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return matches;
        }
    }

    recommended_skills.iter().take(2).cloned().collect()
}

pub(crate) fn merge_skill_matches(
    primary: Vec<SkillMatch>,
    fallback: Vec<SkillMatch>,
    limit: usize,
) -> Vec<SkillMatch> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();

    for skill in primary.into_iter().chain(fallback) {
        let key = if skill.id.trim().is_empty() {
            skill.path.clone()
        } else {
            skill.id.clone()
        };
        if seen.insert(key) {
            merged.push(skill);
        }
        if merged.len() >= limit {
            break;
        }
    }

    merged
}

fn candidate_scope_depth(path: &str) -> usize {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    segments.len().clamp(1, 3)
}

fn derive_workstream_scope(path: &str, depth: usize) -> String {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() {
        return ".".to_string();
    }
    if segments.len() == 1 {
        return segments[0].to_string();
    }

    let directory_segments = &segments[..segments.len() - 1];
    if directory_segments.is_empty() {
        return segments[0].to_string();
    }

    let clamped_depth = depth.max(1);
    if clamped_depth > directory_segments.len() {
        return path.to_string();
    }

    directory_segments[..clamped_depth].join("/")
}

fn group_hits_for_workstreams(search_hits: &[SearchHit]) -> Vec<(String, Vec<SearchHit>)> {
    let max_depth = search_hits
        .iter()
        .map(|hit| candidate_scope_depth(&hit.path))
        .max()
        .unwrap_or(1);

    let mut best_groups = HashMap::<String, Vec<SearchHit>>::new();
    for depth in 1..=max_depth {
        let mut groups = HashMap::<String, Vec<SearchHit>>::new();
        for hit in search_hits {
            let scope = derive_workstream_scope(&hit.path, depth);
            groups.entry(scope).or_default().push(hit.clone());
        }

        if groups.len() >= best_groups.len() {
            best_groups = groups;
        }
        if best_groups.len() > 1 {
            break;
        }
    }

    best_groups.into_iter().collect()
}

pub(crate) fn build_task_decomposition(
    bundle: &ContextBundle,
    config: &ServerConfig,
) -> TaskDecomposition {
    let mut scopes = group_hits_for_workstreams(&bundle.search_hits);
    scopes.sort_by(|left, right| {
        let left_score = left.1.iter().map(|hit| hit.score).sum::<f64>();
        let right_score = right.1.iter().map(|hit| hit.score).sum::<f64>();
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut workstreams = Vec::new();
    for (index, (scope, hits)) in scopes
        .into_iter()
        .take(config.max_parallel_workstreams)
        .enumerate()
    {
        let representative = hits.first().cloned();
        let mut recommended_files = hits.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
        recommended_files.sort();
        recommended_files.dedup();
        recommended_files.truncate(5);

        let mut matching_symbols = hits
            .iter()
            .flat_map(|hit| hit.matching_symbols.clone())
            .collect::<Vec<_>>();
        matching_symbols.sort();
        matching_symbols.dedup();
        matching_symbols.truncate(8);

        let can_run_in_parallel = index > 0
            && scope != "."
            && recommended_files
                .iter()
                .all(|path| !is_likely_shared_file(path));

        let scope_query = scope.to_lowercase();
        let recommended_skills = bundle
            .recommended_skills
            .iter()
            .filter(|skill| {
                skill.category.to_lowercase().contains(&scope_query)
                    || skill.path.to_lowercase().contains(&scope_query)
                    || skill.description.to_lowercase().contains(&scope_query)
            })
            .take(2)
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>();

        let objective = representative
            .as_ref()
            .map(|hit| {
                format!(
                    "Advance `{}` by focusing on `{}` and the files most relevant to that scope.",
                    bundle.task, hit.path
                )
            })
            .unwrap_or_else(|| format!("Advance `{}` inside scope `{scope}`.", bundle.task));

        let rationale = representative
            .as_ref()
            .map(|hit| {
                format!(
                    "This scope matched the task strongly through `{}` with score {:.1}.",
                    hit.path, hit.score
                )
            })
            .unwrap_or_else(|| {
                "This scope groups the strongest matching files for the task.".to_string()
            });

        let handoff = format!(
            "Own the `{scope}` slice for task `{}`. Stay within {:?} when possible, note cross-scope contracts before editing shared files, and leave durable decisions in remember_memory.",
            bundle.task, recommended_files
        );

        workstreams.push(TaskWorkstream {
            id: format!("ws-{}", index + 1),
            title: if scope == "." {
                "Root-level coordination".to_string()
            } else {
                format!("{} workstream", scope)
            },
            objective,
            rationale,
            recommended_files,
            matching_symbols,
            recommended_skills,
            can_run_in_parallel,
            handoff,
        });
    }

    if workstreams.is_empty() {
        workstreams.push(TaskWorkstream {
            id: "ws-1".to_string(),
            title: "Primary implementation path".to_string(),
            objective: format!("Advance `{}` with a single focused pass.", bundle.task),
            rationale:
                "No strong file clusters were found, so a single exploratory stream is safer."
                    .to_string(),
            recommended_files: bundle.overview.key_files.iter().take(5).cloned().collect(),
            matching_symbols: Vec::new(),
            recommended_skills: bundle
                .recommended_skills
                .iter()
                .take(2)
                .map(|skill| skill.name.clone())
                .collect(),
            can_run_in_parallel: false,
            handoff: format!(
                "Drive the task `{}` end-to-end, then record durable findings in remember_memory.",
                bundle.task
            ),
        });
    }

    let can_parallelize = workstreams
        .iter()
        .filter(|stream| stream.can_run_in_parallel)
        .count()
        > 0;
    let mut shared_context = bundle
        .overview
        .key_files
        .iter()
        .take(6)
        .cloned()
        .collect::<Vec<_>>();
    for memory in &bundle.memories {
        if shared_context.len() >= 10 {
            break;
        }
        shared_context.push(format!("memory: {}", memory.title));
    }
    shared_context.dedup();

    let mut coordination_notes = vec![
        "Lock interfaces or file ownership before multiple workstreams edit neighboring modules."
            .to_string(),
        "Use remember_memory for durable decisions so the next session does not repeat discovery."
            .to_string(),
    ];
    if can_parallelize {
        coordination_notes.push(
            "If the host supports subagents, delegate only streams whose recommended files do not overlap."
                .to_string(),
        );
    }
    if config.prefer_full_access {
        coordination_notes.push(
            "Full-access or auto-approved mode is advisory only here: the actual permission boundary is controlled by the host agent, not by the MCP companion."
                .to_string(),
        );
    }

    let mut first_actions = vec![
        "Run warm_workspace to ensure index, git, and memory caches are warm.".to_string(),
        "Inspect the first workstream's recommended files and confirm the smallest safe edit surface.".to_string(),
    ];
    if bundle.search_hits.len() > 3 || bundle.task.split_whitespace().count() > 8 {
        first_actions.push(
            "Keep the decomposition visible while working so independent slices can be handled in parallel when the host allows it."
                .to_string(),
        );
    }
    if config.prefer_full_access {
        first_actions.push(
            "If the workspace is trusted and the host exposes it, switch to full-access or auto-approve before large edits or test runs."
                .to_string(),
        );
    }
    if !bundle.recommended_skills.is_empty() {
        first_actions.push(
            "Map the first workstream to one of the recommended external skills before implementation so the plan is grounded in a concrete playbook."
                .to_string(),
        );
    }

    let delegate_ready_count = workstreams
        .iter()
        .filter(|workstream| workstream.can_run_in_parallel)
        .count();

    TaskDecomposition {
        task: bundle.task.clone(),
        execution_mode: config.execution_mode.clone(),
        prefer_full_access: config.prefer_full_access,
        can_parallelize,
        summary: format!(
            "Task decomposed into {} workstream(s), with {} delegate-ready slice(s), using cached search hits, memories, and repo state. Execution mode is `{}`.",
            workstreams.len(),
            delegate_ready_count,
            config.execution_mode
        ),
        recommended_starting_tools: vec![
            "orchestrate_task".to_string(),
            "warm_workspace".to_string(),
            "build_context_bundle".to_string(),
            "decompose_task".to_string(),
            "search_workspace".to_string(),
            "search_skills".to_string(),
            "remember_memory".to_string(),
        ],
        shared_context,
        recommended_skills: bundle.recommended_skills.clone(),
        workstreams,
        coordination_notes,
        first_actions,
    }
}

fn agent_role_for_workstream(index: usize, workstream: &TaskWorkstream) -> &'static str {
    if index == 0 {
        "coordinator"
    } else if workstream.can_run_in_parallel {
        "parallel-specialist"
    } else {
        "specialist"
    }
}

fn build_completion_criteria(workstream: &TaskWorkstream) -> Vec<String> {
    let mut criteria = vec![
        format!(
            "Advance the workstream objective while staying primarily inside {:?}.",
            workstream.recommended_files
        ),
        "Call out cross-scope contracts before touching shared files or interfaces.".to_string(),
        "Leave durable workflow or architecture decisions in remember_memory if another session will need them."
            .to_string(),
    ];

    if !workstream.matching_symbols.is_empty() {
        criteria.push(format!(
            "Verify the behavior around symbols {:?} after the change.",
            workstream.matching_symbols
        ));
    }

    criteria
}

fn build_subagent_prompt(
    task: &str,
    workstream: &TaskWorkstream,
    recommended_skills: &[SkillMatch],
    shared_context: &[String],
    execution_mode: &str,
    prefer_full_access: bool,
    agent_role: &str,
) -> String {
    let skill_paths = recommended_skills
        .iter()
        .map(|skill| format!("{} ({})", skill.name, skill.path))
        .collect::<Vec<_>>();
    let access_hint = if prefer_full_access {
        "If the host exposes a trusted full-access mode, you may use it for this slice after the initial read-only scan."
    } else {
        "Stay inside the host's default approval boundary unless this slice clearly needs more access."
    };

    format!(
        "Role: {agent_role}. Task: {task}. Workstream: {} [{}]. Objective: {}. Prioritize files: {:?}. Matching symbols: {:?}. Shared context: {:?}. Load these skills first: {:?}. Handoff: {}. Execution mode: {execution_mode}. {}",
        workstream.title,
        workstream.id,
        workstream.objective,
        workstream.recommended_files,
        workstream.matching_symbols,
        shared_context,
        skill_paths,
        workstream.handoff,
        access_hint
    )
}

pub(crate) fn build_task_orchestration(
    bundle: ContextBundle,
    decomposition: TaskDecomposition,
    workstream_skill_matches: &HashMap<String, Vec<SkillMatch>>,
    config: &ServerConfig,
) -> TaskOrchestration {
    let mut stages = Vec::new();
    if let Some(primary) = decomposition.workstreams.first() {
        stages.push(OrchestrationStage {
            id: "stage-1".to_string(),
            title: "Coordinator setup".to_string(),
            objective: "Open shared context, confirm file ownership, and establish any cross-scope contracts before fan-out."
                .to_string(),
            workstream_ids: vec![primary.id.clone()],
            run_in_parallel: false,
        });
    }

    let parallel_ids = decomposition
        .workstreams
        .iter()
        .skip(1)
        .filter(|workstream| workstream.can_run_in_parallel)
        .map(|workstream| workstream.id.clone())
        .collect::<Vec<_>>();
    if !parallel_ids.is_empty() {
        stages.push(OrchestrationStage {
            id: format!("stage-{}", stages.len() + 1),
            title: "Parallel implementation".to_string(),
            objective: "Delegate independent workstreams whose files do not overlap once the coordinator has locked scope boundaries."
                .to_string(),
            workstream_ids: parallel_ids,
            run_in_parallel: true,
        });
    }

    let sequential_ids = decomposition
        .workstreams
        .iter()
        .skip(1)
        .filter(|workstream| !workstream.can_run_in_parallel)
        .map(|workstream| workstream.id.clone())
        .collect::<Vec<_>>();
    if !sequential_ids.is_empty() {
        stages.push(OrchestrationStage {
            id: format!("stage-{}", stages.len() + 1),
            title: "Sequential follow-up".to_string(),
            objective: "Handle overlapping or coordination-heavy slices after the delegate-ready work is merged."
                .to_string(),
            workstream_ids: sequential_ids,
            run_in_parallel: false,
        });
    }

    let subagent_specs = decomposition
        .workstreams
        .iter()
        .enumerate()
        .map(|(index, workstream)| {
            let agent_role = agent_role_for_workstream(index, workstream).to_string();
            let recommended_skills = workstream_skill_matches
                .get(&workstream.id)
                .cloned()
                .unwrap_or_default();
            let completion_criteria = build_completion_criteria(workstream);
            let prompt = build_subagent_prompt(
                &bundle.task,
                workstream,
                &recommended_skills,
                &decomposition.shared_context,
                &config.execution_mode,
                config.prefer_full_access,
                &agent_role,
            );

            SubagentSpec {
                workstream_id: workstream.id.clone(),
                title: workstream.title.clone(),
                agent_role,
                run_in_parallel: workstream.can_run_in_parallel,
                objective: workstream.objective.clone(),
                recommended_files: workstream.recommended_files.clone(),
                matching_symbols: workstream.matching_symbols.clone(),
                shared_context: decomposition.shared_context.clone(),
                recommended_skills,
                completion_criteria,
                handoff: workstream.handoff.clone(),
                prompt,
            }
        })
        .collect::<Vec<_>>();

    let parallel_ready_count = subagent_specs
        .iter()
        .filter(|spec| spec.run_in_parallel)
        .count();
    let recommended_host_steps = vec![
        "Run `warm_workspace` first if the cache is cold or the workspace has changed materially."
            .to_string(),
        "Start with the `coordinator` subagent spec so shared context and file ownership are locked before any fan-out."
            .to_string(),
        "Open the recommended skill paths attached to each subagent spec before implementation so each slice follows a concrete playbook."
            .to_string(),
        "Only fan out the `parallel-specialist` specs together, and only if the host actually supports subagents and tool approvals are already settled."
            .to_string(),
        "Merge results back through the coordinator before editing shared files, running repo-wide checks, or finalizing the handoff."
            .to_string(),
        "Write durable decisions with `remember_memory` once the orchestration completes."
            .to_string(),
    ];

    let mut host_constraints = decomposition.coordination_notes.clone();
    host_constraints.push(
        "The module now defines subagent-ready briefs, but the host still decides whether those briefs become actual parallel agents."
            .to_string(),
    );

    TaskOrchestration {
        task: bundle.task.clone(),
        execution_mode: config.execution_mode.clone(),
        prefer_full_access: config.prefer_full_access,
        summary: format!(
            "Prepared {} orchestration stage(s) and {} subagent spec(s), with {} parallel-ready delegate(s).",
            stages.len(),
            subagent_specs.len(),
            parallel_ready_count
        ),
        context_bundle: bundle,
        decomposition,
        stages,
        subagent_specs,
        recommended_host_steps,
        host_constraints,
    }
}

fn is_likely_shared_file(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_lowercase();
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());

    if !normalized.contains('/') || normalized.starts_with(".github/") {
        return true;
    }

    if normalized.contains("/shared/")
        || normalized.contains("/common/")
        || normalized.contains("/types/")
        || normalized.contains("/interfaces/")
    {
        return true;
    }

    matches!(
        file_name,
        "cargo.toml"
            | "package.json"
            | "pnpm-workspace.yaml"
            | "readme.md"
            | "readme"
            | "tsconfig.json"
            | "pyproject.toml"
            | "lib.rs"
            | "main.rs"
            | "mod.rs"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.jsx"
            | "types.ts"
            | "types.rs"
            | "config.ts"
            | "config.rs"
            | "settings.py"
            | "__init__.py"
            | "schema.prisma"
    )
}
