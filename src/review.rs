use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use futures::future::join_all;
use tokio::sync::Semaphore;

use crate::git::{
    Worktree, cleanup_worktree, create_pr_worktree, diff_for_file, fetch_base_branch,
    fetch_pr_head_ref, recent_commits_for_file,
};
use crate::github::{fetch_pr_details, find_recent_prs_for_file};
use crate::progress::ProgressReporter;
use crate::provider::{Provider, invoke_typed};
use crate::runlog::RunLogger;
use crate::shell::capture_command_with_input;
use crate::types::{
    CheckExecution, CheckPlanDraft, ContextReviewDraft, FileAggregate, FileReviewDraft,
    FileReviewJob, FinalReviewDraft, FinalReviewReport, HistoricalCommit, HistoricalPr,
    PullRequestDetails, sort_findings,
};

#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub pr_number: u64,
    pub repo_name: String,
    pub repo_path: PathBuf,
    pub user_request: Option<String>,
    pub max_commits_per_file: usize,
    pub max_prs_per_file: usize,
    pub pr_scan_limit: usize,
    pub parallelism: usize,
    pub check_timeout_secs: u64,
    pub keep_worktree: bool,
}

pub async fn run_review(
    options: ReviewOptions,
    provider: Arc<dyn Provider>,
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
) -> Result<FinalReviewReport> {
    let pr = {
        let step = progress.begin_step(
            "phase",
            format!("loading PR #{} metadata", options.pr_number),
        );
        match fetch_pr_details(&options.repo_path, &options.repo_name, options.pr_number).await {
            Ok(pr) => {
                step.done(format!("{} changed files", pr.files.len()));
                pr
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    };

    {
        let step = progress.begin_step(
            "phase",
            format!("fetching base branch {}", pr.base_ref_name),
        );
        match fetch_base_branch(&options.repo_path, &pr.base_ref_name).await {
            Ok(()) => step.done("base branch ready"),
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    }

    let review_ref = {
        let step = progress.begin_step("phase", format!("checking out PR #{}", options.pr_number));
        match fetch_pr_head_ref(&options.repo_path, options.pr_number).await {
            Ok(review_ref) => {
                step.done(review_ref.clone());
                review_ref
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    };

    let worktree = {
        let step = progress.begin_step("phase", "creating worktree".to_string());
        match create_pr_worktree(&options.repo_path, options.pr_number, &review_ref).await {
            Ok(worktree) => {
                step.done(worktree.path.display().to_string());
                worktree
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    };

    progress.info(
        "phase",
        "building repo step is not configured; skipping local build execution",
    );

    let mut run_result = async {
        let jobs = collect_jobs(&options, &pr, &worktree, progress.clone()).await?;
        let aggregates = review_files(
            &options,
            &pr,
            &worktree,
            jobs,
            provider.clone(),
            progress.clone(),
        )
        .await?;
        let mut report = aggregate_final_report(
            &options,
            &pr,
            &worktree,
            aggregates,
            provider.clone(),
            progress.clone(),
        )
        .await?;
        let check_plan = plan_checks(
            &options,
            &pr,
            &worktree,
            &report,
            provider.clone(),
            progress.clone(),
        )
        .await?;
        let checks = run_checks(
            &options,
            &worktree,
            &check_plan,
            run_logger.clone(),
            progress.clone(),
        )
        .await?;
        report.checks_summary = summarize_checks(&check_plan.summary, &checks);
        report.checks = checks;
        Ok(report)
    }
    .await;

    if !options.keep_worktree {
        let step = progress.begin_step("phase", "cleaning up worktree".to_string());
        match cleanup_worktree(&options.repo_path, &worktree).await {
            Ok(()) => {
                step.done("temporary worktree removed");
                if let Ok(report) = run_result.as_mut() {
                    report.worktree_path =
                        format!("{} (removed after run)", worktree.path.display());
                    report
                        .notes
                        .push("Temporary worktree was cleaned up after completion.".to_string());
                }
            }
            Err(error) => {
                step.fail(error.to_string());
                if let Ok(report) = run_result.as_mut() {
                    report.notes.push(format!(
                        "Failed to clean up temporary worktree {}: {error}",
                        worktree.path.display()
                    ));
                }
            }
        }
    }

    if let Ok(report) = run_result.as_mut() {
        report.run_artifact_dir = run_logger.root().display().to_string();
    }

    run_result
}

async fn collect_jobs(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    progress: Arc<ProgressReporter>,
) -> Result<Vec<FileReviewJob>> {
    let total_files = pr.files.len();
    let step = progress.begin_step(
        "phase",
        format!(
            "aggregating historical commits/PRs that touch {} files",
            total_files
        ),
    );
    let mut jobs = Vec::new();

    for (index, file) in pr.files.iter().enumerate() {
        progress.info(
            "history",
            format!("[{}/{}] scanning {}", index + 1, total_files, file.path),
        );
        let diff_excerpt = diff_for_file(&worktree.path, &pr.base_ref_name, &file.path)
            .await
            .map(|value| excerpt(&value, 16_000))
            .unwrap_or_default();

        let recent_commits = recent_commits_for_file(
            &worktree.path,
            &pr.base_ref_name,
            &file.path,
            options.max_commits_per_file,
        )
        .await
        .with_context(|| format!("failed to gather commit history for {}", file.path))?;

        let recent_prs = find_recent_prs_for_file(
            &options.repo_path,
            &options.repo_name,
            pr.number,
            &file.path,
            options.pr_scan_limit,
            options.max_prs_per_file,
        )
        .await
        .with_context(|| format!("failed to gather prior PRs for {}", file.path))?;

        jobs.push(FileReviewJob {
            file: file.path.clone(),
            additions: file.additions,
            deletions: file.deletions,
            diff_excerpt,
            recent_commits,
            recent_prs,
        });

        if let Some(job) = jobs.last() {
            progress.info(
                "history",
                format!(
                    "[{}/{}] {} -> {} commits, {} prior PRs",
                    index + 1,
                    total_files,
                    job.file,
                    job.recent_commits.len(),
                    job.recent_prs.len()
                ),
            );
        }
    }

    step.done(format!("{} file review jobs ready", jobs.len()));
    Ok(jobs)
}

async fn review_files(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    jobs: Vec<FileReviewJob>,
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<Vec<FileAggregate>> {
    let total_invocations = jobs.iter().map(planned_invocations_for_job).sum::<usize>() + 2;
    progress.set_agent_total(total_invocations);
    let queue_step = progress.begin_step(
        "phase",
        format!("spawning subagents for {} changed files", jobs.len()),
    );

    let semaphore = Arc::new(Semaphore::new(options.parallelism));
    let mut tasks = Vec::new();
    let total_files = jobs.len();

    for (index, job) in jobs.into_iter().enumerate() {
        progress.info(
            "agents",
            format!(
                "[{}/{}] queued {} provider invocations for {}",
                index + 1,
                total_files,
                planned_invocations_for_job(&job),
                job.file
            ),
        );
        let semaphore = semaphore.clone();
        let provider = provider.clone();
        let progress = progress.clone();
        let worktree_path = worktree.path.clone();
        let pr = pr.clone();
        let user_request = options.user_request.clone();
        tasks.push(tokio::spawn(async move {
            review_single_file(
                &semaphore,
                provider,
                &worktree_path,
                &pr,
                job,
                user_request.as_deref(),
                progress,
            )
            .await
        }));
    }

    queue_step.done(format!(
        "{} provider invocations queued with parallelism={}",
        total_invocations, options.parallelism
    ));

    let results = join_all(tasks).await;
    let mut aggregates = Vec::new();
    for result in results {
        aggregates.push(result.context("file review task panicked")??);
    }
    Ok(aggregates)
}

async fn review_single_file(
    semaphore: &Arc<Semaphore>,
    provider: Arc<dyn Provider>,
    worktree_path: &PathBuf,
    pr: &PullRequestDetails,
    job: FileReviewJob,
    user_request: Option<&str>,
    progress: Arc<ProgressReporter>,
) -> Result<FileAggregate> {
    progress.info(
        "file",
        format!(
            "{} -> current review + {} commit contexts + {} PR contexts + file aggregate",
            job.file,
            job.recent_commits.len(),
            job.recent_prs.len()
        ),
    );
    let base_prompt = build_current_file_prompt(pr, &job, worktree_path, user_request);
    let base_review: FileReviewDraft = invoke_with_semaphore(
        semaphore,
        provider.as_ref(),
        worktree_path,
        &format!("review {}", job.file),
        &base_prompt,
    )
    .await?;

    let mut context_tasks = Vec::new();

    for commit in job.recent_commits.clone() {
        let prompt = build_commit_context_prompt(pr, &job, &commit, worktree_path, user_request);
        let label = format!("context commit {} {}", short_sha(&commit.sha), job.file);
        let semaphore = semaphore.clone();
        let provider = provider.clone();
        let cwd = worktree_path.clone();
        context_tasks.push(tokio::spawn(async move {
            invoke_with_semaphore::<ContextReviewDraft>(
                &semaphore,
                provider.as_ref(),
                &cwd,
                &label,
                &prompt,
            )
            .await
        }));
    }

    for prior_pr in job.recent_prs.clone() {
        let prompt = build_pr_context_prompt(pr, &job, &prior_pr, worktree_path, user_request);
        let label = format!("context pr {} {}", prior_pr.number, job.file);
        let semaphore = semaphore.clone();
        let provider = provider.clone();
        let cwd = worktree_path.clone();
        context_tasks.push(tokio::spawn(async move {
            invoke_with_semaphore::<ContextReviewDraft>(
                &semaphore,
                provider.as_ref(),
                &cwd,
                &label,
                &prompt,
            )
            .await
        }));
    }

    let mut context_reviews = Vec::new();
    for task in join_all(context_tasks).await {
        context_reviews.push(task.context("context review task panicked")??);
    }

    let aggregate_prompt =
        build_file_aggregate_prompt(pr, &job, &base_review, &context_reviews, user_request);
    invoke_with_semaphore::<FileAggregate>(
        semaphore,
        provider.as_ref(),
        worktree_path,
        &format!("aggregate file {}", job.file),
        &aggregate_prompt,
    )
    .await
}

async fn aggregate_final_report(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    mut aggregates: Vec<FileAggregate>,
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<FinalReviewReport> {
    let step = progress.begin_step("phase", "aggregating and ranking reviews".to_string());
    for aggregate in &mut aggregates {
        sort_findings(&mut aggregate.findings);
    }
    aggregates.sort_by(|left, right| left.file.cmp(&right.file));

    let prompt = build_final_report_prompt(options, pr, &aggregates, worktree);
    let draft: FinalReviewDraft = invoke_typed(
        provider.as_ref(),
        &worktree.path,
        &format!("aggregate pr {}", pr.number),
        &prompt,
    )
    .await?;

    let mut ranked_findings = draft.ranked_findings;
    sort_findings(&mut ranked_findings);
    let file_count = aggregates.len();
    let findings_count = ranked_findings.len();
    let report = FinalReviewReport {
        repo: options.repo_name.clone(),
        pr_number: pr.number,
        pr_title: pr.title.clone(),
        provider: provider.kind().as_str().to_string(),
        worktree_path: worktree.path.display().to_string(),
        run_artifact_dir: String::new(),
        executive_summary: draft.executive_summary,
        checks_summary: String::new(),
        ranked_findings,
        per_file: aggregates,
        checks: Vec::new(),
        notes: draft.notes,
    };
    step.done(format!(
        "{} ranked findings across {} files",
        findings_count, file_count
    ));
    Ok(report)
}

async fn plan_checks(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    report: &FinalReviewReport,
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<CheckPlanDraft> {
    let step = progress.begin_step("phase", "planning checks".to_string());
    let prompt = build_check_plan_prompt(options, pr, report, worktree);
    let plan_result: Result<CheckPlanDraft> = async {
        let plan: CheckPlanDraft = invoke_typed(
            provider.as_ref(),
            &worktree.path,
            &format!("plan checks {}", pr.number),
            &prompt,
        )
        .await?;
        validate_check_plan(&plan)?;
        Ok(plan)
    }
    .await;

    match plan_result {
        Ok(plan) => {
            step.done(format!("{} checks planned", plan.checks.len()));
            Ok(plan)
        }
        Err(error) => {
            step.fail(error.to_string());
            Err(error)
        }
    }
}

async fn run_checks(
    options: &ReviewOptions,
    worktree: &Worktree,
    plan: &CheckPlanDraft,
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
) -> Result<Vec<CheckExecution>> {
    let step = progress.begin_step(
        "phase",
        format!("running {} checks sequentially", plan.checks.len()),
    );
    let total = plan.checks.len();
    let mut executions = Vec::with_capacity(total);

    for (index, check) in plan.checks.iter().enumerate() {
        let label = format!("[{}/{}] {}", index + 1, total, check.name);
        let check_step = progress.begin_step("check", label);
        let invocation = run_logger.begin(&format!("check {} {}", index + 1, check.name));
        let command_body = format!(
            "index: {}\nname: {}\ncommand: {}\nrationale: {}\nexpected_signal: {}\nrelated_findings: {}\ncwd: {}\n",
            index + 1,
            check.name,
            check.command,
            check.rationale,
            check.expected_signal,
            check.related_findings.join(" | "),
            worktree.path.display()
        );
        run_logger
            .write_text(&invocation, "check-command", &command_body)
            .await?;

        let started_at = Instant::now();
        let args = vec!["-lc".to_string(), check.command.clone()];
        let output = capture_command_with_input(
            "bash",
            &args,
            &worktree.path,
            None,
            options.check_timeout_secs,
        )
        .await;
        let duration_secs = started_at.elapsed().as_secs_f32();

        let execution = match output {
            Ok(output) => {
                let status = if output.success { "passed" } else { "failed" };
                let result_body = format!(
                    "index: {}\nname: {}\ncommand: {}\nstatus: {}\nexit_code: {:?}\nduration_secs: {:.2}\n\nstdout:\n{}\n\nstderr:\n{}\n",
                    index + 1,
                    check.name,
                    check.command,
                    status,
                    output.status_code,
                    duration_secs,
                    excerpt(&output.stdout, 12000),
                    excerpt(&output.stderr, 12000)
                );
                run_logger
                    .write_text(&invocation, "check-result", &result_body)
                    .await?;

                if output.success {
                    check_step.done(format!("passed in {:.1}s", duration_secs));
                } else {
                    check_step.fail(format!(
                        "failed with exit code {:?} in {:.1}s",
                        output.status_code, duration_secs
                    ));
                }

                CheckExecution {
                    index: index + 1,
                    name: check.name.clone(),
                    command: check.command.clone(),
                    rationale: check.rationale.clone(),
                    expected_signal: check.expected_signal.clone(),
                    related_findings: check.related_findings.clone(),
                    status: status.to_string(),
                    exit_code: output.status_code,
                    duration_secs,
                    stdout_excerpt: excerpt(&output.stdout, 4000),
                    stderr_excerpt: excerpt(&output.stderr, 4000),
                }
            }
            Err(error) => {
                let result_body = format!(
                    "index: {}\nname: {}\ncommand: {}\nstatus: error\nduration_secs: {:.2}\n\nerror:\n{}\n",
                    index + 1,
                    check.name,
                    check.command,
                    duration_secs,
                    error
                );
                run_logger
                    .write_text(&invocation, "check-result", &result_body)
                    .await?;
                check_step.fail(format!("error after {:.1}s -> {}", duration_secs, error));

                CheckExecution {
                    index: index + 1,
                    name: check.name.clone(),
                    command: check.command.clone(),
                    rationale: check.rationale.clone(),
                    expected_signal: check.expected_signal.clone(),
                    related_findings: check.related_findings.clone(),
                    status: "error".to_string(),
                    exit_code: None,
                    duration_secs,
                    stdout_excerpt: String::new(),
                    stderr_excerpt: excerpt(&error.to_string(), 4000),
                }
            }
        };

        executions.push(execution);
    }

    let passed = executions
        .iter()
        .filter(|check| check.status == "passed")
        .count();
    let failed = executions
        .iter()
        .filter(|check| check.status == "failed")
        .count();
    let errored = executions
        .iter()
        .filter(|check| check.status == "error")
        .count();
    step.done(format!(
        "{} passed, {} failed, {} errored",
        passed, failed, errored
    ));
    Ok(executions)
}

fn validate_check_plan(plan: &CheckPlanDraft) -> Result<()> {
    anyhow::ensure!(
        plan.checks.len() >= 5,
        "check plan returned {} checks; expected at least 5",
        plan.checks.len()
    );

    for (index, check) in plan.checks.iter().enumerate() {
        anyhow::ensure!(
            !check.name.trim().is_empty(),
            "check {} is missing a name",
            index + 1
        );
        anyhow::ensure!(
            !check.command.trim().is_empty(),
            "check {} is missing a command",
            index + 1
        );
    }

    Ok(())
}

async fn invoke_with_semaphore<T>(
    semaphore: &Arc<Semaphore>,
    provider: &dyn Provider,
    cwd: &PathBuf,
    label: &str,
    prompt: &str,
) -> Result<T>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema,
{
    let _permit = semaphore.acquire().await?;
    invoke_typed(provider, cwd, label, prompt).await
}

fn build_current_file_prompt(
    pr: &PullRequestDetails,
    job: &FileReviewJob,
    worktree_path: &PathBuf,
    user_request: Option<&str>,
) -> String {
    format!(
        "You are reviewing PR #{pr_number} ({pr_title}) in repo {repo_url}.\n\
         Focus only on file `{file}`.\n\n\
         Worktree path: {worktree}\n\
         Base branch: {base}\n\
         File change stats: +{additions} / -{deletions}\n\n\
         User request:\n{user_request}\n\n\
         Current diff excerpt:\n```diff\n{diff}\n```\n\n\
         Instructions:\n\
         - Inspect the worktree and any nearby code you need.\n\
         - Report only substantive correctness, regression, reliability, or maintainability issues.\n\
         - Ignore style-only nits and duplicate observations.\n\
         - Use priority 0 for release-blocking issues, 1 for major bugs, 2 for moderate issues, 3 for minor-but-actionable issues.\n\
         - Keep source_refs specific to the file, symbols, or commands you inspected.\n\
         - Return at most 5 findings.\n\
         - If there is no meaningful issue, return an empty findings array.",
        pr_number = pr.number,
        pr_title = pr.title,
        repo_url = pr.url,
        file = job.file,
        worktree = worktree_path.display(),
        base = pr.base_ref_name,
        additions = job.additions,
        deletions = job.deletions,
        user_request = render_user_request(user_request),
        diff = job.diff_excerpt
    )
}

fn build_commit_context_prompt(
    pr: &PullRequestDetails,
    job: &FileReviewJob,
    commit: &HistoricalCommit,
    worktree_path: &PathBuf,
    user_request: Option<&str>,
) -> String {
    format!(
        "You are a historical context sub-reviewer for PR #{pr_number} ({pr_title}).\n\
         File under review: `{file}`\n\
         Worktree path: {worktree}\n\n\
         User request:\n{user_request}\n\n\
         Current PR diff excerpt:\n```diff\n{current_diff}\n```\n\n\
         Historical commit touching the same file:\n\
         SHA: {sha}\n\
         Title: {title}\n\
         Timestamp: {unix_time}\n\
         Patch excerpt:\n```diff\n{patch}\n```\n\n\
         Task:\n\
         - Compare the historical change to the current PR.\n\
         - Extract lessons, regressions, or edge cases that are plausibly relevant now.\n\
         - Candidate findings must be grounded in the current PR, not just the old commit.\n\
         - Return zero findings if the historical commit is not useful context.",
        pr_number = pr.number,
        pr_title = pr.title,
        file = job.file,
        worktree = worktree_path.display(),
        user_request = render_user_request(user_request),
        current_diff = job.diff_excerpt,
        sha = commit.sha,
        title = commit.title,
        unix_time = commit.unix_time,
        patch = commit.patch_excerpt
    )
}

fn build_pr_context_prompt(
    pr: &PullRequestDetails,
    job: &FileReviewJob,
    prior_pr: &HistoricalPr,
    worktree_path: &PathBuf,
    user_request: Option<&str>,
) -> String {
    format!(
        "You are a historical context sub-reviewer for PR #{pr_number} ({pr_title}).\n\
         File under review: `{file}`\n\
         Worktree path: {worktree}\n\n\
         User request:\n{user_request}\n\n\
         Current PR diff excerpt:\n```diff\n{current_diff}\n```\n\n\
         Prior merged PR touching the same file:\n\
         Number: #{prior_number}\n\
         Title: {prior_title}\n\
         URL: {prior_url}\n\
         Merged at: {merged_at}\n\
         Body excerpt:\n{body_excerpt}\n\n\
         Diff excerpt:\n```diff\n{prior_diff}\n```\n\n\
         Task:\n\
         - Use this prior PR as historical context only.\n\
         - Surface candidate findings only if the prior PR exposes a likely bug, regression pattern, or overlooked invariant in the current PR.\n\
         - Return zero findings when the historical PR is not relevant.",
        pr_number = pr.number,
        pr_title = pr.title,
        file = job.file,
        worktree = worktree_path.display(),
        user_request = render_user_request(user_request),
        current_diff = job.diff_excerpt,
        prior_number = prior_pr.number,
        prior_title = prior_pr.title,
        prior_url = prior_pr.url,
        merged_at = prior_pr.merged_at,
        body_excerpt = prior_pr.body_excerpt,
        prior_diff = prior_pr.diff_excerpt
    )
}

fn build_file_aggregate_prompt(
    pr: &PullRequestDetails,
    job: &FileReviewJob,
    base_review: &FileReviewDraft,
    context_reviews: &[ContextReviewDraft],
    user_request: Option<&str>,
) -> String {
    format!(
        "You are the file-level aggregation reviewer for PR #{pr_number} ({pr_title}).\n\
         Consolidate feedback for file `{file}`.\n\n\
         User request:\n{user_request}\n\n\
         Current file review:\n{base_review}\n\n\
         Historical context reviews:\n{context_reviews}\n\n\
         Instructions:\n\
         - Merge duplicates.\n\
         - Reject weak or speculative findings.\n\
         - Preserve only findings that would matter in a serious code review.\n\
         - Return at most 5 ranked findings for this file.\n\
         - Put rejected or de-prioritized ideas in discarded_notes.",
        pr_number = pr.number,
        pr_title = pr.title,
        file = job.file,
        user_request = render_user_request(user_request),
        base_review = serde_json::to_string_pretty(base_review).unwrap_or_default(),
        context_reviews = serde_json::to_string_pretty(context_reviews).unwrap_or_default()
    )
}

fn build_final_report_prompt(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    aggregates: &[FileAggregate],
    worktree: &Worktree,
) -> String {
    format!(
        "You are the final PR review aggregator.\n\
         Repo: {repo}\n\
         PR: #{pr_number} {pr_title}\n\
         URL: {pr_url}\n\
         Worktree: {worktree}\n\
         Provider: {provider}\n\n\
         User request:\n{user_request}\n\n\
         File-level aggregated reviews:\n{aggregates}\n\n\
         Instructions:\n\
         - Rank findings across the whole PR.\n\
         - Deduplicate cross-file variants of the same issue.\n\
         - Keep only high-signal findings that a reviewer should actually raise.\n\
         - Return an executive summary, at most 12 ranked findings, and concise notes about coverage gaps if any.\n\
         - If there are no strong findings, say so plainly.",
        repo = options.repo_name,
        pr_number = pr.number,
        pr_title = pr.title,
        pr_url = pr.url,
        worktree = worktree.path.display(),
        provider = "delegated-subprocess",
        user_request = render_user_request(options.user_request.as_deref()),
        aggregates = serde_json::to_string_pretty(aggregates).unwrap_or_default()
    )
}

fn build_check_plan_prompt(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    report: &FinalReviewReport,
    worktree: &Worktree,
) -> String {
    let changed_files = pr
        .files
        .iter()
        .map(|file| {
            format!(
                "- {} (+{} / -{})",
                file.path, file.additions, file.deletions
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let changed_test_files = pr
        .files
        .iter()
        .filter(|file| looks_like_test_file(&file.path))
        .map(|file| format!("- {}", file.path))
        .collect::<Vec<_>>();
    let changed_test_files = if changed_test_files.is_empty() {
        "- none".to_string()
    } else {
        changed_test_files.join("\n")
    };

    format!(
        "You are planning a post-review checks phase for PR #{pr_number} ({pr_title}) in repo {repo}.\n\
         Worktree path: {worktree}\n\
         Base branch: {base}\n\n\
         User request:\n{user_request}\n\n\
         Changed files:\n{changed_files}\n\n\
         Changed test-like files:\n{changed_test_files}\n\n\
         Ranked findings:\n{findings}\n\n\
         Per-file summaries:\n{per_file}\n\n\
         Requirements:\n\
         - Return at least 5 checks.\n\
         - Check #1 must run the added or modified tests that most directly encode the changed behavior, because those tests should have failed before the patch. If no test files changed, choose the narrowest existing repro command for the changed behavior and make that check #1.\n\
         - Commands must be non-interactive shell commands that can be run from the worktree with `bash -lc`.\n\
         - Use repo-specific guidance from the global reviewer instructions when available, especially for build/test commands.\n\
         - Prefer targeted checks first, then broader build/test or lint validation where useful.\n\
         - Avoid duplicate commands.\n\
         - Every check must include a clear rationale and the expected signal to inspect.\n\
         - Related findings should cite finding titles or file paths, not vague references.",
        pr_number = pr.number,
        pr_title = pr.title,
        repo = options.repo_name,
        worktree = worktree.path.display(),
        base = pr.base_ref_name,
        user_request = render_user_request(options.user_request.as_deref()),
        changed_files = changed_files,
        changed_test_files = changed_test_files,
        findings = serde_json::to_string_pretty(&report.ranked_findings).unwrap_or_default(),
        per_file = serde_json::to_string_pretty(&report.per_file).unwrap_or_default()
    )
}

pub fn render_markdown(report: &FinalReviewReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# PR Review: #{} {}\n\n",
        report.pr_number, report.pr_title
    ));
    out.push_str(&format!(
        "- Repo: `{}`\n- Provider: `{}`\n- Worktree: `{}`\n- Run artifacts: `{}`\n\n",
        report.repo, report.provider, report.worktree_path, report.run_artifact_dir
    ));
    out.push_str("## Executive Summary\n\n");
    out.push_str(report.executive_summary.trim());
    out.push_str("\n\n");

    out.push_str("## Ranked Findings\n\n");
    if report.ranked_findings.is_empty() {
        out.push_str("No high-confidence findings.\n\n");
    } else {
        for (index, finding) in report.ranked_findings.iter().enumerate() {
            out.push_str(&format!(
                "{}. [P{}] `{}`: {}\n",
                index + 1,
                finding.priority,
                finding.file,
                finding.title
            ));
            out.push_str(&format!("   Confidence: {:.2}\n", finding.confidence));
            out.push_str(&format!(
                "   Why it matters: {}\n",
                finding.rationale.trim()
            ));
            out.push_str(&format!(
                "   Suggested fix: {}\n",
                finding.suggested_fix.trim()
            ));
            if !finding.source_refs.is_empty() {
                out.push_str(&format!(
                    "   References: {}\n",
                    finding.source_refs.join(", ")
                ));
            }
            out.push('\n');
        }
    }

    out.push_str("## Checks\n\n");
    out.push_str(report.checks_summary.trim());
    out.push_str("\n\n");
    if report.checks.is_empty() {
        out.push_str("No checks were executed.\n\n");
    } else {
        for check in &report.checks {
            out.push_str(&format!(
                "{}. [{}] {}\n",
                check.index,
                check.status.to_ascii_uppercase(),
                check.name
            ));
            out.push_str(&format!("   Command: `{}`\n", check.command));
            out.push_str(&format!("   Rationale: {}\n", check.rationale.trim()));
            out.push_str(&format!(
                "   Expected signal: {}\n",
                check.expected_signal.trim()
            ));
            out.push_str(&format!(
                "   Exit code: {:?}, duration: {:.1}s\n",
                check.exit_code, check.duration_secs
            ));
            if !check.related_findings.is_empty() {
                out.push_str(&format!(
                    "   Related findings: {}\n",
                    check.related_findings.join(", ")
                ));
            }
            if !check.stdout_excerpt.trim().is_empty() {
                out.push_str("   Stdout excerpt:\n");
                out.push_str("```text\n");
                out.push_str(check.stdout_excerpt.trim());
                out.push_str("\n```\n");
            }
            if !check.stderr_excerpt.trim().is_empty() {
                out.push_str("   Stderr excerpt:\n");
                out.push_str("```text\n");
                out.push_str(check.stderr_excerpt.trim());
                out.push_str("\n```\n");
            }
            out.push('\n');
        }
    }

    out.push_str("## Per-File Summaries\n\n");
    for aggregate in &report.per_file {
        out.push_str(&format!("### `{}`\n\n", aggregate.file));
        out.push_str(aggregate.summary.trim());
        out.push_str("\n\n");
        if aggregate.findings.is_empty() {
            out.push_str("No retained findings.\n\n");
        } else {
            for finding in &aggregate.findings {
                out.push_str(&format!(
                    "- [P{}] {} (confidence {:.2})\n",
                    finding.priority, finding.title, finding.confidence
                ));
            }
            out.push('\n');
        }
    }

    if !report.notes.is_empty() {
        out.push_str("## Notes\n\n");
        for note in &report.notes {
            out.push_str(&format!("- {}\n", note.trim()));
        }
        out.push('\n');
    }

    out
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

fn short_sha(sha: &str) -> &str {
    let end = sha.len().min(8);
    &sha[..end]
}

fn planned_invocations_for_job(job: &FileReviewJob) -> usize {
    2 + job.recent_commits.len() + job.recent_prs.len()
}

fn summarize_checks(plan_summary: &str, checks: &[CheckExecution]) -> String {
    let passed = checks
        .iter()
        .filter(|check| check.status == "passed")
        .count();
    let failed = checks
        .iter()
        .filter(|check| check.status == "failed")
        .count();
    let errored = checks
        .iter()
        .filter(|check| check.status == "error")
        .count();

    format!(
        "{}\n\nExecuted {} checks: {} passed, {} failed, {} errored.",
        plan_summary.trim(),
        checks.len(),
        passed,
        failed,
        errored
    )
}

fn looks_like_test_file(path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    lowered.contains("/test")
        || lowered.contains("/tests/")
        || lowered.contains("__tests__")
        || lowered.contains("spec")
        || lowered.ends_with("_test.py")
        || lowered.ends_with("_test.rs")
        || lowered.ends_with(".spec.ts")
        || lowered.ends_with(".spec.js")
        || lowered.ends_with(".test.ts")
        || lowered.ends_with(".test.js")
}

fn render_user_request(user_request: Option<&str>) -> String {
    match user_request {
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => "No additional user request provided.".to_string(),
    }
}
