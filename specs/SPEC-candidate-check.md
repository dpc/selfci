# SPEC-candidate-check: Candidate validation

A candidate check resolves revisions, prepares the tree to be tested, and
runs the project's base-selected entry command. The command may fan out into
named jobs and report steps through SelfCI's local control plane.

## Candidate preparation

The caller supplies a project root and may supply base and candidate revision
expressions. An inline check defaults its candidate to `HEAD` for Git or `@`
for Jujutsu and defaults its base to that candidate. The worker count defaults
to the host's available parallelism.

SelfCI resolves both expressions to immutable commit identities before
exporting content. It also retains the original user expression and a change
identity; Git uses the commit identity as its change identity, while Jujutsu
has a distinct change identity.

When base and candidate commit identities differ, an inline check constructs a
disposable merge or rebase result using the configured mode and tests that
integrated result. An inline check tests the candidate directly when the
identities are equal. A queued check always invokes configured test
integration and tests the result returned by that operation. Preparation must
not move the base reference or alter the user's working copy.

The check exports writable temporary worktrees at the resolved base and
tested-candidate identities using the configured full, partial, or shallow
clone mode. Temporary worktrees and export references are cleaned up.
SelfCI-owned synthetic Jujutsu preparations remain as anonymous visible heads
in repository storage unless a merge-queue landing publishes one. SelfCI does
not eagerly abandon them because even abandoning an exact commit can rebase a
concurrent descendant or delete a bookmark; preserving external work takes
precedence over eager cleanup. Jujutsu does not currently garbage-collect these
visible heads, so repeated checks accumulate them and repository owners may
need to inspect and abandon obsolete SelfCI preparations manually when no
concurrent work depends on them. Unreferenced synthetic Git commits remain
subject to normal Git garbage collection.

After `post-clone` and again after all jobs, SelfCI stages the exported Git
worktrees and requires their resulting trees to equal the prepared base and
candidate trees. Persistent tracked changes and non-ignored untracked files
fail the check even when commands exit successfully. Ignored files are excluded
so build outputs can use project-defined ignored paths. Clean filters define
the canonical Git tree comparison. The trusted-command mutate-and-restore
exception is documented in
[DECISION-toctou-integrity](DECISION-toctou-integrity.md).

## Job execution

The check loads the command and prefix from the exported base, following
[DESIGN-base-selected-job-command](DESIGN-base-selected-job-command.md), and
runs the command in the tested-candidate worktree. The initial job is named
`main`. Every dynamic job uses the same command and differs through
`SELFCI_JOB_NAME`; job names are unique within a check.

Workers bound concurrent command execution. Jobs share the tested-candidate
worktree and execute with host privileges; SelfCI does not provide the
isolation excluded by
[DESIGN-user-supplied-isolation](DESIGN-user-supplied-isolation.md).

Job scripts can request named jobs, wait for known jobs, and report step
progress over a per-check Unix-domain socket. Waiting normally reports a
job's result; `job wait --success` also fails when the waited-for job did not
succeed. Starting a step completes a previously running step successfully.
`step fail` marks the current step failed, and `--ignore` keeps that failure
visible without making it fatal.

The current control path acknowledges a new job after queueing it, but the
coordinator counts it only after a worker starts it. The parent can therefore
finish before an accepted queued child starts. A parent that waits while every
worker is occupied can deadlock, and a zero worker count is not rejected.
This baseline makes no stronger job-quiescence guarantee; the intended
semantics of those cases remain unresolved.

The environment distinguishes the originally submitted candidate from the
tested integration result. It exposes original candidate user/change/commit
identities, base and candidate worktree paths, job name, control socket, and
whether the check was invoked inline or by the merge queue.
When a separate integration result was prepared, it also exposes that tested
result's change and commit identities. `docs/environment.md` is the
user-facing variable reference.

## Result

A command that cannot be started, exits unsuccessfully, or contains a
non-ignored failed step makes that started job unsuccessful. The check
aggregates worker and step output and fails if a started job is unsuccessful.
Inline checks can stream output; merge-queue checks capture it for run status
and diagnostics.

Revision resolution, test integration, configuration loading, worktree
export, and worker-control failures also fail the check. Results and worktrees
are run-local; the check engine does not provide durable run history.

The merge queue in [SPEC-merge-queue](SPEC-merge-queue.md) invokes this same
check path after preparing its candidate.
