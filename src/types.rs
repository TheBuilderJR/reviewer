use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestDetails {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub body: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub files: Vec<ChangedFile>,
    pub commits: Vec<PrCommit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrCommit {
    pub oid: String,
    pub message_headline: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalCommit {
    pub sha: String,
    pub title: String,
    pub unix_time: i64,
    pub patch_excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalPr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub merged_at: String,
    pub body_excerpt: String,
    pub diff_excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReviewJob {
    pub file: String,
    pub additions: u64,
    pub deletions: u64,
    pub diff_excerpt: String,
    pub recent_commits: Vec<HistoricalCommit>,
    pub recent_prs: Vec<HistoricalPr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFinding {
    pub file: String,
    pub title: String,
    pub priority: u8,
    pub confidence: f32,
    pub rationale: String,
    pub suggested_fix: String,
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileReviewDraft {
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContextReviewDraft {
    pub source_id: String,
    pub source_kind: String,
    pub title: String,
    pub relevance: String,
    pub candidate_findings: Vec<ReviewFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileAggregate {
    pub file: String,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub discarded_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FinalReviewDraft {
    pub executive_summary: String,
    pub ranked_findings: Vec<ReviewFinding>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildExecution {
    pub status: String,
    pub summary: String,
    pub commands_run: Vec<String>,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckSpec {
    pub name: String,
    pub command: String,
    pub rationale: String,
    pub expected_signal: String,
    pub related_findings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckPlanDraft {
    pub summary: String,
    pub checks: Vec<CheckSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckExecution {
    pub index: usize,
    pub name: String,
    pub command: String,
    pub rationale: String,
    pub expected_signal: String,
    pub related_findings: Vec<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_secs: f32,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FinalReviewReport {
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub provider: String,
    pub worktree_path: String,
    pub run_artifact_dir: String,
    pub executive_summary: String,
    pub build: Option<BuildExecution>,
    pub checks_summary: String,
    pub ranked_findings: Vec<ReviewFinding>,
    pub per_file: Vec<FileAggregate>,
    pub checks: Vec<CheckExecution>,
    pub notes: Vec<String>,
}

pub fn sort_findings(findings: &mut [ReviewFinding]) {
    findings.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| {
                right
                    .confidence
                    .partial_cmp(&left.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.title.cmp(&right.title))
    });
}
