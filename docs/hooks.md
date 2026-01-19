# Hooks

Hooks are commands that run at specific points in the merge queue (MQ) lifecycle. They allow customization of the CI/merge process without modifying job commands.

## Configuration

Hooks are configured under the `mq:` section in either:

- `.config/selfci/ci.yaml` - checked into source control
- `.config/selfci/local.yaml` - local overrides, not checked in

Local hooks override hooks from ci.yaml.

```yaml
mq:
  pre-start:
    command: 'echo "About to start mq daemon"'

  post-start:
    command: 'notify-send "MQ daemon started"'

  pre-clone:
    command: 'echo "About to clone $SELFCI_CANDIDATE_ID"'

  post-clone:
    command: 'ls $SELFCI_CANDIDATE_DIR'

  pre-merge:
    command: 'echo "About to merge"'

  post-merge:
    command: 'notify-send "Merged into $SELFCI_MQ_BASE_BRANCH"'
```

Each hook supports `command` (required) and `command-prefix` (optional, defaults to `["bash", "-c"]`).

## Hook Types

### pre-start

Runs **before** daemonization when starting the MQ daemon. Unlike
other hooks the I/O is interactive, so can be used to enter passwords, etc. e.g. for interactive
`git pull`.

Failure will prevent mq daemon from starting.

### post-start

Runs **after** daemonization when starting the MQ daemon.

Failure will stop the mq daemon.

### pre-clone

Runs before worktrees are created for each candidate.
Does not have access to `SELFCI_BASE_DIR` or `SELFCI_CANDIDATE_DIR` (worktrees don't exist yet).
Can be used `git fetch`-like commands (if non-interactive) to avoid stale local branch state.

Failure will fail the candidate check.

### post-clone

Runs after worktrees are created, before the job command.

Failure will fail the candidate check.

### pre-merge

Runs after check passes, before merging the candidate.

Failure will fail the candidate check and prevent merge.

### post-merge

Runs after a successful merge. Useful for automatically
pushing updates to remotes (if non-interactive).

Failure is logged but job status remains unchanged (already merged).

## Execution Context

Unlike jobs, hooks run in the repository's root directory (the original repo, not a worktree). This is where `.config/selfci/` is located.

## Environment Variables

### Startup Hooks (pre-start, post-start)

No candidate-specific environment variables are set since these run once at daemon startup.

### Candidate Hooks (pre-clone, post-clone, pre-merge, post-merge)

| Variable | Description |
|----------|-------------|
| `SELFCI_CANDIDATE_COMMIT_ID` | Git/jj commit hash of the candidate |
| `SELFCI_CANDIDATE_CHANGE_ID` | Jujutsu change ID (same as commit ID for git) |
| `SELFCI_CANDIDATE_ID` | User-provided revision string |
| `SELFCI_MQ_BASE_BRANCH` | Base branch for the merge queue (e.g., "main") |

### post-clone Only

| Variable | Description |
|----------|-------------|
| `SELFCI_BASE_DIR` | Path to the base worktree |
| `SELFCI_CANDIDATE_DIR` | Path to the candidate worktree |

## Hook Output

Hook output is captured (except pre-start) and included in the job status output under dedicated sections:

- `### Pre-Clone Hook`
- `### Post-Clone Hook`
- `### Pre-Merge Hook`
- `### Post-Merge Hook`

Hook output is **not** included in merge commit messages, as typically it will
be configured as local setting.
