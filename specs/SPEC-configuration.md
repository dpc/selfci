# SPEC-configuration: Configuration and initialization

SelfCI configuration lives under `.config/selfci/` at the project root.
`ci.yaml` is the tracked project configuration. `local.yaml` is intended for
untracked, machine-local merge-queue hook replacements, and the generated
`.gitignore` ignores that file.

Configuration is strict: unknown fields are rejected rather than ignored.

## Project configuration

The `job` section requires a string `command`. Its `command-prefix` is a list
and defaults to `["bash", "-c"]`. `clone-mode` is `full`, `partial`, or
`shallow` and defaults to `partial`.

The optional `mq` section can set:

- `base-branch`;
- `merge-mode`, either `rebase` (the default) or `merge`; the legacy
  `merge-style` spelling is also accepted;
- `pre-start`, `post-start`, `pre-clone`, `post-clone`, `pre-merge`, and
  `post-merge` hooks, each using an optional command and command prefix. A
  configured hook without a command is a no-op.

`local.yaml` accepts only those six hook slots. A local hook, when present,
replaces the corresponding project hook as a whole. It cannot override the
job command, clone mode, base branch, or integration mode.

## Initialization

`selfci init` operates in the current directory, which must itself contain
`.git` or `.jj`; VCS detection does not search ancestor directories. It
creates `ci.yaml`, `local.yaml`, and `.config/selfci/.gitignore` from the
bundled templates. Existing files are preserved rather than overwritten.

## Configuration sources and timing

Configuration is read at several boundaries:

- an inline check reads clone mode and test-integration mode from the invoking
  checkout;
- after exporting the base, every candidate check reads the job command and
  prefix from that writable temporary worktree, as described by
  [DESIGN-base-selected-job-command](DESIGN-base-selected-job-command.md);
- merge-queue startup takes the base branch from `--base-branch` when
  supplied, otherwise from `mq.base-branch`;
- before daemonization, startup reads configuration to run `pre-start`; the
  daemon then rereads merged project/local configuration for `post-start` and
  retains its integration mode and candidate-stage hooks.

Consequently, changing merge-queue settings does not reconfigure an already
running daemon, while the base-selected job entry command is selected anew for
each check.

The parser currently accepts an empty command-prefix list even though command
execution requires a first element. This baseline does not establish empty
prefixes as supported behavior.

The YAML does not define a static job graph. The selected command may use the
runtime job-control commands described by
[SPEC-candidate-check](SPEC-candidate-check.md). Environment variables are
documented for users in `docs/environment.md`; this record governs their
configuration provenance rather than duplicating that reference.

This configuration drives [SPEC-candidate-check](SPEC-candidate-check.md) and
[SPEC-merge-queue](SPEC-merge-queue.md).
