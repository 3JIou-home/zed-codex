use std::{path::Path, process::Command};

use crate::model::GitSummary;

pub fn collect_git_summary(
    root: &Path,
    limit_commits: usize,
    include_diffstat: bool,
) -> GitSummary {
    let status_output = run_git(root, &["status", "--short", "--branch"]);
    let status_text = match status_output {
        Ok(text) => text,
        Err(message) => {
            return GitSummary {
                available: false,
                branch: None,
                status_lines: Vec::new(),
                recent_commits: Vec::new(),
                diff_stats: Vec::new(),
                message: Some(message),
            }
        }
    };

    let mut lines = status_text.lines();
    let branch = lines.next().map(|line| line.trim().to_string());
    let status_lines = lines
        .take(20)
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let recent_commits = run_git(root, &["log", "--oneline", &format!("-n{limit_commits}")])
        .ok()
        .map(|text| {
            text.lines()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let diff_stats = if include_diffstat {
        run_git(root, &["diff", "--stat", "HEAD"])
            .ok()
            .map(|text| {
                text.lines()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    GitSummary {
        available: true,
        branch,
        status_lines,
        recent_commits,
        diff_stats,
        message: None,
    }
}

fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("git is unavailable: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(if detail.is_empty() {
            "git returned a non-zero exit status".to_string()
        } else {
            detail
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
