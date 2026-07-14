# ARCH-selfci: SelfCI architecture

SelfCI is a local command-line continuous-integration tool and merge queue. It
operates on a project repository through Git or Git-backed Jujutsu, prepares
disposable worktrees, and runs project-supplied commands on the user's host.

## Components

- The CLI parses commands, selects a project root and VCS, and dispatches
  inline checks, job/step control requests, and merge-queue client or daemon
  operations.
- The configuration layer loads strict YAML configuration and combines tracked
  project settings with the limited local hook overrides described by
  [SPEC-configuration](SPEC-configuration.md).
- The revision and VCS layer resolves user expressions to stable change and
  commit identities, exports exact revisions into Git worktrees, and performs
  Git- or Jujutsu-specific test integration and final integration.
- The candidate-check coordinator prepares base and tested-candidate
  worktrees, starts workers, and aggregates command and step results as
  described by [SPEC-candidate-check](SPEC-candidate-check.md).
- Worker threads run named jobs. Job scripts use a per-check Unix-domain
  control socket through `selfci job` and `selfci step`; this is an internal
  local control plane rather than a remote service.
- The merge-queue client and project-local daemon communicate over another
  Unix-domain socket. The daemon serializes candidate runs, invokes the shared
  candidate-check coordinator, runs configured hooks, and optionally performs
  final integration as described by
  [SPEC-merge-queue](SPEC-merge-queue.md).

The merge queue depends on the candidate-check path rather than implementing a
second check runner. Both paths share configuration, revision, VCS, and
worktree-export code.

## Main flows

An inline check resolves its base and candidate, constructs a disposable
merge or rebase result when their commits differ, exports the base and tested
candidate, and executes the job command loaded from the exported base in the
tested candidate worktree. The boundary and its limits are recorded in
[DESIGN-base-selected-job-command](DESIGN-base-selected-job-command.md).

For a queued run, the daemon freezes the submitted candidate identity at
enqueue time and resolves the current base when processing begins. It prepares
and checks a disposable integration result. A landing-enabled run that passes
then performs a separate final integration of the original candidate into the
configured base. The final operation does not verify that the mutable base
still names the commit used to prepare the checked result. Candidates are
processed one at a time, while jobs within one check may run concurrently.

## Ownership and boundaries

Each candidate check owns its temporary worktrees, control socket, workers,
and result aggregation. The merge-queue daemon owns its runtime directory,
socket, in-memory run records, and queue. The current implementation does not
persist run history or run IDs across daemon restarts; this describes its
present storage boundary rather than establishing a durability policy.

User expressions are resolved before snapshot operations. Immutable commit
identities select exported content; Jujutsu change identities additionally
track revisions across rewriting operations. Test integration must not move
the configured base or modify the user's working copy.

SelfCI supplies disposable worktrees and bounded concurrency, but it does not
sandbox commands or isolate jobs from one another. Candidate commands and
merge-queue hooks execute with the invoking user's privileges, as recorded in
[DESIGN-user-supplied-isolation](DESIGN-user-supplied-isolation.md).

The runtime assumes a Unix process and filesystem environment, including
Unix-domain sockets and signals. Some lifecycle and cleanup paths use
Linux-specific facilities; this record does not establish a broader platform
support commitment.
