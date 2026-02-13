# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-02-12

### Added

- Install `.gitignore` in `.config/selfci/` to ignore `local.yaml`
- Better user feedback on `selfci init` (shows if config already exists)
- CHANGELOG.md

### Changed

- Rename `merge-style` to `merge-mode` in configuration
- Better formatting of `mq list` table output using comfy-table
- Improved `Active Steps` display in status output
- Cleaner headers in status log

### Fixed

- Don't ignore YAML parsing errors
- Runs always have a merge mode set

## [0.2.0] - 2026-01-22

### Added

- Hooks system with `pre-start`, `post-start`, `pre-clone`, `post-clone`, `pre-merge`, `post-merge`
- Local configuration file (`local.yaml`) for user-specific overrides not checked into VCS
- `SELFCI_VERSION` environment variable
- `SELFCI_MERGED_*` environment variables for merged commit info
- Test merge on `selfci check` command

### Changed

- Improved documentation for hooks and jobs
- Better initialization and daemon startup
- Nicer "Starting check" message
- Better merge commit messages
- Improved CI job to use treefmt

### Fixed

- Rebase candidate on top of base before check in merge queue
- Merge queue termination handling

## [0.1.0] - 2026-01-16

### Added

- Initial release of SelfCI
- Core `selfci check` command for running CI checks on commits
- `selfci init` command for initializing configuration
- Merge queue daemon (`selfci mq start/stop/add/list/status`)
- `selfci mq check` for one-off checks without merging
- Auto-start merge queue daemon
- Support for Git and Jujutsu version control systems
- Clone modes: full, partial (blobless), and shallow
- `mq.merge-style` configuration (rebase or merge)
- `mq.base-branch` configuration for default base branch
- Dynamic jobs with `selfci job start/wait`
- Step tracking with `selfci step start/fail`
- XDG runtime directory support for daemon files
- Daemonized merge queue with proper process management

<!-- TODO: Switch to Radicle links when it supports linking to tags -->
[Unreleased]: https://github.com/dpc/selfci/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/dpc/selfci/releases/tag/v0.3.0
[0.2.0]: https://github.com/dpc/selfci/releases/tag/v0.2.0
[0.1.0]: https://github.com/dpc/selfci/releases/tag/v0.1.0
