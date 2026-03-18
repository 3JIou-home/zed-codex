use super::{build_skill_catalog, search_skills, split_frontmatter, SkillIndexOptions};
use crate::model::{SkillCatalog, SkillRecord};
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn catalog_indexes_markdown_agents() {
    let temp_root = std::env::temp_dir().join(format!(
        "codex-companion-skills-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(temp_root.join("engineering")).unwrap();
    fs::write(
        temp_root.join("engineering").join("rapid-prototyper.md"),
        "---\nname: Rapid Prototyper\ndescription: Build MVPs fast\nemoji: ⚡\n---\n# Rapid Prototyper\n\nBuild fast.\n",
    )
    .unwrap();

    let catalog = build_skill_catalog(&SkillIndexOptions {
        roots: vec![temp_root.clone()],
        include_globs: vec!["**/*.md".to_string()],
        max_skill_bytes: 32_768,
    })
    .unwrap();

    assert_eq!(catalog.total_skills, 1);
    assert_eq!(catalog.skills[0].name, "Rapid Prototyper");
    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn skill_search_prefers_name_matches() {
    let catalog = build_skill_catalog(&SkillIndexOptions {
        roots: Vec::new(),
        include_globs: vec!["**/*.md".to_string()],
        max_skill_bytes: 32_768,
    })
    .unwrap();

    let manual_catalog = SkillCatalog {
        roots: Vec::new(),
        indexed_at: "2026-03-17T00:00:00Z".to_string(),
        total_skills: 2,
        skills: vec![
            SkillRecord {
                id: "1".to_string(),
                name: "Rapid Prototyper".to_string(),
                description: "Build MVPs quickly".to_string(),
                path: "engineering/rapid-prototyper.md".to_string(),
                source_root: "/tmp".to_string(),
                category: "engineering".to_string(),
                emoji: None,
                vibe: None,
                tags: vec!["engineering".to_string()],
                preview: "Build fast".to_string(),
                content: "prototype fast".to_string(),
            },
            SkillRecord {
                id: "2".to_string(),
                name: "Technical Writer".to_string(),
                description: "Write docs".to_string(),
                path: "engineering/technical-writer.md".to_string(),
                source_root: "/tmp".to_string(),
                category: "engineering".to_string(),
                emoji: None,
                vibe: None,
                tags: vec!["engineering".to_string()],
                preview: "Write".to_string(),
                content: "docs".to_string(),
            },
        ],
    };

    let results = search_skills(&manual_catalog, Some("rapid prototype"), 5);
    assert_eq!(
        results.hits.first().map(|hit| hit.name.as_str()),
        Some("Rapid Prototyper")
    );
    assert!(catalog.total_skills == 0);
}

#[test]
fn skill_search_boosts_technical_categories_for_technical_queries() {
    let catalog = SkillCatalog {
        roots: Vec::new(),
        indexed_at: "2026-03-17T00:00:00Z".to_string(),
        total_skills: 3,
        skills: vec![
            SkillRecord {
                id: "eng".to_string(),
                name: "Code Reviewer".to_string(),
                description: "Reviews code quality and performance".to_string(),
                path: "engineering/engineering-code-reviewer.md".to_string(),
                source_root: "/tmp".to_string(),
                category: "engineering".to_string(),
                emoji: None,
                vibe: None,
                tags: vec!["engineering".to_string()],
                preview: "Reviews code".to_string(),
                content: "Code review for Rust plugins".to_string(),
            },
            SkillRecord {
                id: "pm".to_string(),
                name: "Senior Project Manager".to_string(),
                description: "Converts specs to tasks and plans handoffs".to_string(),
                path: "project-management/project-manager-senior.md".to_string(),
                source_root: "/tmp".to_string(),
                category: "project-management".to_string(),
                emoji: None,
                vibe: None,
                tags: vec!["project-management".to_string()],
                preview: "Plans tasks".to_string(),
                content: "Breaks work into streams".to_string(),
            },
            SkillRecord {
                id: "mkt".to_string(),
                name: "Growth Hacker".to_string(),
                description: "Improves campaign performance and growth planning".to_string(),
                path: "marketing/marketing-growth-hacker.md".to_string(),
                source_root: "/tmp".to_string(),
                category: "marketing".to_string(),
                emoji: None,
                vibe: None,
                tags: vec!["marketing".to_string()],
                preview: "Marketing growth".to_string(),
                content: "Campaign planning".to_string(),
            },
        ],
    };

    let results = search_skills(&catalog, Some("rust plugin planning performance"), 3);
    let names = results
        .hits
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names.first().copied(), Some("Code Reviewer"));
    assert!(names.contains(&"Senior Project Manager"));
    assert_ne!(names.first().copied(), Some("Growth Hacker"));
}

#[test]
fn split_frontmatter_supports_crlf() {
    let (frontmatter, body) = split_frontmatter(
        "---\r\nname: Rapid Prototyper\r\ndescription: Build MVPs fast\r\n---\r\n# Title\r\n\r\nBody\r\n",
    );

    let frontmatter = frontmatter.expect("frontmatter should parse");
    assert_eq!(
        frontmatter.get("name").map(String::as_str),
        Some("Rapid Prototyper")
    );
    assert!(body.starts_with("# Title"));
}
