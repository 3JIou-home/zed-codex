use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context, Result};
use chrono::Utc;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;

use crate::model::{SkillCatalog, SkillMatch, SkillRecord, SkillSearchResults};
use crate::text::tokenize_query;

#[derive(Debug, Clone)]
pub struct SkillIndexOptions {
    pub roots: Vec<PathBuf>,
    pub include_globs: Vec<String>,
    pub max_skill_bytes: usize,
}

pub fn skill_index_options_signature(options: &SkillIndexOptions) -> String {
    let roots = options
        .roots
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\u{0}");
    blake3::hash(
        format!(
            "{}:{}:{}",
            roots,
            options.include_globs.join("\u{0}"),
            options.max_skill_bytes
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

pub fn build_skill_catalog(options: &SkillIndexOptions) -> Result<SkillCatalog> {
    let include_set = build_include_set(&options.include_globs)?;
    let mut skills = Vec::new();

    for root in &options.roots {
        if !root.exists() || !root.is_dir() {
            continue;
        }

        let mut walker = WalkBuilder::new(root);
        walker.standard_filters(true);
        walker.hidden(false);
        walker.follow_links(false);

        for entry in walker.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };

            if !entry
                .file_type()
                .map(|file_type| file_type.is_file())
                .unwrap_or(false)
            {
                continue;
            }

            let absolute_path = entry.path();
            let relative_path = normalize_relative_path(root, absolute_path)?;
            if should_skip_skill_path(&relative_path, &include_set) {
                continue;
            }

            let metadata = match fs::metadata(absolute_path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.len() == 0 || metadata.len() as usize > options.max_skill_bytes {
                continue;
            }

            let bytes = match fs::read(absolute_path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(skill) = parse_skill(root, &relative_path, &text) {
                skills.push(skill);
            }
        }
    }

    skills.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(SkillCatalog {
        roots: options
            .roots
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        indexed_at: Utc::now().to_rfc3339(),
        total_skills: skills.len(),
        skills,
    })
}

pub fn search_skills(
    catalog: &SkillCatalog,
    query: Option<&str>,
    limit: usize,
) -> SkillSearchResults {
    let normalized_query = query
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty());
    let tokens = normalized_query
        .as_ref()
        .map(|value| tokenize_query(value))
        .unwrap_or_default();

    let mut hits = catalog
        .skills
        .iter()
        .filter_map(|skill| {
            let (score, match_reasons) = score_skill(skill, normalized_query.as_deref(), &tokens);
            if normalized_query.is_some() && score <= 0.0 {
                return None;
            }

            Some(SkillMatch {
                id: skill.id.clone(),
                name: skill.name.clone(),
                description: skill.description.clone(),
                path: skill.path.clone(),
                source_root: skill.source_root.clone(),
                category: skill.category.clone(),
                emoji: skill.emoji.clone(),
                vibe: skill.vibe.clone(),
                preview: skill.preview.clone(),
                score,
                match_reasons,
            })
        })
        .collect::<Vec<_>>();

    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.name.cmp(&right.name))
    });

    if limit > 0 {
        hits.truncate(limit);
    }

    SkillSearchResults {
        query: normalized_query,
        indexed_at: Some(catalog.indexed_at.clone()),
        hits,
    }
}

fn build_include_set(globs: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in globs {
        builder.add(Glob::new(pattern)?);
    }
    builder
        .build()
        .context("failed to compile skill include globs")
}

fn should_skip_skill_path(relative_path: &str, include_set: &GlobSet) -> bool {
    if relative_path.starts_with(".git/")
        || relative_path.starts_with(".github/")
        || relative_path.starts_with("scripts/")
    {
        return true;
    }

    let file_name = Path::new(relative_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(file_name, "README.md" | "CONTRIBUTING.md" | "LICENSE") {
        return true;
    }

    !include_set.is_match(relative_path)
}

fn normalize_relative_path(root: &Path, absolute_path: &Path) -> Result<String> {
    let relative = absolute_path.strip_prefix(root).with_context(|| {
        format!(
            "{} is not inside {}",
            absolute_path.display(),
            root.display()
        )
    })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn parse_skill(root: &Path, relative_path: &str, text: &str) -> Option<SkillRecord> {
    let (frontmatter, body) = split_frontmatter(text);
    let name = frontmatter
        .as_ref()
        .and_then(|map| map.get("name").cloned())
        .or_else(|| first_heading(body))
        .or_else(|| {
            Path::new(relative_path)
                .file_stem()
                .and_then(|name| name.to_str())
                .map(humanize_slug)
        })?;

    let description = frontmatter
        .as_ref()
        .and_then(|map| map.get("description").cloned())
        .or_else(|| summarize_body(body))
        .unwrap_or_else(|| "External skill".to_string());
    let emoji = frontmatter
        .as_ref()
        .and_then(|map| map.get("emoji").cloned());
    let vibe = frontmatter
        .as_ref()
        .and_then(|map| map.get("vibe").cloned());

    let mut tags = Path::new(relative_path)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(|segment| segment.replace(".md", ""))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if let Some(frontmatter) = &frontmatter {
        if let Some(color) = frontmatter.get("color") {
            tags.push(color.clone());
        }
    }
    tags = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let category = relative_path
        .split('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("external")
        .to_string();

    Some(SkillRecord {
        id: blake3::hash(format!("{}::{relative_path}", root.display()).as_bytes())
            .to_hex()
            .to_string(),
        name,
        description,
        path: relative_path.to_string(),
        source_root: root.display().to_string(),
        category,
        emoji,
        vibe,
        tags,
        preview: summarize_body(body).unwrap_or_else(|| make_preview(body)),
        content: trimmed_body(body),
    })
}

fn split_frontmatter(text: &str) -> (Option<std::collections::HashMap<String, String>>, &str) {
    static FRONTMATTER_RE: OnceLock<Regex> = OnceLock::new();
    let frontmatter_re = FRONTMATTER_RE.get_or_init(|| {
        Regex::new(r"(?s)\A(?:\u{feff})?---\r?\n(?P<frontmatter>.*?)\r?\n---(?:\r?\n)?")
            .expect("frontmatter regex must compile")
    });
    let Some(captures) = frontmatter_re.captures(text) else {
        return (None, text);
    };

    let mut frontmatter = std::collections::HashMap::new();
    let raw_frontmatter = captures
        .name("frontmatter")
        .map(|capture| capture.as_str())
        .unwrap_or_default();
    for line in raw_frontmatter.lines() {
        if let Some((key, value)) = line.split_once(':') {
            frontmatter.insert(
                key.trim().to_lowercase(),
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }

    let body = text.get(captures.get(0).map(|capture| capture.end()).unwrap_or(0)..);
    (Some(frontmatter), body.unwrap_or_default())
}

fn first_heading(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("# ")
            .map(|value| value.trim().to_string())
    })
}

fn summarize_body(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#'))
        .find(|line| line.len() > 20)
        .map(|line| {
            let mut line = line.to_string();
            if line.len() > 220 {
                line.truncate(220);
                line.push_str("...");
            }
            line
        })
}

fn make_preview(text: &str) -> String {
    let mut preview = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("---"))
        .take(8)
        .collect::<Vec<_>>()
        .join(" | ");
    if preview.len() > 320 {
        preview.truncate(320);
        preview.push_str("...");
    }
    preview
}

fn trimmed_body(text: &str) -> String {
    let mut body = text.trim().to_string();
    if body.len() > 8_000 {
        body.truncate(8_000);
        body.push_str("\n...");
    }
    body
}

fn humanize_slug(slug: &str) -> String {
    slug.replace('-', " ")
        .split_whitespace()
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_any_token(tokens: &[String], candidates: &[&str]) -> bool {
    tokens
        .iter()
        .any(|token| candidates.iter().any(|candidate| token == candidate))
}

fn score_skill_intent(category: &str, path: &str, tokens: &[String]) -> (f64, Vec<String>) {
    let technical = has_any_token(
        tokens,
        &[
            "agent",
            "agents",
            "analysis",
            "benchmark",
            "bug",
            "cache",
            "cli",
            "codex",
            "code",
            "debug",
            "decompose",
            "decomposition",
            "extension",
            "fix",
            "index",
            "mcp",
            "performance",
            "plan",
            "planning",
            "plugin",
            "prompt",
            "refactor",
            "repo",
            "review",
            "rust",
            "server",
            "skill",
            "skills",
            "test",
            "testing",
            "workflow",
            "zed",
        ],
    );
    let planning = has_any_token(
        tokens,
        &[
            "breakdown",
            "decompose",
            "decomposition",
            "handoff",
            "parallel",
            "plan",
            "planning",
            "scope",
            "task",
            "tasks",
            "workflow",
        ],
    );
    let performance = has_any_token(
        tokens,
        &[
            "benchmark",
            "cache",
            "index",
            "latency",
            "performance",
            "speed",
        ],
    );
    let design = has_any_token(tokens, &["design", "prototype", "ui", "ux"]);
    let docs = has_any_token(
        tokens,
        &["doc", "docs", "documentation", "guide", "tutorial"],
    );

    let mut score = 0.0;
    let mut reasons = Vec::new();

    if technical {
        match category {
            "engineering" => {
                score += 10.0;
                reasons.push("intent:technical:engineering".to_string());
            }
            "testing" => {
                score += 9.0;
                reasons.push("intent:technical:testing".to_string());
            }
            "project-management" => {
                score += 6.0;
                reasons.push("intent:technical:project-management".to_string());
            }
            "marketing" | "sales" | "paid-media" => {
                score -= 4.0;
                reasons.push("penalty:nontechnical-category".to_string());
            }
            _ => {}
        }
    }

    if planning {
        match category {
            "project-management" => {
                score += 8.0;
                reasons.push("intent:planning:project-management".to_string());
            }
            "engineering" => {
                score += 3.0;
                reasons.push("intent:planning:engineering".to_string());
            }
            "testing" => {
                score += 2.0;
                reasons.push("intent:planning:testing".to_string());
            }
            _ => {}
        }
        if path.contains("workflow") {
            score += 3.0;
            reasons.push("intent:planning:workflow".to_string());
        }
    }

    if performance {
        match category {
            "testing" => {
                score += 8.0;
                reasons.push("intent:performance:testing".to_string());
            }
            "engineering" => {
                score += 4.0;
                reasons.push("intent:performance:engineering".to_string());
            }
            _ => {}
        }
    }

    if design && category == "design" {
        score += 8.0;
        reasons.push("intent:design".to_string());
    }

    if docs {
        match category {
            "project-management" | "support" | "product" => {
                score += 4.0;
                reasons.push("intent:docs".to_string());
            }
            _ => {}
        }
    }

    (score, reasons)
}

fn score_skill(
    skill: &SkillRecord,
    normalized_query: Option<&str>,
    tokens: &[String],
) -> (f64, Vec<String>) {
    let name = skill.name.to_lowercase();
    let description = skill.description.to_lowercase();
    let path = skill.path.to_lowercase();
    let category = skill.category.to_lowercase();
    let tags = skill.tags.join(" ").to_lowercase();
    let vibe = skill.vibe.clone().unwrap_or_default().to_lowercase();
    let content = skill.content.to_lowercase();

    let mut score = 0.0;
    let mut reasons = Vec::new();

    if let Some(query) = normalized_query {
        if name.contains(query) {
            score += 20.0;
            reasons.push("name".to_string());
        }
        if description.contains(query) {
            score += 14.0;
            reasons.push("description".to_string());
        }
        if category.contains(query) {
            score += 12.0;
            reasons.push("category".to_string());
        }
        if path.contains(query) {
            score += 10.0;
            reasons.push("path".to_string());
        }
        if tags.contains(query) {
            score += 10.0;
            reasons.push("tags".to_string());
        }
        if content.contains(query) {
            score += 6.0;
            reasons.push("content".to_string());
        }
    }

    for token in tokens {
        if name.contains(token) {
            score += 8.0;
            reasons.push(format!("token:name:{token}"));
        }
        if description.contains(token) {
            score += 5.0;
            reasons.push(format!("token:description:{token}"));
        }
        if category.contains(token) || path.contains(token) || tags.contains(token) {
            score += 4.0;
            reasons.push(format!("token:path:{token}"));
        }
        if vibe.contains(token) || content.contains(token) {
            score += 2.0;
        }
    }

    let (intent_score, intent_reasons) = score_skill_intent(&category, &path, tokens);
    score += intent_score;
    reasons.extend(intent_reasons);

    reasons.sort();
    reasons.dedup();
    (score, reasons)
}

#[cfg(test)]
mod tests;
