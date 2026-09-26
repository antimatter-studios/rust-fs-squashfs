#!/usr/bin/env bash
#
# test-targets.sh unit|images|oracle|kernel|all — print the `cargo test`
# target arguments for one tier of the suite.
#
#   unit    the library, the binaries, and every tests/*.rs that reaches
#           neither a fixture nor the VM: no tool, no kernel, no harness
#   images  the tests that read a committed fixture but need no VM —
#           everything a host without KVM (GitHub's arm64 runners) can
#           still run
#   oracle  every tests/*.rs that runs a squashfs-tools tool (in the VM)
#   kernel  every tests/*.rs that mounts one of our images with the real
#           kernel (in the VM)
#   all     every target the four tiers above cover, in one selection:
#           what `chore test:native` runs once as the whole suite, and
#           what the in-guest suite runs. NOT a bare `cargo test`, so a
#           tier deliberately left out of `all` stays out of both.
#
# DERIVED FROM THE TESTS THEMSELVES, not from a list someone has to keep
# in step. A test names a fixture through `test-disks` or one of
# `common`'s accessors, reaches a tool only through
# `fs_squashfs_test_support::oracle` / `assert_unsquashfs_walks` /
# `mksquashfs_from_guest_tree` — or `common`'s wrappers over them — and
# the kernel only through `guest_kernel_*`. Those helpers fail, never
# skip, when the VM or the tool is missing, and tests/test_contract.rs
# fails the suite if a test reaches either any other way. So the
# classification cannot drift without something going red.
#
# WHY THE `common` WRAPPERS ARE LISTED TOO. Unlike the sibling drivers,
# this suite's tests do not call the oracle helpers directly: they call
# `common::build_with_mksquashfs`, `build_with_mksquashfs_args` and
# `unsquashfs_extract_file`, which are thin wrappers. Matching only the
# support crate's names would classify every one of those files as a unit
# test — which would run it on the no-VM job, where it would fail, and
# that is the failure this file exists to make impossible.
#
# THE TIERS ARE DISJOINT, so the tasks together run each file once. A
# file that reaches more than one oracle is placed at the most specific:
# the kernel first, then the tools. A kernel test that also builds its
# image with `mksquashfs` has the kernel's verdict.
#
# Library tests that need a fixture or the VM would live in modules named
# `needs_host`; `unit` excludes them with cargo's name filter. There are
# none today — src/ is pure parsing — and the filter is what keeps that
# true by construction rather than by inspection.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

# A tool, reached directly or through one of common's wrappers.
TOOL='oracle\(|assert_unsquashfs_walks\(|mksquashfs_from_guest_tree\(|build_with_mksquashfs|unsquashfs_extract_file\('
# The real kernel, which is only ever reached in the guest.
KERNEL='guest_kernel_'
# A committed fixture, by path or through common's accessors.
FIXTURE='test-disks|test_disks_dir\(|basic_fixture_path\(|fixture_bytes\(|basic_big_bin\('
# Anything that leaves this process at all.
HOST="$FIXTURE|$TOOL|$KERNEL"
# Anything that needs the VM.
VM="$TOOL|$KERNEL"

tier="${1:-}"
args=()
for f in "$REPO"/tests/*.rs; do
    name="$(basename "$f" .rs)"
    case "$tier" in
        all) args+=(--test "$name") ;;
        unit) grep -qE "$HOST" "$f" || args+=(--test "$name") ;;
        images)
            if grep -qE "$FIXTURE" "$f" && ! grep -qE "$VM" "$f"; then
                args+=(--test "$name")
            fi
            ;;
        oracle)
            if grep -qE "$TOOL" "$f" && ! grep -qE "$KERNEL" "$f"; then
                args+=(--test "$name")
            fi
            ;;
        kernel) grep -qE "$KERNEL" "$f" && args+=(--test "$name") ;;
        *) echo "usage: test-targets.sh unit|images|oracle|kernel|all" >&2; exit 2 ;;
    esac
done

# A SELECTION THAT CAME OUT EMPTY IS A MISTAKE, NOT A TIER WITH NOTHING
# IN IT. `cargo test` with no --test argument runs EVERY target, so an
# empty selection does not run nothing — it runs everything, in a tier
# whose budget and floor were measured for a handful of files. That is the
# opposite of quiet and the opposite of correct.
if [ "${#args[@]}" -eq 0 ] && [ "$tier" != unit ]; then
    echo "test-targets.sh: the '$tier' selection matched no test file." >&2
    echo "                 A renamed helper is the usual cause: this script greps" >&2
    echo "                 tests/*.rs for the ways in that tests/test_contract.rs" >&2
    echo "                 allows, so renaming one without updating both leaves a" >&2
    echo "                 tier that would run the whole suite instead." >&2
    exit 1
fi

case "$tier" in
    unit) printf '%s\n' --lib --bins "${args[@]}" -- --skip needs_host:: ;;
    all) printf '%s\n' --lib --bins "${args[@]}" ;;
    *) printf '%s\n' "${args[@]}" ;;
esac
