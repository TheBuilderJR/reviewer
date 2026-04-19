use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::progress::ProgressReporter;
use crate::shell::{CommandProgress, run_command_reported};

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    review_ref: String,
}

pub async fn fetch_pr_head_ref(
    repo_path: &Path,
    pr_number: u64,
    progress: Arc<ProgressReporter>,
) -> Result<String> {
    let review_ref = format!("refs/remotes/origin/reviewer-harness/pr-{pr_number}");
    let fetch_base = vec![
        "fetch".to_string(),
        "origin".to_string(),
        format!("refs/pull/{pr_number}/head:{review_ref}"),
    ];
    run_command_reported(
        "git",
        &prefix(repo_path, fetch_base),
        repo_path,
        CommandProgress::new(progress, format!("git fetch PR #{pr_number} head")),
    )
    .await
    .with_context(|| format!("failed fetching PR #{pr_number}"))?;

    Ok(review_ref)
}

pub async fn is_git_repo(repo_path: &Path, progress: Arc<ProgressReporter>) -> bool {
    let args = vec![
        "-C".to_string(),
        repo_path.display().to_string(),
        "rev-parse".to_string(),
        "--is-inside-work-tree".to_string(),
    ];
    run_command_reported(
        "git",
        &args,
        repo_path,
        CommandProgress::new(progress, "git rev-parse --is-inside-work-tree"),
    )
    .await
    .is_ok()
}

pub async fn create_pr_worktree(
    repo_path: &Path,
    pr_number: u64,
    review_ref: &str,
    progress: Arc<ProgressReporter>,
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
    run_command_reported(
        "git",
        &prefix(repo_path, add_args),
        repo_path,
        CommandProgress::new(progress, format!("git worktree add for PR #{pr_number}")),
    )
    .await
    .context("failed creating worktree")?;

    Ok(Worktree {
        path: worktree_path,
        review_ref: review_ref.to_string(),
    })
}

pub async fn cleanup_worktree(
    repo_path: &Path,
    worktree: &Worktree,
    progress: Arc<ProgressReporter>,
) -> Result<()> {
    let remove_args = vec![
        "worktree".to_string(),
        "remove".to_string(),
        "--force".to_string(),
        worktree.path.display().to_string(),
    ];
    run_command_reported(
        "git",
        &prefix(repo_path, remove_args),
        repo_path,
        CommandProgress::new(progress.clone(), "git worktree remove --force"),
    )
    .await
    .context("failed removing worktree")?;

    let update_ref_args = vec![
        "update-ref".to_string(),
        "-d".to_string(),
        worktree.review_ref.clone(),
    ];
    let _ = run_command_reported(
        "git",
        &prefix(repo_path, update_ref_args),
        repo_path,
        CommandProgress::new(progress, "git update-ref -d reviewer-harness ref"),
    )
    .await;
    Ok(())
}

pub async fn fetch_base_branch(
    repo_path: &Path,
    base_ref: &str,
    progress: Arc<ProgressReporter>,
) -> Result<()> {
    let args = vec![
        "fetch".to_string(),
        "origin".to_string(),
        base_ref.to_string(),
    ];
    run_command_reported(
        "git",
        &prefix(repo_path, args),
        repo_path,
        CommandProgress::new(progress, format!("git fetch origin {base_ref}")),
    )
    .await
    .with_context(|| format!("failed fetching base branch {base_ref}"))?;
    Ok(())
}

pub async fn diff_for_file(
    worktree_path: &Path,
    base_ref: &str,
    file: &str,
    progress: Arc<ProgressReporter>,
) -> Result<String> {
    let args = vec![
        "-C".to_string(),
        worktree_path.display().to_string(),
        "diff".to_string(),
        "--unified=40".to_string(),
        format!("origin/{base_ref}...HEAD"),
        "--".to_string(),
        file.to_string(),
    ];
    Ok(run_command_reported(
        "git",
        &args,
        worktree_path,
        CommandProgress::new(progress, format!("git diff for {file}")),
    )
    .await?
    .stdout)
}

fn prefix(repo_path: &Path, args: Vec<String>) -> Vec<String> {
    let mut prefixed = vec!["-C".to_string(), repo_path.display().to_string()];
    prefixed.extend(args);
    prefixed
}
