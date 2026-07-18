# DECISION-toctou-integrity: Reject time-of-check/time-of-use gaps

Authority: confirmed, 2026-07-18, dpc

SelfCI must not rely on a checked value and later use a mutable replacement
without proving that it is the same value. Publication and other
integrity-sensitive transitions use immutable identities and atomic
expected-old updates. Any unavoidable exception must be explicitly documented
and justified where it is governed.

This rule favors a safe failure, or a fresh check against newly resolved input,
over publishing or acting on content that did not pass the relevant check.
Process-local serialization is not a substitute for compare-and-swap because
repository and operating-system state can be changed by other processes.

## Rationale

SelfCI executes user commands and repository operations over meaningful
intervals. Names such as branches, bookmarks, paths, and process IDs can change
between separate observations. Silent replacement can turn a passing result
into approval for different content or can overwrite another writer.
Immutable prepared artifacts and expected-old transitions make the integrity
condition executable rather than timing-dependent.

## Explicit exceptions

SelfCI does not sandbox project-supplied hooks and jobs. It attests exported
source trees after `post-clone` and after jobs, but a trusted command can
deliberately mutate, observe, and restore a file within its own execution.
Preventing that self-deception requires the user-supplied isolation deliberately
excluded by [DESIGN-user-supplied-isolation](DESIGN-user-supplied-isolation.md);
it does not let SelfCI publish a different prepared commit.

Project-ignored files are treated as runtime inputs and outputs rather than
publishable source and are excluded from tree attestation. This is necessary
for ordinary build artifacts and caches; projects whose result depends on an
ignored input are responsible for creating or validating that input in their
check command.

Temporary Jujutsu export bookmarks use the reserved
`selfci-export-{base,candidate}-<pid>-<nonce>` namespace and cleanup deletes
those names after use or after proving their recorded process is absent. The
delete operation has no expected-target form, so another writer deliberately
retargeting a live reserved nonce can race cleanup. The namespace and
per-run name make that collision outside the supported repository interface.

Stale merge-queue runtime cleanup revalidates recorded daemon identity before
removing its private runtime path, but filesystem replacement can race the
subsequent pathname removal. Runtime directories are same-user mode-0700 and
their contents are an internal ownership namespace; safe coordination by other
processes uses the verified daemon socket rather than replacing those paths.

On macOS, the bounded forced-stop fallback retains the raw-PID reuse race
documented and justified in [SPEC-merge-queue](SPEC-merge-queue.md). Normal
shutdown uses the verified socket and an armed process watcher instead.

The architecture applying this decision is [ARCH-selfci](ARCH-selfci.md).
