#!/usr/bin/env bash
set -eou pipefail

function check_leftover_dbg() {
  set -euo pipefail

  errors=""
  while IFS= read -r -d '' path; do
    if grep -q 'dbg!(' "$path"; then
      >&2 echo "$path contains dbg! macro"
      errors=1
    fi
  done < <(find . -name '*.rs' -print0)

  if [ -n "$errors" ]; then
    return 1
  fi
}

case "$SELFCI_JOB_NAME" in
  main)
    selfci job start "lint"
    selfci job start "cargo"
    ;;

  cargo)
    selfci step start "cargo check"
    cargo check

    selfci step start "cargo build"
    cargo build

    selfci step start "cargo nextest run"
    cargo nextest run
    ;;

  lint)
    selfci step start "check leftover dbg!"
    if ! check_leftover_dbg ; then
      selfci step fail
    fi

    selfci step start "cargo fmt"
    if ! cargo fmt --all --check ; then
      selfci step fail
    fi
    ;;

  *)
    echo "Unknown job: $SELFCI_JOB_NAME"
    exit 1
esac
