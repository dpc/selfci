# DESIGN-base-selected-job-command: Base-selected job command

Status: inferred

For each candidate check, SelfCI loads `job.command` and
`job.command-prefix` from a writable temporary worktree exported at the
resolved base identity. It then executes that entry command with the tested
candidate worktree as its current directory.

With an independently selected trusted base, an inline check prevents the
candidate revision from replacing the YAML entry command that governs the
check. The default inline base is the candidate itself, so callers seeking
that protection must select a distinct trusted base.

The merge queue runs its live-root `post-clone` hook after exporting the base
but before loading this configuration. That hook receives the writable base
path, but SelfCI attests the exported tree before loading the command. A
persistent hook change to the base export fails the check rather than changing
the command that governs it.

Even without such mutation, loading the entry command from the base does not
make all executed check logic base-controlled. The selected command can
deliberately invoke relative scripts or build logic from the candidate
worktree. A command that needs base-owned files can use `SELFCI_BASE_DIR`.

## Rationale and tradeoffs

Loading the entry command from the exported base gives maintainers a boundary
for initial check policy when the base and any pre-check hooks are trusted,
while retaining SelfCI's script-oriented composition. Running in the
candidate worktree makes ordinary build and test commands convenient and
allows a base policy to delegate to candidate code.

That delegation narrows the security guarantee. The design is not a sandbox,
and projects that invoke candidate-controlled scripts accept that those
scripts can affect the host with the user's privileges. Stronger containment
must be supplied by the configured command, as described by
[DESIGN-user-supplied-isolation](DESIGN-user-supplied-isolation.md).

Not every operational setting is selected from the base export. Clone mode
and the integration mode for an inline check are read from the invoking
checkout. Merge-queue base selection, integration mode, and hooks are
live-root startup inputs described by
[SPEC-configuration](SPEC-configuration.md); the base may be supplied by the
startup CLI rather than checkout configuration. These inputs are outside the
base-selected entry-command guarantee.

This decision constrains [SPEC-configuration](SPEC-configuration.md) and
[SPEC-candidate-check](SPEC-candidate-check.md), and is summarized by
[ARCH-selfci](ARCH-selfci.md).
