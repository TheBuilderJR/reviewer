# reviewer

Rust CLI for PR review orchestration using either `codex` or `claude` as the review engine.

## What it does

For a GitHub PR, the harness:

1. Fetches the PR and creates a detached git worktree.
2. Runs an explicit build/setup phase in the PR worktree, driven by the instructions in `~/.reviewer.md`.
3. Prepares one review job per changed file.
4. Shells out to the selected provider for one reviewer per changed file, with each reviewer starting from that file but allowed to inspect nearby code in other files.
5. Uses the build result plus those file reviews to plan at least 5 checks, then runs those checks sequentially in the PR worktree.
6. Writes a final review with an executive summary and inline comments like a real code review.
7. Writes every model prompt and raw response to a per-run directory under `/tmp/run_<uuid>`.
8. Requires `~/.reviewer.md` and prepends it to every model prompt as shared reviewer guidance.
9. Streams live progress to stderr for major phases and per-agent start/finish status.

The harness assumes the selected CLI is already authenticated.

## Requirements

- `git`
- `gh`
- `codex` or `claude`
- access to the target GitHub repo and PR
- `~/.reviewer.md` with repo-specific build/test/review guidance

## Usage

```bash
cargo run -- \
  --provider codex \
  --pr https://github.com/pytorch/pytorch/pull/180747 \
  --output-markdown /tmp/pr-123-review.md \
  --output-json /tmp/pr-123-review.json
```

Release binaries are published from Git tags. For CentOS or other Linux hosts, use the static `x86_64-unknown-linux-musl` artifact from the GitHub release page.

Claude works the same way:

```bash
cargo run -- \
  --provider claude \
  --pr https://github.com/pytorch/pytorch/pull/180747
```

You can also pass through provider-specific flags when the local CLI needs them:

```bash
cargo run -- \
  --provider claude \
  --extra-args "--dangerously-enable-internet-mode --dangerously-skip-permissions" \
  --pr https://github.com/pytorch/pytorch/pull/180747
```

`--pr` accepts either a plain PR number like `180747` or a full GitHub PR URL. If you are not already inside the target repo checkout and you pass a full URL, reviewer will clone the repo under `/tmp/reviewer-repos/...` automatically and run from there.

Optional controls:

- `--repo owner/name` to skip repo autodetection or pair a numeric `--pr` with a repo when you are not in the target checkout
- `--model <name>` to pass a provider-specific model name through to the CLI
- `--extra-args "<shell-style flags>"` to pass provider-specific flags straight through to `codex` or `claude`
- `--parallelism <n>`
- `--agent-timeout-secs <n>`
- `--check-timeout-secs <n>` to control the timeout for each sequential shell check
- `--keep-worktree`

## Releases

- Pushes and pull requests run `cargo test`.
- Tags matching `v*` build and publish a release artifact.
- `v0.0.1` and later publish `reviewer-x86_64-unknown-linux-musl.tar.gz`, which is intended to run cleanly on CentOS without depending on the host glibc version.

## Notes

- Progress is printed to stderr so it stays visible while the final Markdown report is still clean on stdout.
- `~/.reviewer.md` still matters everywhere: the build agent, every file reviewer, the checks planner, and the final review writer all see that shared guidance.
- The build phase is executed by the selected provider inside the PR worktree. It uses `~/.reviewer.md` as the primary source of truth for build/setup instructions and reports the commands it actually ran in the final Markdown.
- Provider subprocess failures bubble up directly. If `claude` or `codex` is logged in but not actually usable for the org/account, the run will fail with the CLI error text.
- Each provider invocation writes paired files such as `1776545033_initial-prompt_review-src-main-rs-<hash>_1.txt` and `1776545033_response_review-src-main-rs-<hash>_1.txt`. The CLI prints the run directory path at the end, including on failure.
- Those per-invocation artifacts include the exact captured provider subprocess streams as `subprocess_stdout` and `subprocess_stderr`, so you can inspect the real CLI output instead of model-authored excerpts.
- Sequential checks also write `check-command` and `check-result` artifacts under the same run directory.
- Those artifact files now include the exact provider argv as JSON, so extra flags like `--dangerously-skip-permissions` are visible after the fact.
- `~/.reviewer.md` is required. If it is missing, the CLI exits immediately and asks the user to create it before any run starts.
