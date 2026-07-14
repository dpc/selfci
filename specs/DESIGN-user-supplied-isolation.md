# DESIGN-user-supplied-isolation: User-supplied execution isolation

Status: inferred

SelfCI does not embed or require a container, virtual machine, sandbox, or
remote executor. It runs project commands and merge-queue hooks directly on
the local host. Projects select any stronger isolation mechanism as part of
their configured commands and environment.

SelfCI does provide disposable base and candidate worktrees and a bounded
worker pool. Those mechanisms protect the user's working copy from normal
check preparation and bound job concurrency; they are not a host security
boundary. All jobs in a check use the same tested-candidate worktree, so they
also receive no per-job filesystem isolation from SelfCI.

## Rationale and consequences

Leaving containment to the project keeps the runner local and composable with
tools such as Nix, containers, or other sandboxing systems without prescribing
one of them. It also avoids making a weak built-in sandbox appear to be a
complete trust boundary.

CI authors are responsible for protecting the host from candidate and hook
code and for preventing concurrently running jobs from interfering through
their shared worktree. This applies even when the entry command is selected
from the base as described by
[DESIGN-base-selected-job-command](DESIGN-base-selected-job-command.md).

This decision constrains job execution in
[SPEC-candidate-check](SPEC-candidate-check.md) and is summarized by
[ARCH-selfci](ARCH-selfci.md).
