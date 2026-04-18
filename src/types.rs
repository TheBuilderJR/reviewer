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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReviewJob {
    pub file: String,
    pub additions: u64,
    pub deletions: u64,
    pub diff_excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFinding {
    #[serde(default)]
    pub file: String,
    #[serde(default, alias = "label", alias = "summary")]
    pub title: String,
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default, alias = "body", alias = "reason", alias = "why")]
    pub rationale: String,
    #[serde(default, alias = "fix", alias = "suggestion")]
    pub suggested_fix: String,
    #[serde(default, alias = "references")]
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InlineComment {
    #[serde(default, alias = "path")]
    pub file: String,
    #[serde(default, alias = "line", alias = "line_number")]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default, alias = "summary", alias = "label")]
    pub title: String,
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default, alias = "comment", alias = "message", alias = "rationale")]
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileReviewDraft {
    #[serde(default)]
    pub file: String,
    #[serde(default, alias = "executive_summary")]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub inline_comments: Vec<InlineComment>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FinalReviewDraft {
    #[serde(default, alias = "summary")]
    pub executive_summary: String,
    #[serde(default, alias = "findings")]
    pub summary_findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub inline_comments: Vec<InlineComment>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildExecution {
    #[serde(default = "default_build_status")]
    pub status: String,
    #[serde(default, alias = "result")]
    pub summary: String,
    #[serde(default, alias = "commands")]
    pub commands_run: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default, alias = "why")]
    pub rationale: String,
    #[serde(default, alias = "expected")]
    pub expected_signal: String,
    #[serde(default, alias = "related")]
    pub related_findings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckPlanDraft {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
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
    pub summary_findings: Vec<ReviewFinding>,
    pub inline_comments: Vec<InlineComment>,
    pub checks_summary: String,
    pub per_file: Vec<FileReviewDraft>,
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

pub fn sort_inline_comments(comments: &mut [InlineComment]) {
    comments.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.end_line.cmp(&right.end_line))
            .then_with(|| left.priority.cmp(&right.priority))
            .then_with(|| {
                right
                    .confidence
                    .partial_cmp(&left.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.title.cmp(&right.title))
    });
}

fn default_priority() -> u8 {
    2
}

fn default_confidence() -> f32 {
    0.7
}

fn default_build_status() -> String {
    "skipped".to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{BuildExecution, FileReviewDraft, InlineComment};

    #[test]
    fn deserializes_file_review_without_top_level_file() {
        let value = json!({
            "summary": "Looks fine overall.",
            "findings": [],
            "inline_comments": [],
            "notes": []
        });

        let parsed: FileReviewDraft = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.file, "");
        assert_eq!(parsed.summary, "Looks fine overall.");
    }

    #[test]
    fn deserializes_inline_comment_line_alias() {
        let value = json!({
            "title": "Use a tuple append here",
            "line": 17,
            "comment": "This should stay line-anchored."
        });

        let parsed: InlineComment = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.start_line, Some(17));
        assert_eq!(parsed.body, "This should stay line-anchored.");
    }

    #[test]
    fn deserializes_build_execution_with_defaults() {
        let value = json!({
            "summary": "Build could not run in this environment."
        });

        let parsed: BuildExecution = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.status, "skipped");
        assert_eq!(parsed.summary, "Build could not run in this environment.");
        assert!(parsed.commands_run.is_empty());
    }
}
