# Environment Variables

selfci sets `SELFCI_VERSION` as a process-level environment variable at startup. This means **every external command** that selfci executes — including `git`, `jj`, job commands, and hooks — inherits it automatically.

The value matches the output of `selfci --version` (e.g., `0.4.0 (abc1234)`).

## Global Variables

| Variable | Description |
|----------|-------------|
| `SELFCI_VERSION` | Version of the running selfci instance (set for all child processes) |

## Configuration Variables

These are read by selfci from the environment at startup:

| Variable | Description |
|----------|-------------|
| `SELFCI_LOG` | Logging level configuration (e.g., `debug`, `info`, `warn`, `error`) |
| `SELFCI_LOG_FULL` | When set, enables full logging format with timestamps and targets |
| `SELFCI_VCS_FORCE` | Force a specific VCS (`git` or `jujutsu`) |
| `SELFCI_ROOT_DIR` | Root working directory for CI checks |
| `SELFCI_MQ_RUNTIME_DIR` | Explicit runtime directory for MQ daemon |

## Job Variables

Set when running job commands (see [jobs.md](jobs.md) for details):

| Variable | Description |
|----------|-------------|
| `SELFCI_JOB_NAME` | Name of the current job |
| `SELFCI_JOB_SOCK_PATH` | Path to the job control socket |
| `SELFCI_BASE_DIR` | Path to the base worktree |
| `SELFCI_CANDIDATE_DIR` | Path to the candidate worktree |
| `SELFCI_CANDIDATE_COMMIT_ID` | Git/jj commit hash of the original candidate |
| `SELFCI_CANDIDATE_CHANGE_ID` | Jujutsu change ID of the original candidate |
| `SELFCI_CANDIDATE_ID` | User-provided revision string |
| `SELFCI_MERGED_COMMIT_ID` | Commit hash after test merge/rebase onto base |
| `SELFCI_MERGED_CHANGE_ID` | Change ID after test merge/rebase |
| `SELFCI_TESTED_COMMIT_ID` | Exact prepared commit tested by CI |
| `SELFCI_TESTED_CHANGE_ID` | Exact prepared change tested by CI |

## Hook Variables

Set when running hooks (see [hooks.md](hooks.md) for details):

| Variable | Description |
|----------|-------------|
| `SELFCI_CANDIDATE_COMMIT_ID` | Git/jj commit hash of the original candidate |
| `SELFCI_CANDIDATE_CHANGE_ID` | Jujutsu change ID of the original candidate |
| `SELFCI_CANDIDATE_ID` | User-provided revision string |
| `SELFCI_MQ_BASE_BRANCH` | Base branch for the merge queue |
| `SELFCI_MERGED_COMMIT_ID` | Commit hash after test merge/rebase |
| `SELFCI_MERGED_CHANGE_ID` | Change ID after test merge/rebase |
| `SELFCI_TESTED_COMMIT_ID` | Exact prepared commit tested by CI |
| `SELFCI_TESTED_CHANGE_ID` | Exact prepared change tested by CI |
| `SELFCI_LANDED_COMMIT_ID` | Verified commit published to the base (post-merge only) |
| `SELFCI_LANDED_CHANGE_ID` | Verified change published to the base (post-merge only) |
| `SELFCI_BASE_DIR` | Path to the base worktree (post-clone only) |
| `SELFCI_CANDIDATE_DIR` | Path to the candidate worktree (post-clone only) |
