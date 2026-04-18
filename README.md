# reviewer

Rust CLI for PR review orchestration using either `codex` or `claude` as the review engine.

## What it does

For a GitHub PR, the harness:

1. Fetches the PR and creates a detached git worktree.
2. Collects changed files plus recent commits and merged PRs touching each file.
3. Shells out to the selected provider for:
   - one current-state reviewer per changed file
   - one historical-context reviewer per recent commit
   - one historical-context reviewer per recent PR
   - one file-level aggregation pass
   - one final PR-level aggregation pass
4. Produces a ranked review report in Markdown and optional JSON.
5. Writes every model prompt and raw response to a per-run directory under `/tmp/run_<uuid>`.
6. If `~/.reviewer.md` exists, prepends it to every model prompt as shared reviewer guidance.
7. Streams live progress to stderr for major phases and per-agent start/finish status.
8. Adds a final `checks` phase that plans at least 5 sanity checks, then executes them sequentially in the PR worktree.

The harness assumes the selected CLI is already authenticated.

## Requirements

- `git`
- `gh`
- `codex` or `claude`
- access to the target GitHub repo and PR

## Usage

```bash
cargo run -- \
  --provider codex \
  --repo-path /path/to/target/repo \
  --pr 123 \
  --output-markdown /tmp/pr-123-review.md \
  --output-json /tmp/pr-123-review.json
```

Release binaries are published from Git tags. For CentOS or other Linux hosts, use the static `x86_64-unknown-linux-musl` artifact from the GitHub release page.

Claude works the same way:

```bash
cargo run -- \
  --provider claude \
  --repo-path /path/to/target/repo \
  --pr 123
```

You can also pass through provider-specific flags when the local CLI needs them:

```bash
cargo run -- \
  --provider claude \
  --extra-args "--dangerously-enable-internet-mode --dangerously-skip-permissions" \
  --repo-path /path/to/target/repo \
  --pr 123
```

Optional controls:

- `--repo owner/name` to skip repo autodetection through `gh repo view`
- `--model <name>` to pass a provider-specific model name through to the CLI
- `--extra-args "<shell-style flags>"` to pass provider-specific flags straight through to `codex` or `claude`
- `--max-commits-per-file <n>`
- `--max-prs-per-file <n>`
- `--pr-scan-limit <n>`
- `--parallelism <n>`
- `--agent-timeout-secs <n>`
- `--check-timeout-secs <n>` to control the timeout for each sequential shell check
- `--keep-worktree`

## Releases

- Pushes and pull requests run `cargo test`.
- Tags matching `v*` build and publish a release artifact.
- `v0.0.1` and later publish `reviewer-x86_64-unknown-linux-musl.tar.gz`, which is intended to run cleanly on CentOS without depending on the host glibc version.

## Notes

- The current implementation gathers prior PR context by scanning recent merged PRs and filtering to file matches. That is intentionally simple and may be slow on large repos.
- Progress is printed to stderr so it stays visible while the final Markdown report is still clean on stdout.
- The current build step is only advisory in progress output; actual build/test execution happens through the final `checks` phase.
- Provider subprocess failures bubble up directly. If `claude` or `codex` is logged in but not actually usable for the org/account, the run will fail with the CLI error text.
- Each provider invocation writes paired files such as `1776545033_initial-prompt_review-src-main-rs-<hash>_1.txt` and `1776545033_response_review-src-main-rs-<hash>_1.txt`. The CLI prints the run directory path at the end, including on failure.
- Sequential checks also write `check-command` and `check-result` artifacts under the same run directory.
- Those artifact files now include the exact provider argv as JSON, so extra flags like `--dangerously-skip-permissions` are visible after the fact.
- `~/.reviewer.md` is optional. If present, its contents are injected into every provider prompt, which is the right place for stable instructions like build/test commands, repo-specific invariants, or review priorities.
