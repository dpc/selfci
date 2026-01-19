# Jobs

Jobs are the CI commands that selfci executes to validate candidate commits.

## Configuration

Jobs are configured in `.config/selfci/ci.yaml`:

```yaml
job:
  command: |
    set -eou pipefail
    cargo test
    cargo clippy
```

The same command runs for all jobs. Use `$SELFCI_JOB_NAME` to differentiate behavior.

## Execution

Jobs run in a temporary clone of the candidate commit. The config is read from the **base** commit (not the candidate) to prevent bypassing CI by modifying the config.

Two worktrees are created:

- **Base worktree**: Contains the base commit (e.g., `main` branch)
- **Candidate worktree**: Contains the candidate commit being tested

The job command runs in the candidate worktree directory.

## Environment Variables

The following environment variables are available to job commands:

| Variable | Description |
|----------|-------------|
| `SELFCI_JOB_NAME` | Name of the current job (e.g., "main") |
| `SELFCI_JOB_SOCK_PATH` | Path to the job control socket for step logging |
| `SELFCI_BASE_DIR` | Path to the base worktree |
| `SELFCI_CANDIDATE_DIR` | Path to the candidate worktree |
| `SELFCI_CANDIDATE_COMMIT_ID` | Git/jj commit hash of the candidate |
| `SELFCI_CANDIDATE_CHANGE_ID` | Jujutsu change ID (same as commit ID for git) |
| `SELFCI_CANDIDATE_ID` | User-provided revision string (e.g., "HEAD", branch name) |

## Steps

Jobs can report progress using steps:

```bash
selfci step start "build"
cargo build

selfci step start "test"
cargo test
```

Steps can be marked as failed:

```bash
selfci step start "lint"
if ! cargo clippy; then
  selfci step fail          # Fails the job
  # or
  selfci step fail --ignore # Logs failure but will not fail the job as a whole
fi
```

Failing a step does not stop the job. To stop a job, finish execution.

## Sub-jobs

Jobs can spawn sub-jobs that run in parallel:

```bash
selfci job start "lint"
selfci job start "test"
```

Sub-jobs share the same environment variables and worktrees as the parent job.

## Exit Codes

- Exit code 0: Job passed
- Any non-zero exit code: Job failed
- Step failures (without `--ignore`) also fail the job
