# SPEC-merge-queue: Project-local merge queue

The merge queue is a local daemon associated with one canonical project root.
It accepts candidate runs over a Unix-domain socket, processes candidates
serially, delegates validation to
[SPEC-candidate-check](SPEC-candidate-check.md), and can integrate a passing
candidate into a configured base.

## Daemon and clients

The daemon can run in the foreground or fork into the background. An explicit
runtime directory can select it; otherwise clients discover project-matching
per-process runtime directories below the XDG runtime location or the
platform fallback. Runtime files associate the socket and PID with the
canonical project root.

`mq add` submits a landing-enabled run by default. `mq check` and
`mq add --no-merge` submit validation-only runs. The candidate defaults to
`HEAD` for Git or `@-` for Jujutsu and is resolved to immutable identities
before it is placed in the daemon queue. Client submission can autostart a
daemon when configuration supplies a base branch.

The daemon assigns local run IDs, processes one queued candidate at a time,
and exposes list, status, blocking wait, PID, version, runtime-directory, and
stop operations. Candidate checks can still execute multiple jobs
concurrently. The current implementation holds run state and history only in
memory; IDs restart and history disappears when the daemon restarts. Graceful
shutdown stops accepting requests, finishes work already sent to the queue
processor, and joins that processor so run-local resources are cleaned up
before process exit. The stop client force-kills a daemon that does not finish
within its bounded shutdown timeout.

## Run lifecycle

For each run, the daemon:

1. runs the optional `pre-clone` hook;
2. resolves the current configured base to an immutable revision;
3. prepares a disposable merge or rebase result without moving the base;
4. exports base and tested-candidate worktrees;
5. runs the optional `post-clone` hook and the shared candidate check;
6. finishes a validation-only run after a passing check;
7. otherwise runs `pre-merge`, performs final integration of the original
   candidate into the configured base, and runs `post-merge`.

The test integration and final integration are separate VCS operations. The
final operation rereads the movable base branch or bookmark without checking
that it still equals the commit used for the test integration and without an
expected-old compare-and-swap update. Base movement can therefore make the
landed result differ from the checked result, and a concurrent update can
conflict with or be overwritten by landing. This is a current implementation
limitation, not an established landing guarantee.

A failed preparation, check, or pre-merge hook does not intentionally move the
base. The user's checked-out Git branch or Jujutsu working-copy content is not
used as an integration worktree.

The public run states are queued, running, and terminal passed or failed
outcomes with a reason. Status includes available preparation, check, hook,
step, timing, and failure information. While the serving daemon remains
alive, `mq wait` blocks for terminal publication and reflects the terminal
outcome in its exit status. Graceful shutdown records terminal state for work
already sent to the queue processor, but does not join detached client handlers
or guarantee that waiters receive that state before process exit.

## Hooks

The six hook slots and their project/local precedence are defined by
[SPEC-configuration](SPEC-configuration.md). Their ordering and effects are:

- `pre-start` runs interactively before daemonization; failure aborts startup.
- `post-start` runs after daemonization and before request acceptance; its
  output is captured, and failure prevents the daemon from serving.
- `pre-clone` runs before base resolution and worktree preparation; failure
  fails the run.
- `post-clone` runs after worktree export and before jobs; failure fails the
  run.
- `pre-merge` runs only for a passing, landing-enabled run; failure prevents
  final integration and fails the run.
- `post-merge` runs only after successful final integration. Its failure is
  reported but cannot undo integration, so the run remains passed.

Hooks run with the original project root as their working directory.
Candidate-stage hooks receive the submitted candidate identities and base
branch. Hooks after test integration receive its tested identities, and
`post-clone` additionally receives writable temporary worktree paths,
including the base path from which the job command is loaded afterward.
Candidate-stage hook output is shown in run status but excluded from
integration commit descriptions. `pre-start` uses inherited interactive I/O;
captured `post-start` output goes to daemon stderr or its log.

Daemon startup snapshots merge-queue configuration as described by
[SPEC-configuration](SPEC-configuration.md). The daemon and flow are part of
[ARCH-selfci](ARCH-selfci.md).
