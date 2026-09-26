#!/usr/bin/env bash
#
# guest-suite.sh [cargo test args...] — THE WHOLE SUITE, INSIDE THE VM.
#
# The fs-linux-test-harness [test] guest_command: `chore test:vm` (and
# `chore test` on a host that is not Linux) boots the VM and runs this
# from /repo, where the harness mounts this repository.
#
# WHY IT EXISTS. We run the Linux tests on Linux. On a Linux host that is
# the host itself and this is not used. On a Mac there is no SquashFS
# module, no loop mount and no squashfs-tools worth trusting — so the
# suite runs in here, against the same sources, with the same pinned
# toolchain.
#
# The toolchain and the build directory live on the VM's own disk
# (/var/lib, /var/cache), which outlives `vm:down` and is thrown away by
# `vm:destroy`: the first run pays a full build, later runs are
# incremental. The repository itself is a 9p mount, so nothing is written
# back into it except what the tests write to tmp/.
set -euo pipefail

[ "${FLTH_GUEST:-}" = 1 ] ||
    { echo "guest-suite.sh runs INSIDE the harness VM ('chore test:vm')." >&2; exit 1; }

# The path dependencies in Cargo.toml, by directory name.
SIBLINGS="rust-fs-core"

RUST_ROOT=/var/lib/fs-squashfs-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"
export CARGO_TARGET_DIR=/var/cache/fs-squashfs-target
export PATH="$CARGO_HOME/bin:$PATH"

# THE SIBLING CRATES. This crate's Cargo.toml has a path dependency on
# ../rust-fs-core, which on the host is a sibling checkout — and the guest
# is given this repository, not the directory that holds it. The harness
# mounts us at /repo, whose parent IS the guest's root, so
# `../rust-fs-core` resolves to /rust-fs-core: the task stages the sibling
# on the share (from its pinned, clean checkout) and this links it into
# place. `chore siblings` is what keeps it at the right ref.
# shellcheck disable=SC2043  # one sibling today; the list is the point
for sibling in $SIBLINGS; do
    staged="/share/siblings/$sibling"
    [ -d "$staged" ] || {
        echo "guest-suite.sh: $sibling is not staged on the share; 'chore test:vm' does that." >&2
        exit 1
    }
    [ -L "/$sibling" ] || ln -sfn "$staged" "/$sibling"
done

cd /repo
command -v cargo >/dev/null ||
    { echo "guest-suite.sh: no cargo in the guest — 'chore vm:provision' installs it." >&2; exit 1; }

# SCRATCH GOES ON THE GUEST'S OWN DISK, NOT THE 9p MOUNT.
#
# /repo is a virtio-9p share, and it is the one filesystem the oracle
# tools must not work on: mmap over 9p does not support everything a tool
# may ask of it, and the failure is a SIGSEGV rather than a refusal — an
# exit status of 139 with nothing to report. Measured in the sibling
# rust-fs-erofs, whose mkfs died exactly that way on CI's x86_64 guest
# while passing on an aarch64 host, so it is not a difference a developer
# would find locally.
#
# In this direction there is no host to be invisible to: the guest has its
# own copy of everything a tool needs, so /var/tmp is simply better.
# /var/tmp rather than /tmp because a tmpfs /tmp is sized from the guest's
# RAM and the images are not all small.
export FS_SQUASHFS_TEST_TMPDIR=/var/tmp/fs-squashfs-tests
mkdir -p "$FS_SQUASHFS_TEST_TMPDIR"

# THE SAME SELECTION THE NATIVE PATH USES, which means `all` and not
# "every test there is" — the guest is a way of running the Linux suite
# somewhere else, not a different suite. A bare `cargo test` here would
# also pick up any tier deliberately left out of `all`.
echo "== in-guest suite: $(uname -srm), $(cargo --version)"
started=$(date +%s)
# shellcheck disable=SC2046  # the words are the target list
scripts/test.sh --locked --release $(scripts/test-targets.sh all) "$@"
echo "== in-guest suite: $(( $(date +%s) - started ))s"
