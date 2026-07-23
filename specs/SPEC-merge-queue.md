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
within 30 seconds and requires the forced process exit to be observed within a
further 5 seconds. Stop succeeds only after the daemon is known to be absent or
its exit has been observed. Process-observer and signal failures are reported
and do not justify deleting possibly live runtime state. Normal graceful
shutdown leaves runtime cleanup to the daemon; stale and forced-shutdown cleanup
revalidates the recorded PID before removing state.

The normal stop path uses the already-verified Unix socket to request shutdown,
and the responder must acknowledge the PID recorded in the runtime directory.
The exit watcher is armed before that request. Only the timeout fallback sends
`SIGKILL`; on macOS this necessarily retains a narrow raw-PID reuse race between
the final watcher check and signal delivery. PID equality narrows races but is
not a general-purpose instance identity.

## Run lifecycle

For each run, the daemon:

1. runs the optional `pre-clone` hook;
2. resolves the current configured base to an immutable revision;
3. prepares a disposable merge or rebase result without moving the base;
4. exports base and tested-candidate worktrees;
5. runs the optional `post-clone` hook and the shared candidate check;
6. finishes a validation-only run after a passing check;
7. otherwise runs `pre-merge`, atomically publishes the exact checked
   integration if the base still names the resolved commit, and runs
   `post-merge`.

Publication never reconstructs integration from a movable name. Git updates
the base ref with its resolved commit as the expected old object. Jujutsu moves
the named bookmark only from the intersection of that name and resolved commit.
If the expected target no longer matches, the run fails and preserves the
external update; a later queue run resolves, prepares, and checks the newer
base. A submitted Jujutsu commit that is a strict ancestor of the resolved base
tests that exact base and publication performs an expected-old no-op check; if
the base moves before that check, the no-op is classified as not applied. The
actual landed identity is verified before publication hooks run. If
the expected-old movement applies but this verification cannot establish one
unique target, the distinct `publication-unverified` failure reports that
publication may already have applied, suppresses `post-merge`, and requires
repository inspection rather than an automatic retry.
This implements
[DECISION-toctou-integrity](DECISION-toctou-integrity.md).

Jujutsu rebase preparation computes the submitted suffix from exact commit
reachability and performs duplication and rebase in chained isolated
operations. If the checked base contains a different commit revision of a
logical change in the submitted ancestry, preparation fails rather than
silently substituting that mutable change-ID revision.

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
  publication and fails the run.
- `post-merge` runs only after successful publication. Its failure is reported
  but cannot undo publication, so the run remains passed.

Hooks run with the original project root as their working directory.
Their environment identifies the run as a merge-queue operation.
Candidate-stage hooks receive the submitted candidate identities and base
branch. Hooks after test integration receive its tested identities, and
`post-clone` additionally receives writable temporary worktree paths,
including the base path from which the job command is loaded afterward.
Candidate-stage hook output is shown in run status but excluded from
integration commit descriptions. `pre-start` uses inherited interactive I/O;
captured `post-start` output goes to daemon stderr or its log.
`post-merge` additionally receives the verified landed commit and change
identities. The legacy `SELFCI_MERGED_*` variables remain aliases for the
tested identities.

Daemon startup snapshots merge-queue configuration as described by
[SPEC-configuration](SPEC-configuration.md). The daemon and flow are part of
[ARCH-selfci](ARCH-selfci.md).
