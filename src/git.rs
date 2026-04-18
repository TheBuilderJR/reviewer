use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::shell::run_command;
use crate::types::HistoricalCommit;

const GIT_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    review_ref: String,
}

pub async fn fetch_pr_head_ref(repo_path: &Path, pr_number: u64) -> Result<String> {
    let review_ref = format!("refs/remotes/origin/reviewer-harness/pr-{pr_number}");
    let fetch_base = vec![
        "fetch".to_string(),
        "origin".to_string(),
        format!("refs/pull/{pr_number}/head:{review_ref}"),
    ];
    run_command(
        "git",
        &prefix(repo_path, fetch_base),
        repo_path,
        GIT_TIMEOUT_SECS,
    )
    .await
    .with_context(|| format!("failed fetching PR #{pr_number}"))?;

    Ok(review_ref)
}

pub async fn create_pr_worktree(
    repo_path: &Path,
    pr_number: u64,
    review_ref: &str,
) -> Result<Worktree> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let worktree_path = std::env::temp_dir().join(format!("reviewer-pr-{pr_number}-{stamp}"));

    let add_args = vec![
        "worktree".to_string(),
        "add".to_string(),
        "--detach".to_string(),
        worktree_path.display().to_string(),
        review_ref.to_string(),
    ];
    run_command(
        "git",
        &prefix(repo_path, add_args),
        repo_path,
        GIT_TIMEOUT_SECS,
    )
    .await
    .context("failed creating worktree")?;

    Ok(Worktree {
        path: worktree_path,
        review_ref: review_ref.to_string(),
    })
}

pub async fn cleanup_worktree(repo_path: &Path, worktree: &Worktree) -> Result<()> {
    let remove_args = vec![
        "worktree".to_string(),
        "remove".to_string(),
        "--force".to_string(),
        worktree.path.display().to_string(),
    ];
    run_command(
        "git",
        &prefix(repo_path, remove_args),
        repo_path,
        GIT_TIMEOUT_SECS,
    )
    .await
    .context("failed removing worktree")?;

    let update_ref_args = vec![
        "update-ref".to_string(),
        "-d".to_string(),
        worktree.review_ref.clone(),
    ];
    let _ = run_command(
        "git",
        &prefix(repo_path, update_ref_args),
        repo_path,
        GIT_TIMEOUT_SECS,
    )
    .await;
    Ok(())
}

pub async fn fetch_base_branch(repo_path: &Path, base_ref: &str) -> Result<()> {
    let args = vec![
        "fetch".to_string(),
        "origin".to_string(),
        base_ref.to_string(),
    ];
    run_command("git", &prefix(repo_path, args), repo_path, GIT_TIMEOUT_SECS)
        .await
        .with_context(|| format!("failed fetching base branch {base_ref}"))?;
    Ok(())
}

pub async fn diff_for_file(worktree_path: &Path, base_ref: &str, file: &str) -> Result<String> {
    let args = vec![
        "-C".to_string(),
        worktree_path.display().to_string(),
        "diff".to_string(),
        "--unified=40".to_string(),
        format!("origin/{base_ref}...HEAD"),
        "--".to_string(),
        file.to_string(),
    ];
    Ok(run_command("git", &args, worktree_path, GIT_TIMEOUT_SECS)
        .await?
        .stdout)
}

pub async fn recent_commits_for_file(
    worktree_path: &Path,
    base_ref: &str,
    file: &str,
    max_results: usize,
) -> Result<Vec<HistoricalCommit>> {
    if max_results == 0 {
        return Ok(Vec::new());
    }

    let args = vec![
        "-C".to_string(),
        worktree_path.display().to_string(),
        "log".to_string(),
        format!("origin/{base_ref}"),
        "--follow".to_string(),
        format!("-n{max_results}"),
        "--format=%H%x1f%s%x1f%ct".to_string(),
        "--".to_string(),
        file.to_string(),
    ];
    let output = run_command("git", &args, worktree_path, GIT_TIMEOUT_SECS)
        .await
        .with_context(|| format!("failed listing commits for {file}"))?;

    let mut commits = Vec::new();
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split('\u{1f}');
        let sha = match parts.next() {
            Some(value) => value.to_string(),
            None => continue,
        };
        let title = match parts.next() {
            Some(value) => value.to_string(),
            None => continue,
        };
        let unix_time = parts
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or_default();

        let show_args = vec![
            "-C".to_string(),
            worktree_path.display().to_string(),
            "show".to_string(),
            "--stat".to_string(),
            "--patch".to_string(),
            "--unified=20".to_string(),
            "--format=fuller".to_string(),
            sha.clone(),
            "--".to_string(),
            file.to_string(),
        ];

        let patch_excerpt = run_command("git", &show_args, worktree_path, GIT_TIMEOUT_SECS)
            .await
            .map(|output| excerpt(&output.stdout, 10_000))
            .unwrap_or_default();

        commits.push(HistoricalCommit {
            sha,
            title,
            unix_time,
            patch_excerpt,
        });
    }

    Ok(commits)
}

fn prefix(repo_path: &Path, args: Vec<String>) -> Vec<String> {
    let mut prefixed = vec!["-C".to_string(), repo_path.display().to_string()];
    prefixed.extend(args);
    prefixed
}

fn excerpt(value: &str, max_chars: usize) -> String {
    let chars = value.chars().count();
    if chars <= max_chars {
        return value.trim().to_string();
    }

    let head: String = value.chars().take(max_chars * 2 / 3).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(max_chars / 3)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    format!("{head}\n...\n{tail}")
}
