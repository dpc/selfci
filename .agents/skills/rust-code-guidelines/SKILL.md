---
name: rust-code-guidelines
description: Rust code guidelines for the selfci project. Use when writing or reviewing Rust code.
user-invocable: false
---

# Rust Code Guidelines

## No Polling in Non-Test Code

Non-test code must never use polling (sleep loops) to wait for state changes. Use reactive mechanisms instead:

- **Waiting for shared state**: use `Condvar` (condition variable) to be notified of changes
- **Waiting for process exit**: use `pidfd_open` + `poll` on Linux and Android,
  or `kqueue` with `EVFILT_PROC`/`NOTE_EXIT` on macOS
- **Waiting for I/O**: use blocking I/O or `poll`/`epoll` on file descriptors
- **Waiting for socket events**: use blocking `accept`/`read`, not sleep-retry loops

Test code may use polling (`thread::sleep` in retry loops) where the overhead is acceptable.
