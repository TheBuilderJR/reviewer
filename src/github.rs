use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::shell::run_command;
use crate::types::{ChangedFile, HistoricalPr, PrCommit, PullRequestDetails};

const GH_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Deserialize)]
struct GhRepoView {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Debug, Deserialize)]
struct GhPrView {
    number: u64,
    title: String,
    url: String,
    body: Option<String>,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    files: Vec<GhFile>,
    commits: Vec<GhCommit>,
}

#[derive(Debug, Deserialize)]
struct GhFile {
    path: String,
    additions: Option<u64>,
    deletions: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GhCommit {
    oid: String,
    #[serde(rename = "messageHeadline")]
    message_headline: String,
}

#[derive(Debug, Deserialize)]
struct GhPrListItem {
    number: u64,
    title: String,
    url: String,
    body: Option<String>,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
}

pub async fn resolve_repo_name(repo_path: &Path) -> Result<String> {
    let args = vec![
        "repo".to_string(),
        "view".to_string(),
        "--json".to_string(),
        "nameWithOwner".to_string(),
    ];
    let output = run_command("gh", &args, repo_path, GH_TIMEOUT_SECS)
        .await
        .context("failed to resolve GitHub repo via gh")?;
    let value: GhRepoView =
        serde_json::from_str(&output.stdout).context("failed to parse gh repo view output")?;
    Ok(value.name_with_owner)
}

pub async fn fetch_pr_details(
    repo_path: &Path,
    repo: &str,
    pr_number: u64,
) -> Result<PullRequestDetails> {
    let args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr_number.to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--json".to_string(),
        "number,title,url,body,baseRefName,headRefName,headRefOid,files,commits".to_string(),
    ];

    let output = run_command("gh", &args, repo_path, GH_TIMEOUT_SECS)
        .await
        .with_context(|| format!("failed to fetch PR #{pr_number}"))?;

    let pr: GhPrView =
        serde_json::from_str(&output.stdout).context("failed to parse gh pr view output")?;

    Ok(PullRequestDetails {
        number: pr.number,
        title: pr.title,
        url: pr.url,
        body: pr.body.unwrap_or_default(),
        base_ref_name: pr.base_ref_name,
        head_ref_name: pr.head_ref_name,
        head_ref_oid: pr.head_ref_oid,
        files: pr
            .files
            .into_iter()
            .map(|file| ChangedFile {
                path: file.path,
                additions: file.additions.unwrap_or(0),
                deletions: file.deletions.unwrap_or(0),
            })
            .collect(),
        commits: pr
            .commits
            .into_iter()
            .map(|commit| PrCommit {
                oid: commit.oid,
                message_headline: commit.message_headline,
            })
            .collect(),
    })
}

pub async fn find_recent_prs_for_file(
    repo_path: &Path,
    repo: &str,
    current_pr: u64,
    file: &str,
    scan_limit: usize,
    max_results: usize,
) -> Result<Vec<HistoricalPr>> {
    if max_results == 0 {
        return Ok(Vec::new());
    }

    let args = vec![
        "pr".to_string(),
        "list".to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--state".to_string(),
        "merged".to_string(),
        "--limit".to_string(),
        scan_limit.to_string(),
        "--search".to_string(),
        "sort:updated-desc".to_string(),
        "--json".to_string(),
        "number,title,url,body,mergedAt".to_string(),
    ];

    let output = run_command("gh", &args, repo_path, GH_TIMEOUT_SECS)
        .await
        .context("failed listing merged PRs")?;

    let candidates: Vec<GhPrListItem> =
        serde_json::from_str(&output.stdout).context("failed to parse gh pr list output")?;

    let mut matches = Vec::new();
    for candidate in candidates {
        if candidate.number == current_pr {
            continue;
        }

        let detail_args = vec![
            "pr".to_string(),
            "view".to_string(),
            candidate.number.to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--json".to_string(),
            "files".to_string(),
        ];

        let detail_output = match run_command("gh", &detail_args, repo_path, GH_TIMEOUT_SECS).await
        {
            Ok(value) => value,
            Err(_) => continue,
        };

        let detail: serde_json::Value = match serde_json::from_str(&detail_output.stdout) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let touches_file = detail["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["path"].as_str())
            .any(|path| path == file);

        if !touches_file {
            continue;
        }

        let diff_excerpt = fetch_pr_file_diff(repo_path, repo, candidate.number, file)
            .await
            .unwrap_or_default();

        matches.push(HistoricalPr {
            number: candidate.number,
            title: candidate.title,
            url: candidate.url,
            merged_at: candidate.merged_at.unwrap_or_default(),
            body_excerpt: excerpt(&candidate.body.unwrap_or_default(), 4_000),
            diff_excerpt: excerpt(&diff_excerpt, 10_000),
        });

        if matches.len() >= max_results {
            break;
        }
    }

    Ok(matches)
}

async fn fetch_pr_file_diff(
    repo_path: &Path,
    repo: &str,
    pr_number: u64,
    file: &str,
) -> Result<String> {
    let args = vec![
        "pr".to_string(),
        "diff".to_string(),
        pr_number.to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--patch".to_string(),
    ];

    let output = run_command("gh", &args, repo_path, GH_TIMEOUT_SECS)
        .await
        .with_context(|| format!("failed to fetch diff for PR #{pr_number}"))?;

    Ok(filter_file_diff(&output.stdout, file))
}

fn filter_file_diff(diff_text: &str, file: &str) -> String {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_file = None::<String>;

    for line in diff_text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some(path) = current_file.take() {
                if path == file {
                    chunks.push(current.join("\n"));
                }
            }
            current.clear();
            let path = rest
                .split(" b/")
                .nth(1)
                .map(ToString::to_string)
                .unwrap_or_default();
            current_file = Some(path);
        }
        current.push(line.to_string());
    }

    if let Some(path) = current_file {
        if path == file {
            chunks.push(current.join("\n"));
        }
    }

    chunks.join("\n\n")
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

#[cfg(test)]
mod tests {
    use super::filter_file_diff;

    #[test]
    fn filters_single_file_from_patch() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1 +1 @@
-old
+new
diff --git a/src/b.rs b/src/b.rs
index 333..444 100644
--- a/src/b.rs
+++ b/src/b.rs
@@ -1 +1 @@
-left
+right
";

        let filtered = filter_file_diff(diff, "src/b.rs");
        assert!(filtered.contains("src/b.rs"));
        assert!(!filtered.contains("src/a.rs"));
    }
}
