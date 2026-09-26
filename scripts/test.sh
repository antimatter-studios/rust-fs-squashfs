#!/usr/bin/env bash
# test.sh [cargo test args...]  run the suite in an owned scratch directory
# test.sh --print-temp-dir      print that directory and exit
#
# SCRATCH LIVES IN THE REPOSITORY, always: tmp/ (gitignored), and never
# the system temporary directory or a runner-supplied one. The oracle
# tools run inside the fs-linux-test-harness VM, which sees this
# repository at the path the host knows it by and nothing else of the
# host — so an image anywhere else is a path the tool asked to read it
# cannot open. The same rule is written in Rust in
# tests/support/src/lib.rs (select_temp_dir).
#
# FS_SQUASHFS_TEST_TMPDIR supplies an exact directory instead; it must be
# inside the repository, and it is the caller's to delete.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR=""

cleanup() {
    if [[ -n "$RUN_DIR" && -d "$RUN_DIR" ]]; then
        find "$RUN_DIR" -depth -mindepth 1 -delete
        rmdir "$RUN_DIR"
    fi
}
trap cleanup EXIT HUP INT TERM

# Kept in step with GUEST_SCRATCH in tests/support/src/lib.rs. /var/tmp
# rather than /tmp: a tmpfs /tmp is sized from the guest's RAM and the
# images are not all small.
GUEST_SCRATCH=/var/tmp/fs-squashfs-tests

if [[ -n "${FS_SQUASHFS_TEST_TMPDIR:-}" ]]; then
    if [[ "${FLTH_GUEST:-}" != 1 ]]; then
    case "$FS_SQUASHFS_TEST_TMPDIR" in
        "$REPO"/*) ;;
        *)
            echo "test.sh: FS_SQUASHFS_TEST_TMPDIR is $FS_SQUASHFS_TEST_TMPDIR, which is outside" >&2
            echo "         $REPO. The oracle tools run in the harness VM, which sees this" >&2
            echo "         repository and nothing else of the host." >&2
            exit 1
            ;;
    esac
    fi
    # An exact caller-supplied directory is not ours to delete.
    mkdir -p "$FS_SQUASHFS_TEST_TMPDIR"
elif [[ "${FLTH_GUEST:-}" == 1 ]]; then
    # /repo is the 9p mount, and it is the one filesystem the oracle tools
    # must not work on. In this direction there is no host to be invisible
    # to, so the guest's own disk is simply better.
    mkdir -p "$GUEST_SCRATCH"
    RUN_DIR="$(mktemp -d "$GUEST_SCRATCH/run.XXXXXX")"
    export FS_SQUASHFS_TEST_TMPDIR="$RUN_DIR"
else
    mkdir -p "$REPO/tmp"
    RUN_DIR="$(mktemp -d "$REPO/tmp/fs-squashfs-tests.XXXXXX")"
    export FS_SQUASHFS_TEST_TMPDIR="$RUN_DIR"
fi

export TMPDIR="$FS_SQUASHFS_TEST_TMPDIR"

if [[ "${1:-}" == "--print-temp-dir" ]]; then
    printf '%s\n' "$FS_SQUASHFS_TEST_TMPDIR"
    exit 0
fi

cargo test "$@"
