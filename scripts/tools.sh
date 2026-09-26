#!/usr/bin/env bash
#
# tools.sh — what the HOST needs (`chore tools`).
#
#   tools.sh           install what is missing, then verify
#   tools.sh --check   verify only; exit 1 naming what is missing
#
# THE ORACLE TOOLS ARE NOT HERE, AND THAT IS THE POINT. squashfs-tools —
# mksquashfs, unsquashfs, sqfstar — runs inside the fs-linux-test-harness
# VM, built from source at a pinned tag by scripts/vm-setup.sh, and
# nowhere else. A workstation therefore installs none of it: no 4.5.1 from
# Debian, which does not know `-xattrs-add` and fails one xattr test while
# CI passes, no Homebrew build on a Mac that cannot mount what it makes,
# and no chance of an oracle answering differently depending on who asked.
# tests/support/src/oracle.rs is the only way a test reaches one, and
# tests/test_contract.rs fails the suite if anything runs one on the host.
#
# What the host does need:
#
#   python3      scripts/tier.sh asks `cargo metadata` where rust-fs-core
#                is through it. A guard written around a missing
#                interpreter is a guard that never runs
#   PyYAML       scripts/ci-gate.sh PARSES ci.yml rather than scanning it:
#                a quoted key, a flow mapping and a `run: |` block whose
#                contents look like a job key are all ordinary YAML that a
#                line scan reads wrongly. The module is not a binary, so
#                `command -v` cannot see it and it is checked separately
#   the VM       Vagrant, QEMU and KVM/HVF — checked by the harness
#                itself (../fs-linux-test-harness/scripts/host-tools.sh,
#                also `chore vm:host:check`), which knows what it needs
#                on each platform
#
# `--check` LEAVES THE VM OUT. It is the early gate the test tasks run so
# that a missing interpreter fails once instead of in every tier, and it
# is also what the architecture-only CI job can run on a machine with no
# KVM at all. Whether the VM works is answered by booting it: the first
# oracle call does that, and says what to install when it cannot.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
HARNESS_TOOLS="$REPO/../fs-linux-test-harness/scripts/host-tools.sh"

MODE=install
[ "${1:-}" = "--check" ] && MODE=check

# tool:debian-package:homebrew-formula
#
# cc IS HERE BECAUSE tests/c_header_layout.rs COMPILES C. That test builds a
# file of `_Static_assert`s from the Rust layout and requires the C compiler
# to agree, which is the only thing that can catch include/fs_squashfs.h
# drifting from the ABI the library actually exposes. It used to print
# "no C compiler — skipping" and pass, with an assertion that only fired when
# CI was set — so on every developer machine without cc the header went
# unchecked and nothing said so.
TOOLS="python3:python3:python3 cc:gcc:gcc"

# import:debian-package:pip-name — a Python module, checked by importing it.
MODULES="yaml:python3-yaml:PyYAML"

missing() {
    local entry out=""
    for entry in $TOOLS; do
        command -v "${entry%%:*}" >/dev/null 2>&1 || out="$out $entry"
    done
    if command -v python3 >/dev/null 2>&1; then
        for entry in $MODULES; do
            python3 -c "import ${entry%%:*}" >/dev/null 2>&1 || out="$out $entry"
        done
    fi
    echo "${out# }"
}

names() {
    local entry
    for entry in "$@"; do printf '%s ' "${entry%%:*}"; done
}

packages() {
    local field="$1" entry
    shift
    for entry in "$@"; do echo "$entry" | cut -d: -f"$field"; done | sort -u | tr '\n' ' '
}

install_linux() {
    local sudo="" pkgs
    # shellcheck disable=SC2046  # one word per package
    pkgs="$(packages 2 $(missing))"
    [ "$(id -u)" -eq 0 ] || sudo="sudo"
    if ! command -v apt-get >/dev/null 2>&1; then
        echo "tools: no apt-get on this Linux host; install with its package manager: $pkgs" >&2
        exit 1
    fi
    echo "tools: installing with apt-get: $pkgs"
    # shellcheck disable=SC2086  # the package list splits into words
    $sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $pkgs >/dev/null ||
        { $sudo apt-get update -qq && $sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $pkgs >/dev/null; }
}

gap="$(missing)"
if [ -n "$gap" ] && [ "$MODE" = install ]; then
    case "$(uname -s)" in
        Linux) install_linux ;;
        Darwin)
            # shellcheck disable=SC2086
            echo "tools: missing on this Mac: $(names $gap)" >&2
            # shellcheck disable=SC2086
            echo "       brew install $(packages 3 $gap)" >&2
            exit 1
            ;;
        *) echo "tools: unsupported host $(uname -s)" >&2; exit 1 ;;
    esac
    gap="$(missing)"
fi

if [ -n "$gap" ]; then
    # shellcheck disable=SC2086
    echo "tools: missing: $(names $gap)— run 'chore tools'" >&2
    exit 1
fi

printf '  %-9s %s\n' python3 "$(command -v python3)"
printf '  %-9s %s\n' PyYAML "$(python3 -c 'import yaml; print(yaml.__version__)')"
# The compiler is REPORTED, not merely found: which one it is decides whether
# the header's layout assertions mean anything, and `cc` is a symlink whose
# target differs per distribution. One line, and its first line only.
printf '  %-9s %s\n' "${CC:-cc}" "$("${CC:-cc}" --version 2>&1 | sed -n 1p)"
echo "tools: the oracle tools live in the harness VM (scripts/vm-setup.sh), not here."

[ "$MODE" = check ] && exit 0

if [ ! -x "$HARNESS_TOOLS" ]; then
    echo "tools: the fs-linux-test-harness sibling is not checked out." >&2
    echo "       Run 'chore siblings'. The oracle tools, the fixtures and the" >&2
    echo "       kernel tests all run in its VM." >&2
    exit 1
fi
"$HARNESS_TOOLS"
