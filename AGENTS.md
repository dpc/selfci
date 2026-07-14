# SelfCI agent notes

This project uses the Linked Specs convention; consult the `linked-specs`
skill before working with specs or governed code.

## Repository map

- `src/main.rs` wires the CLI entry point.
- `src/lib.rs` exposes the library surface.
- `src/cmd/` contains command implementations like `check`, `mq`, and worker-related logic.
- `src/config.rs`, `src/protocol.rs`, `src/mq_protocol.rs`, and `src/revision.rs` hold core project types and behavior.
- `README.md` explains the product and user-facing workflow.
- `.config/selfci/` contains this project's own SelfCI configuration.

## Validation

Prefer repository-native checks:

- `just check`
- `just test`
- `just final-check`
- `selfci check` (final verification)
